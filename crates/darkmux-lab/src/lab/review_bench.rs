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

fn default_true() -> bool {
    true
}

/// One expected finding the control (the hosted reviewer) raised on a case —
/// the multi-finding ground truth (#1119 parity extension). A case lists EVERY
/// finding the control made and the accepted-fix trail / manual review
/// confirmed valid, so recall and precision are scored per-finding across the
/// corpus rather than one-anchor-per-diff. `required` distinguishes the
/// must-catch defects (which define recall) from valid-but-optional nits.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedFinding {
    /// Substring the correct finding's anchor should contain (the bug's
    /// line/symbol). Drives anchor-accuracy scoring, and recall matching when
    /// `match_contains` is absent.
    pub anchor_contains: String,
    /// Optional substring in the model finding's anchor/title that identifies
    /// THIS bug regardless of anchor precision — lets a bug count as recalled
    /// when the model flags the right defect but anchors imprecisely (#1043).
    /// Falls back to `anchor_contains`.
    #[serde(default)]
    pub match_contains: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub bug_class: Option<String>,
    /// true = the bug needs repo context OUTSIDE the diff (access-gap); false =
    /// fully visible in the diff (keenness-gap). Recall is reported split on
    /// this axis so gains attribute to the right lever (repo-read vs framing).
    #[serde(default)]
    pub access_gap: bool,
    /// When true (default), this finding is a must-catch defect that counts
    /// toward RECALL. When false, it's a valid-but-optional finding (e.g. a
    /// defensive nit the control raised): a model finding matching it is a true
    /// positive (NOT a false positive), but it is not required for recall. This
    /// keeps the control at ~100% precision against its own labels — the
    /// consistency check that the labels are sound.
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

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
    /// NOTE: a `bug` case with no `anchor_contains` is only scored as recalled
    /// when the model emits a HIGH-severity finding — set this unless the
    /// planted defect is genuinely high-severity, or the case is uncatchable.
    /// LEGACY single-anchor path — superseded by `expected` (below) when present.
    #[serde(default)]
    pub anchor_contains: Option<String>,
    /// Multi-finding ground truth (#1119): every real bug in a `bug` case. When
    /// non-empty this is authoritative and enables corpus-wide recall + precision
    /// (false positives counted on bug cases too). When empty, the legacy
    /// single-`anchor_contains` path is used — existing built-in cases + unit
    /// tests score identically.
    #[serde(default)]
    pub expected: Vec<ExpectedFinding>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl Label {
    /// True when this case carries the multi-finding schema (`expected`
    /// non-empty) — a bug case with must-catch defects, or a clean-ish case
    /// with optional nits. When false, the legacy single-`anchor_contains`
    /// path is used.
    fn uses_multi(&self) -> bool {
        !self.expected.is_empty()
    }
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
    /// false-positive findings — clean: all findings; bug (multi path): findings
    /// matching no expected bug. (Legacy bug path leaves this 0.)
    pub fp: usize,
    /// bug: caught. Legacy: flag + (anchor match or high-severity). Multi: every
    /// expected bug matched (per-case "all caught").
    pub recall: bool,
    /// bug: a finding anchored to the expected line(s).
    pub anchor_ok: bool,
    /// flag verdict with an empty findings array — contract violation
    /// (the gpt-oss under-flag failure mode), distinct from a true pass.
    pub empty_flag: bool,
    /// matched the expectation for its kind.
    pub correct: bool,
    // ── multi-finding tallies (#1119) — feed corpus-wide recall + precision ──
    /// expected real bugs in this case (0 for clean).
    pub expected_bugs: usize,
    /// expected bugs recalled (a matching finding under a `flag` verdict).
    pub bugs_caught: usize,
    /// of the expected bugs, how many are access-gap / diff-visible.
    pub expected_access: usize,
    pub expected_diff: usize,
    /// of the caught bugs, how many were access-gap / diff-visible.
    pub caught_access: usize,
    pub caught_diff: usize,
    /// model findings matching an expected bug (true positives).
    pub tp: usize,
    /// caught bugs whose anchor was precise.
    pub anchors_ok: usize,
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
        // (#1119 QA) Fail loud on an unknown `kind` — `score` treats anything
        // not "bug" as clean, while `print_summary` buckets on exact "clean"/"bug",
        // so a typo ("Bug", trailing space) would silently miscount + drop the
        // case from the totals. Operator-sovereignty: never a number you can't trace.
        if label.kind != "clean" && label.kind != "bug" {
            return Err(anyhow!(
                "case \"{id}\": label.kind must be \"clean\" or \"bug\", got {:?} ({})",
                label.kind,
                label_path.display()
            ));
        }
        // Guard the multi-finding ground truth (#1119) so a mislabeled case can't
        // silently distort recall/precision — loud-fail like the `kind` check.
        for (i, e) in label.expected.iter().enumerate() {
            let anchor_empty = e.anchor_contains.trim().is_empty();
            let match_empty = e
                .match_contains
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty();
            if anchor_empty && match_empty {
                return Err(anyhow!(
                    "case \"{id}\": expected[{i}] needs a non-empty anchor_contains or \
                     match_contains — an empty anchor matches every finding ({})",
                    label_path.display()
                ));
            }
        }
        if label.kind == "clean" && label.expected.iter().any(|e| e.required) {
            return Err(anyhow!(
                "case \"{id}\": a \"clean\" case must not carry a required expected finding \
                 (clean = 0 must-catch bugs; use \"required\": false for optional nits) ({})",
                label_path.display()
            ));
        }
        if label.kind == "bug"
            && !label.expected.is_empty()
            && !label.expected.iter().any(|e| e.required)
        {
            return Err(anyhow!(
                "case \"{id}\": a \"bug\" case with expected[] needs at least one required \
                 finding (else it contributes nothing to recall) ({})",
                label_path.display()
            ));
        }
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
    // Bug cases carrying the multi-finding schema (#1119) take the per-bug path;
    // everything else (legacy bug labels, all clean cases) keeps the original
    // behavior — clean scoring is identical either way (no expected bugs).
    if label.uses_multi() {
        return score_multi(label, r, s);
    }
    // ── legacy single-anchor path (behavior unchanged) ──
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

/// Multi-finding scoring (#1119): compute a MAXIMUM bipartite matching between
/// model findings and expected findings (each finding satisfies ≤1 expected,
/// each expected claimed by ≤1 finding), then derive per-bug recall +
/// corpus-ready precision. Max-matching — not greedy — so a model that flagged
/// everything is never under-credited by emit order (frontier QA #1). A finding
/// matching no expected = false positive; a `required` expected matched under a
/// `flag` verdict = recalled; a matched `optional` expected is a true positive
/// but not required for recall (so the control stays ~100% precision on its own
/// labels).
fn score_multi(label: &Label, r: &Review, mut s: CaseScore) -> CaseScore {
    let expected = &label.expected;

    // Recall is scored over the REQUIRED (must-catch) subset only.
    s.expected_bugs = expected.iter().filter(|e| e.required).count();
    s.expected_access = expected.iter().filter(|e| e.required && e.access_gap).count();
    s.expected_diff = expected.iter().filter(|e| e.required && !e.access_gap).count();

    // PRECISION: maximum matching over ALL expected — a finding matching any
    // expected (required OR optional) is a true positive; the rest are FPs.
    let matched_all = max_match(&r.findings, expected, |_| true);
    s.tp = matched_all.iter().filter(|x| x.is_some()).count();
    s.fp = r.findings.len() - s.tp;

    // RECALL: a SEPARATE maximum matching over the REQUIRED expected ONLY (frontier
    // QA #1). Max-cardinality over the *full* set is blind to the required/optional
    // priority — an optional nit sharing a match key could win the contended
    // finding and starve the required bug, under-reporting recall for the very
    // (under-emitting) models we benchmark, order-dependently. A required-only
    // matching provably maximizes required coverage and is order-independent.
    // Credit only under a `flag` verdict (a `pass` carrying a matching finding is
    // a self-contradiction, not a catch).
    let flagged = r.verdict == "flag";
    let matched_req = max_match(&r.findings, expected, |e| e.required);
    for (ei, e) in expected.iter().enumerate() {
        if !e.required {
            continue;
        }
        if let (true, Some(fi)) = (flagged, matched_req[ei]) {
            s.bugs_caught += 1;
            if e.access_gap {
                s.caught_access += 1;
            } else {
                s.caught_diff += 1;
            }
            // Anchor precision: the finding that MATCHED this bug anchors to its
            // line (not just any finding — avoids cross-attribution, QA #8).
            if r.findings[fi]
                .anchor
                .to_lowercase()
                .contains(&e.anchor_contains.to_lowercase())
            {
                s.anchors_ok += 1;
            }
        }
    }

    s.empty_flag = flagged && r.findings.is_empty();
    if s.expected_bugs > 0 {
        s.recall = s.bugs_caught == s.expected_bugs;
        s.anchor_ok = s.anchors_ok == s.expected_bugs;
        s.correct = s.recall;
    } else {
        // clean-ish (no must-catch bugs): correct = no false positives (matching
        // an optional nit is fine; junk is not).
        s.correct = s.fp == 0;
    }
    s
}

/// Maximum bipartite matching (Kuhn's) between model findings and the subset of
/// expected selected by `include`. Returns `matched_by[ei] = Some(fi)` for each
/// matched expected. Run once over all expected (precision) and once over the
/// required-only subset (recall), so an optional finding can never starve a
/// required bug. The matching SIZE is order-independent, so the recall/precision
/// counts derived from it are too.
fn max_match(
    findings: &[Finding],
    expected: &[ExpectedFinding],
    include: impl Fn(&ExpectedFinding) -> bool,
) -> Vec<Option<usize>> {
    let adj: Vec<Vec<usize>> = findings
        .iter()
        .map(|f| {
            expected
                .iter()
                .enumerate()
                .filter(|(_, e)| include(e) && finding_matches_expected(f, e))
                .map(|(ei, _)| ei)
                .collect()
        })
        .collect();
    let mut matched_by: Vec<Option<usize>> = vec![None; expected.len()];
    for fi in 0..findings.len() {
        let mut seen = vec![false; expected.len()];
        augment(fi, &adj, &mut matched_by, &mut seen);
    }
    matched_by
}

/// Kuhn's augmenting-path step for maximum bipartite matching: try to match
/// finding `fi` to some expected, displacing an earlier match when that frees a
/// slot down the chain. `seen` guards against revisiting an expected this pass.
fn augment(fi: usize, adj: &[Vec<usize>], matched_by: &mut [Option<usize>], seen: &mut [bool]) -> bool {
    for &ei in &adj[fi] {
        if seen[ei] {
            continue;
        }
        seen[ei] = true;
        let cur = matched_by[ei];
        if cur.is_none() || augment(cur.unwrap(), adj, matched_by, seen) {
            matched_by[ei] = Some(fi);
            return true;
        }
    }
    false
}

/// A model finding satisfies an expected finding if its title/anchor carries the
/// expected `match_contains` keyword (when set), else if its anchor overlaps the
/// expected `anchor_contains`. Case-insensitive.
fn finding_matches_expected(f: &Finding, e: &ExpectedFinding) -> bool {
    match e.match_contains.as_deref() {
        Some(m) if !m.trim().is_empty() => {
            let hay = format!("{} {}", f.anchor.to_lowercase(), f.title.to_lowercase());
            hay.contains(&m.to_lowercase())
        }
        _ => f
            .anchor
            .to_lowercase()
            .contains(&e.anchor_contains.to_lowercase()),
    }
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

    // Corpus-wide parity (#1119) — the measuring stick against the control's
    // labels: per-bug recall + per-finding precision, with the access-gap vs
    // diff-visible split for lever attribution. Printed only when the corpus
    // carries the multi-finding schema.
    if scored.iter().any(|(c, _)| c.label.uses_multi()) {
        let pct = |num: usize, den: usize| -> f64 {
            if den == 0 {
                100.0
            } else {
                100.0 * num as f64 / den as f64
            }
        };
        let expected_bugs: usize = scored.iter().map(|(_, s)| s.expected_bugs).sum();
        let bugs_caught: usize = scored.iter().map(|(_, s)| s.bugs_caught).sum();
        let tp: usize = scored.iter().map(|(_, s)| s.tp).sum();
        let fp_all: usize = scored.iter().map(|(_, s)| s.fp).sum();
        let total_findings = tp + fp_all;
        let anchors_ok: usize = scored.iter().map(|(_, s)| s.anchors_ok).sum();
        let exp_access: usize = scored.iter().map(|(_, s)| s.expected_access).sum();
        let exp_diff: usize = scored.iter().map(|(_, s)| s.expected_diff).sum();
        let caught_access: usize = scored.iter().map(|(_, s)| s.caught_access).sum();
        let caught_diff: usize = scored.iter().map(|(_, s)| s.caught_diff).sum();
        let recall_pct = pct(bugs_caught, expected_bugs);
        let precision_pct = pct(tp, total_findings);
        // The BAR is only meaningful with must-catch bugs AND emitted findings;
        // otherwise it is n/a rather than a spurious PASS.
        let bar_meaningful = expected_bugs > 0 && total_findings > 0;
        let bar = bar_meaningful && recall_pct >= 80.0 && precision_pct >= 80.0;
        // Show one decimal so the printed number agrees with the `>= 80.0` bar
        // (avoids "80%" printed next to FAIL, QA #3); n/a on an empty denominator.
        let frac = |num: usize, den: usize, p: f64| -> String {
            if den == 0 {
                format!("{}/{} = n/a", num, den)
            } else {
                format!("{}/{} = {:.1}%", num, den, p)
            }
        };
        println!(
            "\n── parity vs control ({}) ──",
            opts.profile_name.as_deref().unwrap_or("default")
        );
        // Loud-fail if a bug case is still legacy while multi cases exist — its
        // findings would silently vanish from the precision denominator (QA #2).
        let legacy_bug_cases = scored
            .iter()
            .filter(|(c, _)| c.label.kind == "bug" && !c.label.uses_multi())
            .count();
        if legacy_bug_cases > 0 {
            println!(
                "⚠ MIXED CORPUS: {} legacy bug case(s) (no expected[]) are EXCLUDED from precision — \
                 migrate every bug case to the multi-finding schema for a trustworthy number.",
                legacy_bug_cases
            );
        }
        println!(
            "recall:    {} bugs  (access-gap {}/{}, diff-visible {}/{})",
            frac(bugs_caught, expected_bugs, recall_pct),
            caught_access,
            exp_access,
            caught_diff,
            exp_diff
        );
        println!(
            "precision: {} findings  ({} false positive{})",
            frac(tp, total_findings, precision_pct),
            fp_all,
            if fp_all == 1 { "" } else { "s" }
        );
        println!("anchor:    {}/{} caught bugs precisely anchored", anchors_ok, bugs_caught);
        println!(
            "BAR (recall≥80 AND precision≥80): {}",
            if !bar_meaningful {
                "n/a"
            } else if bar {
                "PASS ✅"
            } else {
                "FAIL ❌"
            }
        );
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
            expected: Vec::new(),
            notes: None,
        }
    }

    fn ef(anchor: &str, access_gap: bool) -> ExpectedFinding {
        ExpectedFinding {
            anchor_contains: anchor.into(),
            match_contains: None,
            severity: None,
            bug_class: None,
            access_gap,
            required: true,
            notes: None,
        }
    }

    fn ef_opt(anchor: &str) -> ExpectedFinding {
        ExpectedFinding {
            required: false,
            ..ef(anchor, false)
        }
    }

    fn multi_lbl(kind: &str, expected: Vec<ExpectedFinding>) -> Label {
        Label {
            kind: kind.into(),
            intent_title: "t".into(),
            intent_body: String::new(),
            expect_verdict: if kind == "bug" { "flag".into() } else { "pass".into() },
            bug_class: None,
            anchor_contains: None,
            expected,
            notes: None,
        }
    }

    fn finding(sev: &str, anchor: &str, title: &str) -> Finding {
        Finding {
            severity: sev.into(),
            anchor: anchor.into(),
            title: title.into(),
        }
    }

    fn flagged(findings: Vec<Finding>) -> Review {
        Review {
            verdict: "flag".into(),
            findings,
            parsed: true,
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

    #[test]
    fn load_cases_rejects_unknown_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"Regression","intent_title":"t","expect_verdict":"flag"}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(
            err.to_string().contains(r#"must be "clean" or "bug""#),
            "got: {err}"
        );
    }

    #[test]
    fn load_cases_loads_a_good_pair() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("c.label.json"),
            r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
        )
        .unwrap();
        fs::write(d.join("c.diff"), "diff --git a b\n").unwrap();
        let cases = load_cases(d).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "c");
        assert_eq!(cases[0].label.kind, "clean");
    }

    // ── multi-finding path (#1119) ──

    #[test]
    fn multi_two_bugs_partial_recall() {
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false), ef("beta.rs:20", false)]);
        let r = flagged(vec![finding("high", "alpha.rs:10", "off-by-one")]);
        let s = score(&label, &r);
        assert_eq!(s.expected_bugs, 2);
        assert_eq!(s.bugs_caught, 1);
        assert!(!s.recall, "not all bugs caught");
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 0);
    }

    #[test]
    fn multi_extra_finding_is_fp_on_bug_case() {
        // The new behavior: a junk finding on a BUG case is a false positive
        // (legacy scoring only counted FPs on clean cases).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = flagged(vec![
            finding("high", "alpha.rs:10", "real bug"),
            finding("medium", "unrelated.rs:99", "reflexive null check"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1);
        assert!(s.recall);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 1, "the unrelated finding is a false positive");
    }

    #[test]
    fn multi_access_vs_diff_split() {
        let label = multi_lbl("bug", vec![ef("comp.tsx:5", true), ef("calc.ts:40", false)]);
        let r = flagged(vec![
            finding("high", "comp.tsx:5", "block-in-span"),
            finding("high", "calc.ts:40", "undercount"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 2);
        assert_eq!(s.caught_access, 1);
        assert_eq!(s.caught_diff, 1);
        assert_eq!(s.expected_access, 1);
        assert_eq!(s.expected_diff, 1);
        assert!(s.recall);
    }

    #[test]
    fn multi_match_contains_recalls_without_precise_anchor() {
        let label = multi_lbl(
            "bug",
            vec![ExpectedFinding {
                anchor_contains: "calc.ts:40".into(),
                match_contains: Some("undercount".into()),
                severity: None,
                bug_class: None,
                access_gap: false,
                required: true,
                notes: None,
            }],
        );
        // Model flags the right bug (title matches) but anchors the wrong line.
        let r = flagged(vec![finding("high", "calc.ts:12", "day undercount off-by-one")]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1, "recalled via match_contains");
        assert!(s.recall);
        assert_eq!(s.anchors_ok, 0, "anchor was imprecise");
        assert!(!s.anchor_ok);
    }

    #[test]
    fn multi_pass_verdict_does_not_credit_recall() {
        // A matching finding under a `pass` verdict: precision credits the TP,
        // but recall does not (the model contradicted itself; it did not flag).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = Review {
            verdict: "pass".into(),
            findings: vec![finding("high", "alpha.rs:10", "real bug")],
            parsed: true,
        };
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 0, "pass verdict: not flagged");
        assert!(!s.recall);
        assert_eq!(s.tp, 1, "the finding still matches a real bug");
    }

    #[test]
    fn multi_all_caught_is_correct() {
        let label = multi_lbl("bug", vec![ef("a:1", false), ef("b:2", true)]);
        let r = flagged(vec![finding("high", "a:1", "x"), finding("medium", "b:2", "y")]);
        let s = score(&label, &r);
        assert!(s.recall);
        assert!(s.correct);
        assert_eq!(s.fp, 0);
    }

    #[test]
    fn multi_max_matching_beats_greedy() {
        // Frontier QA #1: overlapping match keys — a file-level bug + a
        // line-specific bug in the same file; the model flagged BOTH (one at the
        // exact line, one elsewhere in the file). Greedy would strand one bug as
        // a miss + a spurious FP; maximum matching credits both.
        let label = multi_lbl("bug", vec![ef("calc.ts", false), ef("calc.ts:40", false)]);
        let r = flagged(vec![
            finding("high", "calc.ts:40", "exact"),
            finding("high", "calc.ts:88", "elsewhere in the same file"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 2, "both bugs caught under max matching");
        assert_eq!(s.tp, 2);
        assert_eq!(s.fp, 0, "no spurious false positive");
        assert!(s.recall);
    }

    #[test]
    fn multi_duplicate_finding_is_fp() {
        // Two identical findings for one bug: one TP, one FP (max matching
        // preserves duplicate-as-FP).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = flagged(vec![
            finding("high", "alpha.rs:10", "bug"),
            finding("high", "alpha.rs:10", "bug again"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 1, "the duplicate is a false positive");
    }

    #[test]
    fn multi_optional_finding_is_tp_not_recall() {
        // A required bug + an optional nit, both flagged. The optional match is a
        // TP (not an FP) but NOT in the recall denominator — keeps the control at
        // ~100% precision on its own labels.
        let label = multi_lbl("bug", vec![ef("a:1", false), ef_opt("nit.rs:9")]);
        let r = flagged(vec![
            finding("high", "a:1", "real"),
            finding("low", "nit.rs:9", "nit"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.expected_bugs, 1, "only the required bug counts for recall");
        assert_eq!(s.bugs_caught, 1);
        assert!(s.recall);
        assert_eq!(s.tp, 2, "both findings match an expected (required + optional)");
        assert_eq!(s.fp, 0, "the optional-nit match is not a false positive");
    }

    #[test]
    fn multi_clean_with_optional_only() {
        // A clean-ish case carrying only an optional nit: 0 required bugs (no
        // recall impact); flagging the nit is a TP, junk is an FP.
        let label = multi_lbl("clean", vec![ef_opt("nit.rs:9")]);
        let ok = score(&label, &flagged(vec![finding("low", "nit.rs:9", "nit")]));
        assert_eq!(ok.expected_bugs, 0);
        assert_eq!(ok.tp, 1);
        assert_eq!(ok.fp, 0);
        assert!(ok.correct, "no false positives ⇒ correct");
        let junk = score(&label, &flagged(vec![finding("high", "other.rs:1", "junk")]));
        assert_eq!(junk.fp, 1);
        assert!(!junk.correct);
    }

    #[test]
    fn load_cases_rejects_empty_expected_match_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"bug","intent_title":"t","expected":[{"anchor_contains":""}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("matches every finding"), "got: {err}");
    }

    #[test]
    fn load_cases_rejects_clean_with_required_bug() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"clean","intent_title":"t","expected":[{"anchor_contains":"a:1"}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("must not carry a required"), "got: {err}");
    }

    #[test]
    fn multi_optional_does_not_steal_recall_from_required() {
        // Frontier QA (2nd pass) #1: a required + an optional expected share an
        // overlapping match key, and the model emits ONE finding that IS the
        // required bug. Max-cardinality over the full set could spend it on the
        // optional; the required-only matching must not. Recall must be TRUE
        // regardless of expected[] order.
        let req = ef("calc.ts:40", false); // required (default)
        let opt = ef_opt("calc.ts"); // optional, subsuming match key
        let f = finding("high", "calc.ts:40", "the required bug");
        for order in [vec![opt.clone(), req.clone()], vec![req.clone(), opt.clone()]] {
            let s = score(&multi_lbl("bug", order), &flagged(vec![f.clone()]));
            assert_eq!(s.expected_bugs, 1);
            assert!(s.recall, "required bug caught regardless of expected[] order");
            assert_eq!(s.bugs_caught, 1);
            assert_eq!(s.tp, 1, "the one finding is a true positive");
        }
    }

    #[test]
    fn load_cases_rejects_bug_with_no_required() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"bug","intent_title":"t","expected":[{"anchor_contains":"a:1","required":false}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("at least one required"), "got: {err}");
    }
}
