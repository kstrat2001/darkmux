//! review_bench (#1119) — reproducible PR-reviewer evaluation.
//!
//! Runs the `pr-reviewer` role over a labeled diff corpus (the
//! `pr-review-bench` fixture: `cases/<id>.diff` + `cases/<id>.label.json`)
//! and scores **precision** (false positives on correct diffs), **recall**
//! (planted bugs caught), **verdict accuracy**, and **anchor accuracy**
//! against the ground-truth labels — replacing the anecdotal "I watched it
//! over-flag." Run it across profiles/models and the rows ARE the bake-off
//! matrix, reproducibly (the model axis is `--profile` / `--profiles-file`).
//!
//! Dispatch goes through `darkmux_crew::dispatch` (internal runtime, `--json`),
//! same substrate as `darkmux crew dispatch pr-reviewer` — so a bench run and a
//! real CI review exercise the identical path.

use crate::providers::prompt::extract_reply_text;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Ground-truth label for one diff case (`<id>.label.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    /// "clean" (expect pass, 0 findings) | "bug" (expect flag, ≥1 finding).
    pub kind: String,
    #[serde(default)]
    pub intent_title: String,
    #[serde(default)]
    pub intent_body: String,
    #[serde(default)]
    pub expect_verdict: String,
    #[serde(default)]
    pub bug_class: Option<String>,
    /// Substring the correct finding's anchor should contain (bug cases).
    #[serde(default)]
    pub anchor_contains: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// A loaded case: the label + the unified diff under review.
#[derive(Debug)]
pub struct Case {
    pub id: String,
    pub label: Label,
    pub diff: String,
}

/// The model's review, reduced to what scoring needs. `parsed == false`
/// means the review was empty/degenerate (e.g. a #1050 thinking-family model
/// routing its answer to reasoning_content) — scored distinctly from a real
/// `pass` so a broken model can't read as "perfect precision."
#[derive(Debug, Default)]
pub struct Review {
    pub verdict: String,
    pub findings: Vec<Finding>,
    pub parsed: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Finding {
    pub severity: String,
    pub anchor: String,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct CaseScore {
    pub verdict: String,
    pub findings: usize,
    pub degenerate: bool,
    /// clean: number of (false-positive) findings.
    pub fp: usize,
    /// bug: caught (flag + anchor match or a high-severity finding).
    pub recall: bool,
    /// bug: a finding anchored to the expected line.
    pub anchor_ok: bool,
    /// flag verdict with an empty findings array — contract violation
    /// (the gpt-oss under-flag failure mode), distinct from a true pass.
    pub empty_flag: bool,
    /// matched the expectation for its kind.
    pub correct: bool,
}

pub struct ReviewBenchOpts {
    pub cases_dir: PathBuf,
    pub profile_name: Option<String>,
    pub config_path: Option<String>,
    pub timeout_seconds: u32,
}

pub fn run_review_bench(opts: ReviewBenchOpts) -> Result<()> {
    let cases = load_cases(&opts.cases_dir)?;
    if cases.is_empty() {
        return Err(anyhow!(
            "no cases (*.label.json + sibling *.diff) found in {}",
            opts.cases_dir.display()
        ));
    }
    eprintln!(
        "pr-review-bench: {} cases · profile={} · profiles-file={}",
        cases.len(),
        opts.profile_name.as_deref().unwrap_or("(default)"),
        opts.config_path.as_deref().unwrap_or("(registry)"),
    );
    println!("{:<34}{:<7}{:<7}{:<4}outcome", "case", "kind", "verd", "f");
    let mut scored: Vec<(&Case, CaseScore)> = Vec::new();
    for c in &cases {
        let prompt = build_prompt(c);
        let stdout = dispatch_case(&prompt, &c.id, &opts)
            .with_context(|| format!("dispatching case {}", c.id))?;
        let review = parse_review(&extract_reply_text(&stdout));
        let s = score(&c.label, &review);
        println!(
            "{:<34}{:<7}{:<7}{:<4}{}",
            c.id,
            c.label.kind,
            if s.degenerate { "—" } else { s.verdict.as_str() },
            s.findings,
            describe(&c.label, &s),
        );
        scored.push((c, s));
    }
    print_summary(&scored, &opts);
    Ok(())
}

/// Load every `<id>.label.json` + sibling `<id>.diff` in `dir`, id-sorted.
fn load_cases(dir: &Path) -> Result<Vec<Case>> {
    let mut label_paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading cases dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".label.json"))
                .unwrap_or(false)
        })
        .collect();
    label_paths.sort();
    let mut cases = Vec::new();
    for label_path in label_paths {
        let fname = label_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let id = fname.trim_end_matches(".label.json").to_string();
        let label: Label = serde_json::from_str(&fs::read_to_string(&label_path)?)
            .with_context(|| format!("parsing label {}", label_path.display()))?;
        let diff_path = dir.join(format!("{id}.diff"));
        let diff = fs::read_to_string(&diff_path)
            .with_context(|| format!("reading diff {}", diff_path.display()))?;
        cases.push(Case { id, label, diff });
    }
    Ok(cases)
}

/// Build the pr-reviewer prompt — same shape as the `darkmux-review.yml`
/// workflow: the author's intent (title + description) then the diff.
fn build_prompt(c: &Case) -> String {
    let body = if c.label.intent_body.trim().is_empty() {
        "(no description provided)"
    } else {
        c.label.intent_body.trim()
    };
    format!(
        "PR review request: {title}\n\n\
         Description (the author's stated intent for this change):\n{body}\n\n\
         Respond per your role contract (the single JSON object). Assess the diff \
         against the stated intent above. The full diff is below; you have only this diff.\n\n\
         ```diff\n{diff}\n```\n",
        title = c.label.intent_title,
        body = body,
        diff = c.diff,
    )
}

/// Dispatch one case through the `pr-reviewer` role on the internal runtime,
/// returning the raw `--json` envelope stdout (parsed by `extract_reply_text`).
fn dispatch_case(prompt: &str, case_id: &str, opts: &ReviewBenchOpts) -> Result<String> {
    use darkmux_crew::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts, Runtime};
    let session_id = format!(
        "pr-review-bench-{}-{}",
        case_id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let d = DispatchOpts {
        role_id: "pr-reviewer".to_string(),
        message: prompt.to_string(),
        deliver: None,
        session_id: Some(session_id),
        timeout_seconds: opts.timeout_seconds,
        skip_preflight: false,
        json: true,
        watch_paths: Vec::new(),
        workdir: None,
        sprint_id: None,
        runtime: Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
        // pr-review is single-turn; no compaction to override.
        compaction: CompactionDispatchArgs::default(),
        profile_name: opts.profile_name.clone(),
        config_path: opts.config_path.clone(),
        image: None,
    };
    let r = dispatch(d).context("pr-review-bench internal-runtime dispatch")?;
    Ok(r.stdout)
}

/// Extract the model's review JSON (verdict + findings) from `final_assistant`
/// text, tolerant of ```json fences and surrounding prose. Returns
/// `parsed == false` when no `verdict`-bearing object can be recovered.
pub fn parse_review(text: &str) -> Review {
    for cand in json_candidates(text) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cand) {
            if let Some(verdict) = v.get("verdict").and_then(|x| x.as_str()) {
                let findings = v
                    .get("findings")
                    .and_then(|f| f.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|f| Finding {
                                severity: f
                                    .get("severity")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or_default()
                                    .to_lowercase(),
                                anchor: f
                                    .get("anchor")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                title: f
                                    .get("title")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Review {
                    verdict: verdict.to_lowercase(),
                    findings,
                    parsed: true,
                };
            }
        }
    }
    Review::default()
}

/// Candidate JSON substrings to try parsing, most-specific first:
/// a fenced ```json block, the whole text, then first-`{`..last-`}`.
fn json_candidates(text: &str) -> Vec<String> {
    let s = text.trim();
    let mut out = Vec::new();
    if let Some(open) = s.find("```") {
        if let Some(close_rel) = s[open + 3..].find("```") {
            let inner = s[open + 3..open + 3 + close_rel]
                .trim_start_matches("json")
                .trim();
            if !inner.is_empty() {
                out.push(inner.to_string());
            }
        }
    }
    out.push(s.to_string());
    if let (Some(a), Some(b)) = (s.find('{'), s.rfind('}')) {
        if b > a {
            out.push(s[a..=b].to_string());
        }
    }
    out
}

/// Score one review against its label. Pure — the unit-tested core.
pub fn score(label: &Label, r: &Review) -> CaseScore {
    let mut s = CaseScore {
        verdict: r.verdict.clone(),
        findings: r.findings.len(),
        ..Default::default()
    };
    if !r.parsed {
        s.degenerate = true;
        return s;
    }
    let high = r.findings.iter().filter(|f| f.severity == "high").count();
    if label.kind == "bug" {
        let anchor_ok = label
            .anchor_contains
            .as_ref()
            .map(|a| r.findings.iter().any(|f| f.anchor.contains(a.as_str())))
            .unwrap_or(false);
        s.anchor_ok = anchor_ok;
        s.recall = r.verdict == "flag" && (anchor_ok || high > 0);
        s.empty_flag = r.verdict == "flag" && r.findings.is_empty();
        s.correct = s.recall;
    } else {
        // clean: any finding is a false positive; want pass + empty.
        s.fp = r.findings.len();
        s.correct = r.verdict == "pass" && r.findings.is_empty();
    }
    s
}

fn describe(label: &Label, s: &CaseScore) -> String {
    if s.degenerate {
        return "DEGENERATE (empty/unparseable review — e.g. #1050)".to_string();
    }
    if label.kind == "bug" {
        let mut bits = vec![format!("recall={}", yn(s.recall)), format!("anchor={}", yn(s.anchor_ok))];
        if s.empty_flag {
            bits.push("EMPTY-FLAG (flag w/ 0 findings — contract violation)".to_string());
        }
        bits.join(" ")
    } else if s.fp == 0 && s.correct {
        "clean-pass".to_string()
    } else {
        format!("{} false-positive(s)", s.fp)
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "Y"
    } else {
        "N"
    }
}

fn print_summary(scored: &[(&Case, CaseScore)], opts: &ReviewBenchOpts) {
    let clean: Vec<&CaseScore> = scored
        .iter()
        .filter(|(c, _)| c.label.kind == "clean")
        .map(|(_, s)| s)
        .collect();
    let bugs: Vec<&CaseScore> = scored
        .iter()
        .filter(|(c, _)| c.label.kind == "bug")
        .map(|(_, s)| s)
        .collect();
    let fp_total: usize = clean.iter().map(|s| s.fp).sum();
    let clean_pass = clean.iter().filter(|s| s.correct).count();
    let recall = bugs.iter().filter(|s| s.recall).count();
    let anchor = bugs.iter().filter(|s| s.anchor_ok).count();
    let degenerate = scored.iter().filter(|(_, s)| s.degenerate).count();
    println!("\n── summary ({}) ──", opts.profile_name.as_deref().unwrap_or("default"));
    println!("clean: {}/{} pass · {} false positives", clean_pass, clean.len(), fp_total);
    println!("bug:   {}/{} recall · {}/{} correct anchor", recall, bugs.len(), anchor, bugs.len());
    if degenerate > 0 {
        println!("degenerate: {} (empty/unparseable — model unfit for this role)", degenerate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lbl(kind: &str, anchor: Option<&str>) -> Label {
        Label {
            kind: kind.into(),
            intent_title: "t".into(),
            intent_body: String::new(),
            expect_verdict: if kind == "bug" { "flag".into() } else { "pass".into() },
            bug_class: None,
            anchor_contains: anchor.map(str::to_string),
            notes: None,
        }
    }

    #[test]
    fn parse_review_plain_json() {
        let r = parse_review(r#"{"verdict":"pass","findings":[]}"#);
        assert!(r.parsed);
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn parse_review_fenced_with_prose() {
        let r = parse_review("Here is my review:\n```json\n{\"verdict\":\"flag\",\"findings\":[{\"severity\":\"HIGH\",\"anchor\":\"let sql = format!(x)\",\"title\":\"SQLi\"}]}\n```\nDone.");
        assert!(r.parsed);
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "high"); // lowercased
    }

    #[test]
    fn parse_review_findings_with_braces_in_strings() {
        // a suggestion/detail containing { } must not break extraction.
        let r = parse_review(r#"{"verdict":"flag","findings":[{"severity":"high","anchor":"a","title":"x","suggestion":"fn f() { ok }"}]}"#);
        assert!(r.parsed);
        assert_eq!(r.findings.len(), 1);
    }

    #[test]
    fn parse_review_degenerate_when_no_verdict() {
        assert!(!parse_review("").parsed);
        assert!(!parse_review("just some reasoning, no json").parsed);
        assert!(!parse_review(r#"{"summary":"thought about it"}"#).parsed);
    }

    #[test]
    fn score_clean_pass_is_correct() {
        let s = score(&lbl("clean", None), &Review { verdict: "pass".into(), findings: vec![], parsed: true });
        assert_eq!(s.fp, 0);
        assert!(s.correct);
    }

    #[test]
    fn score_clean_with_finding_is_false_positive() {
        let r = Review { verdict: "pass".into(), findings: vec![Finding { severity: "medium".into(), ..Default::default() }], parsed: true };
        let s = score(&lbl("clean", None), &r);
        assert_eq!(s.fp, 1);
        assert!(!s.correct); // a finding on a clean diff is wrong even if verdict=pass
    }

    #[test]
    fn score_bug_recall_via_anchor() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "let sql = format!(\"SELECT ...\", name)".into(), title: "SQLi".into() }], parsed: true };
        let s = score(&lbl("bug", Some("format!(\"SELECT")), &r);
        assert!(s.recall);
        assert!(s.anchor_ok);
        assert!(s.correct);
    }

    #[test]
    fn score_bug_recall_via_high_severity_without_anchor_match() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "wrong line".into(), title: "SQLi".into() }], parsed: true };
        let s = score(&lbl("bug", Some("format!")), &r);
        assert!(s.recall); // high severity counts as caught
        assert!(!s.anchor_ok); // but the anchor missed
    }

    #[test]
    fn score_bug_empty_flag_is_contract_violation_not_recall() {
        // verdict=flag with zero findings (the gpt-oss failure mode).
        let r = Review { verdict: "flag".into(), findings: vec![], parsed: true };
        let s = score(&lbl("bug", Some("format!")), &r);
        assert!(s.empty_flag);
        assert!(!s.recall);
        assert!(!s.correct);
    }

    #[test]
    fn score_degenerate_review() {
        let s = score(&lbl("clean", None), &Review::default());
        assert!(s.degenerate);
        assert!(!s.correct);
    }
}
