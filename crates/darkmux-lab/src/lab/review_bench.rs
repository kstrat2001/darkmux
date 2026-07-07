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

#[derive(Debug, Default, serde::Serialize)]
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

/// Output contract the reviewer dispatches under (#1119 free-form mode).
/// `Strict` is the shipped `pr-reviewer` role — grammar-locked to a JSON
/// schema via `output_schema` (LMStudio `response_format: json_schema`).
/// `FreeForm` dispatches the sibling `pr-reviewer-freeform` role instead —
/// no grammar lock, ordinary prose with `MUST FIX:`/`CONSIDER:` marker
/// lines — to measure whether the JSON contract itself suppresses recall
/// (a real model returned `verdict: pass, findings: []` under Strict on a
/// diff it caught 5/5 free-form in an ad-hoc probe; this mode makes that
/// comparison reproducible). Scoring is identical either way: both modes
/// reduce to the same `Review { verdict, findings }` shape and go through
/// the same `score()`/`score_multi()` matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BenchMode {
    #[default]
    Strict,
    FreeForm,
    /// The production agentic condition (#1197 anchor experiment): dispatches
    /// `pr-reviewer-agentic` — tools + the repository checked out at the
    /// reviewed commit (per-case tree under `ReviewBenchOpts::workdirs`,
    /// mounted as the dispatch workdir). Same freeform marker dialect, same
    /// parser, same scorer — the ONLY variable added over `FreeForm` is
    /// evidence access. Diff-only cells measured 0–2/18 recall across both
    /// tiers (2026-07-05); this mode measures whether access is the lever.
    Agentic,
    /// (#1222) The dialectic (adversarial) condition: prosecutor → defender
    /// → judge as three chained per-seat dispatches (`lab::dialectic`).
    /// Advocates run agentic (per-case repo tree mounted, like `Agentic`);
    /// the judge is tool-less and rules on the record. The judge's sustained
    /// charges become the review, scored by the same matcher as every other
    /// mode — the dialectic is entirely upstream of scoring.
    Dialectic,
}

impl BenchMode {
    fn role_id(self) -> &'static str {
        match self {
            BenchMode::Strict => "pr-reviewer",
            BenchMode::FreeForm => "pr-reviewer-freeform",
            BenchMode::Agentic => "pr-reviewer-agentic",
            // The dialectic mode dispatches three per-seat roles; the
            // per-case loop branches to `run_debate` before ever asking the
            // mode for a single role id.
            BenchMode::Dialectic => unreachable!("dialectic dispatches per-seat roles"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            BenchMode::Strict => "strict",
            BenchMode::FreeForm => "freeform",
            BenchMode::Agentic => "agentic",
            BenchMode::Dialectic => "dialectic",
        }
    }
}

pub struct ReviewBenchOpts {
    pub cases_dir: PathBuf,
    pub profile_name: Option<String>,
    pub config_path: Option<String>,
    pub timeout_seconds: u32,
    /// (#1198) Where to write the scores.json artifact. `None` = the default
    /// location under the runs dir (`review-bench-<unix-ts>/scores.json`).
    pub scores_out: Option<PathBuf>,
    pub mode: BenchMode,
    /// Agentic mode's evidence root: a directory holding one subdirectory PER
    /// CASE ID — the repository tree at that case's reviewed commit (built by
    /// the operator, e.g. `git archive <commit> | tar -x`; the bench never
    /// touches the source repos). Each case's tree mounts as the dispatch
    /// workdir. Required when `mode == Agentic` (and `Dialectic`, whose
    /// advocates run agentic); a case with no tree bails the bench loudly —
    /// a half-agentic run would corrupt comparability.
    pub workdirs: Option<PathBuf>,
    /// (#1222) Per-seat profile overrides for `Dialectic`; each seat falls
    /// back to `profile_name`. Debug phase runs ONE local profile in all
    /// three seats — same weights everywhere, so any delta vs the solo
    /// baseline is attributable purely to the structure.
    pub prosecutor_profile: Option<String>,
    pub defender_profile: Option<String>,
    pub judge_profile: Option<String>,
}

pub fn run_review_bench(opts: ReviewBenchOpts) -> Result<()> {
    let cases = load_cases(&opts.cases_dir)?;
    if cases.is_empty() {
        return Err(anyhow!(
            "no cases (*.label.json + sibling *.diff) found in {}",
            opts.cases_dir.display()
        ));
    }
    // Agentic mode's preflight: every case must have its evidence tree BEFORE
    // any dispatch spends tokens — a half-agentic run corrupts comparability.
    let workdir_for = |case_id: &str| -> Option<PathBuf> {
        opts.workdirs.as_ref().map(|root| root.join(case_id))
    };
    if matches!(opts.mode, BenchMode::Agentic | BenchMode::Dialectic) {
        let root = opts.workdirs.as_ref().ok_or_else(|| {
            anyhow!(
                "--{} requires --workdirs <root> (one repo tree per case id)",
                opts.mode.label()
            )
        })?;
        let missing: Vec<&str> = cases
            .iter()
            .filter(|c| !root.join(&c.id).is_dir())
            .map(|c| c.id.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(anyhow!(
                "{} mode: no repo tree under {} for case(s): {} — build each with \
                 `git archive <reviewed-commit> | tar -x -C <root>/<case-id>`",
                opts.mode.label(),
                root.display(),
                missing.join(", ")
            ));
        }
    } else if opts.workdirs.is_some() {
        return Err(anyhow!(
            "--workdirs only applies to --agentic / --dialectic (diff-only modes \
             never read a repo tree; passing one would silently measure nothing)"
        ));
    }
    eprintln!(
        "pr-review-bench: {} cases · profile={} · profiles-file={} · mode={}",
        cases.len(),
        opts.profile_name.as_deref().unwrap_or("(default)"),
        opts.config_path.as_deref().unwrap_or("(registry)"),
        opts.mode.label(),
    );
    println!("{:<34}{:<7}{:<7}{:<4}outcome", "case", "kind", "verd", "f");
    let mut scored: Vec<(&Case, CaseScore)> = Vec::new();
    let mut meta: Vec<EnvelopeMeta> = Vec::new();
    // (#1222) Dialectic mode's composite artifacts: one debate envelope per
    // case, written beside scores.json — the dispatches stay atomic; the
    // debate exists as an artifact, not a flow shape.
    let mut debates: Vec<super::dialectic::DebateEnvelope> = Vec::new();
    for c in &cases {
        let review = if opts.mode == BenchMode::Dialectic {
            use super::dialectic::Seat;
            let workdir = workdir_for(&c.id);
            let (review, debate) =
                super::dialectic::run_debate(c, workdir.as_deref(), |seat, prompt| {
                    let profile = match seat {
                        Seat::Prosecutor => opts.prosecutor_profile.as_deref(),
                        Seat::Defender => opts.defender_profile.as_deref(),
                        Seat::Judge => opts.judge_profile.as_deref(),
                    };
                    // The judge is tool-less by design — no tree mounted; it
                    // rules on the record the prompt carries.
                    let wd = if seat == Seat::Judge {
                        None
                    } else {
                        workdir.clone()
                    };
                    dispatch_case(
                        prompt,
                        &format!("{}-{}", c.id, seat.label()),
                        wd,
                        seat.role_id(),
                        profile,
                        &opts,
                    )
                })
                .with_context(|| format!("debating case {}", c.id))?;
            // One meta row per case: the prosecutor's model (debug phase: all
            // seats share it) + the debate's TOTAL token cost across seats —
            // tokens_to_solution stays the honest full price of the review.
            let seat_tokens = |r: &Option<super::dialectic::SeatRecord>| {
                r.as_ref().and_then(|s| s.total_tokens)
            };
            let mut total = debate.prosecutor.total_tokens;
            for t in [seat_tokens(&debate.defender), seat_tokens(&debate.judge)] {
                total = match (total, t) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, None) => a,
                    (None, b) => b,
                };
            }
            meta.push(EnvelopeMeta {
                model: debate.prosecutor.model.clone(),
                total_tokens: total,
            });
            debates.push(debate);
            review
        } else {
            let prompt = build_prompt(c, opts.mode);
            let workdir = (opts.mode == BenchMode::Agentic)
                .then(|| workdir_for(&c.id))
                .flatten();
            let stdout = dispatch_case(
                &prompt,
                &c.id,
                workdir,
                opts.mode.role_id(),
                None,
                &opts,
            )
            .with_context(|| format!("dispatching case {}", c.id))?;
            meta.push(envelope_meta(&stdout));
            let reply = extract_reply_text(&stdout);
            match opts.mode {
                BenchMode::Strict => parse_review(&reply),
                _ => parse_freeform_review(&reply),
            }
        };
        let s = score(&c.label, &review);
        println!(
            "{:<34}{:<7}{:<7}{:<4}{}",
            c.id,
            c.label.kind,
            if s.degenerate { "—" } else { s.verdict.as_str() },
            s.findings,
            describe(&c.label, &s),
        );
        if let (BenchMode::Dialectic, Some(d)) = (opts.mode, debates.last()) {
            println!(
                "{:<34}↳ charges {} (struck {}) · answers {} (voided {}) · sustained {}{}",
                "",
                d.charges.len(),
                d.struck_charges,
                d.rebuttals.len(),
                d.voided_rebuttals,
                d.sustained,
                if d.defense_degenerate {
                    " · DEFENSE DEGENERATE"
                } else {
                    ""
                },
            );
        }
        scored.push((c, s));
    }
    print_summary(&scored, &opts);
    // (#1198) Persist the run as a scores.json artifact — the bench suite's
    // dual-key score substrate (#1197). Failure to persist is a WARNING, not
    // a bench failure: the operator already has the stdout report.
    match write_scores_artifact(&scored, &meta, &debates, &opts) {
        Ok(path) => eprintln!("scores: {}", path.display()),
        Err(e) => eprintln!("scores: WARNING — artifact not written: {e:#}"),
    }
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
/// workflow: the author's intent (title + description) then the diff. The
/// role-contract line differs by mode: Strict points at the JSON object the
/// grammar-locked role emits; FreeForm points at nothing (the contract —
/// `MUST FIX:`/`CONSIDER:` marker lines in prose — lives entirely in the
/// `pr-reviewer-freeform` system prompt, not restated here).
fn build_prompt(c: &Case, mode: BenchMode) -> String {
    let body = if c.label.intent_body.trim().is_empty() {
        "(no description provided)"
    } else {
        c.label.intent_body.trim()
    };
    let instruction = match mode {
        BenchMode::Strict => "Respond per your role contract (the single JSON object).",
        BenchMode::FreeForm | BenchMode::Agentic => "Write your review per your role contract.",
        // Dialectic never reaches build_prompt — the per-case loop hands the
        // case to `lab::dialectic::run_debate`, which builds per-seat prompts.
        BenchMode::Dialectic => unreachable!("dialectic builds per-seat prompts"),
    };
    // The evidence sentence is the mode's load-bearing difference: the
    // diff-only modes must SAY the diff is all there is (an honest reviewer
    // hedges unverifiable hypotheses), and the agentic mode must say the
    // opposite — telling a tool-wearing model it "has only this diff" would
    // fight its role's explore-before-concluding directives.
    let evidence = match mode {
        BenchMode::Strict | BenchMode::FreeForm => {
            "The full diff is below; you have only this diff."
        }
        BenchMode::Agentic => {
            "The full diff is below, and the repository at the reviewed commit is \
             checked out in your working directory — read the surrounding code to \
             verify your hypotheses before concluding."
        }
        BenchMode::Dialectic => unreachable!("dialectic builds per-seat prompts"),
    };
    format!(
        "PR review request: {title}\n\n\
         Description (the author's stated intent for this change):\n{body}\n\n\
         {instruction} Assess the diff \
         against the stated intent above. {evidence}\n\n\
         ```diff\n{diff}\n```\n",
        title = c.label.intent_title,
        body = body,
        instruction = instruction,
        evidence = evidence,
        diff = c.diff,
    )
}

/// Dispatch one case (or one dialectic seat) through `role_id` on the
/// internal runtime, returning the raw `--json` envelope stdout (parsed by
/// `extract_reply_text`). `profile` overrides `opts.profile_name` for this
/// dispatch — the per-seat profile hook (#1222).
fn dispatch_case(
    prompt: &str,
    case_id: &str,
    workdir: Option<PathBuf>,
    role_id: &str,
    profile: Option<&str>,
    opts: &ReviewBenchOpts,
) -> Result<String> {
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
        role_id: role_id.to_string(),
        message: prompt.to_string(),
        deliver: None,
        session_id: Some(session_id),
        timeout_seconds: opts.timeout_seconds,
        skip_preflight: false,
        json: true,
        watch_paths: Vec::new(),
        // Agentic mode mounts the case's repo tree at /workspace; diff-only
        // modes dispatch with no workdir (a fresh tempdir).
        workdir,
        sprint_id: None,
        runtime: Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
        // pr-review is single-turn; no compaction to override.
        compaction: CompactionDispatchArgs::default(),
        profile_name: profile
            .map(str::to_string)
            .or_else(|| opts.profile_name.clone()),
        config_path: opts.config_path.clone(),
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        max_completion_tokens: None,
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

/// Parse a `pr-reviewer-freeform` reply (ordinary prose, no JSON) into the
/// same `Review` shape `parse_review` produces, so scoring is identical
/// either way. Scans for `MUST FIX:` (⇒ severity high, contributes to
/// `verdict: flag`) and `CONSIDER:` (⇒ severity medium, non-blocking) marker
/// lines — tolerating a leading bullet (`-`, `*`, `•`) and markdown bold
/// (`**MUST FIX:**`). Text following a marker, up to the next marker or a
/// blank line, is folded into that finding's `anchor`/`title` (both set to
/// the same text — the free-form finding has no separate quote-the-line
/// field, so `finding_matches_expected`'s anchor-or-title haystack search
/// still works unchanged). A review with no marker lines and no other real
/// content is `parsed: false` (degenerate, e.g. a genuinely empty reply) —
/// non-empty prose with zero markers is a real, scoreable "no issues" pass.
pub fn parse_freeform_review(text: &str) -> Review {
    let mut findings: Vec<Finding> = Vec::new();
    let mut current: Option<(String, String)> = None; // (severity, accumulated text)
    let flush = |current: &mut Option<(String, String)>, findings: &mut Vec<Finding>| {
        if let Some((severity, body)) = current.take() {
            let body = body.trim().to_string();
            if !body.is_empty() {
                findings.push(Finding {
                    severity,
                    anchor: body.clone(),
                    title: body,
                });
            }
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = strip_marker(line, "MUST FIX") {
            flush(&mut current, &mut findings);
            current = Some(("high".to_string(), rest));
        } else if let Some(rest) = strip_marker(line, "CONSIDER") {
            flush(&mut current, &mut findings);
            current = Some(("medium".to_string(), rest));
        } else if line.is_empty() {
            flush(&mut current, &mut findings);
        } else if let Some((_, body)) = current.as_mut() {
            body.push(' ');
            body.push_str(line);
        }
    }
    flush(&mut current, &mut findings);
    let verdict = if findings.iter().any(|f| f.severity == "high") {
        "flag"
    } else {
        "pass"
    };
    Review {
        verdict: verdict.to_string(),
        findings,
        parsed: !text.trim().is_empty(),
    }
}

/// If `line` (after stripping leading bullet/markdown-bold decoration) starts
/// with `marker` case-insensitively, return the remainder — decoration and an
/// optional `:` separator stripped, trimmed. Else `None`. Uses `str::get`
/// (not direct byte slicing) so arbitrary UTF-8 prose that happens not to
/// start with the marker can never panic on a non-char-boundary index.
fn strip_marker(line: &str, marker: &str) -> Option<String> {
    let is_deco = |c: char| matches!(c, '-' | '*' | '•' | '_') || c.is_whitespace();
    let s = line.trim_start_matches(is_deco);
    let prefix = s.get(..marker.len())?;
    if !prefix.eq_ignore_ascii_case(marker) {
        return None;
    }
    let rest = s[marker.len()..]
        .trim_start_matches(is_deco)
        .trim_start_matches(':')
        .trim_start_matches(is_deco);
    Some(rest.trim().to_string())
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

// ─── #1198: scores.json emission ────────────────────────────────────────

/// Per-dispatch metadata pulled from the `--json` envelope (best-effort —
/// the score math never depends on it, only the artifact's provenance).
#[derive(Debug, Default, Clone)]
pub(crate) struct EnvelopeMeta {
    pub model: Option<String>,
    pub total_tokens: Option<u64>,
}

/// Parse the dispatch envelope (the last stdout line starting with `{` —
/// tolerant of pull-progress noise ahead of it, unlike `extract_reply_text`
/// which parses the whole stdout as one JSON value) for `metrics.model` +
/// token totals. The envelope is compact single-line JSON, so a reply-
/// internal brace can never appear as its own stdout line.
pub(crate) fn envelope_meta(stdout: &str) -> EnvelopeMeta {
    let candidate = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or(stdout);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate.trim()) else {
        return EnvelopeMeta::default();
    };
    let m = v.get("metrics").cloned().unwrap_or(serde_json::Value::Null);
    let total = m
        .get("total_tokens")
        .and_then(|t| t.as_u64())
        .or_else(|| {
            let p = m.get("prompt_tokens").and_then(|t| t.as_u64());
            let c = m.get("completion_tokens").and_then(|t| t.as_u64());
            match (p, c) {
                (Some(p), Some(c)) => Some(p + c),
                _ => None,
            }
        });
    EnvelopeMeta {
        model: m.get("model").and_then(|s| s.as_str()).map(str::to_string),
        total_tokens: total,
    }
}

/// Build the run's score rows: one capability row per case, plus the
/// corpus-wide aggregates the summary prints (recall / precision / anchor
/// rate on the multi-finding schema; clean-pass + legacy recall otherwise).
/// Pure — unit-testable without dispatching.
pub(crate) fn build_score_rows(
    scored: &[(&Case, CaseScore)],
    meta: &[EnvelopeMeta],
    artifact: &super::scores::ArtifactKey,
) -> Vec<super::scores::ScoreRow> {
    use super::scores::{Outcome, ScoreFamily, ScoreRow};
    let row = |axis: &str, outcome: Outcome, value: Option<f64>, detail: serde_json::Value| ScoreRow {
        bench: "review-bench".into(),
        bench_version: "1".into(),
        source: "native".into(),
        family: ScoreFamily::Capability,
        axis: axis.to_string(),
        artifact: artifact.clone(),
        outcome,
        value,
        trial: 0,
        k: 1,
        tokens_to_solution: None,
        seconds_per_token: None,
        budget_turns: None,
        budget_tokens: None,
        detail,
    };

    let mut rows = Vec::new();
    for (i, (c, s)) in scored.iter().enumerate() {
        // A degenerate review (empty/unparseable) is a CAPABILITY failure —
        // the dispatch ran; the model failed the contract. (A dispatch that
        // never ran bails the bench before scoring, so no infra rows here
        // yet — per-case infra tolerance is #1199-adjacent follow-up.)
        let outcome = if s.degenerate || !s.correct {
            Outcome::CapabilityFail
        } else {
            Outcome::Pass
        };
        let mut r = row(
            "case",
            outcome,
            Some(if s.correct { 1.0 } else { 0.0 }),
            serde_json::json!({ "case": c.id, "kind": c.label.kind, "score": s }),
        );
        r.tokens_to_solution = meta.get(i).and_then(|m| m.total_tokens);
        rows.push(r);
    }

    let clean: Vec<&CaseScore> = scored
        .iter()
        .filter(|(c, _)| c.label.kind == "clean")
        .map(|(_, s)| s)
        .collect();
    let clean_pass = clean.iter().filter(|s| s.correct).count();
    // Clean-only fp here so this row's detail agrees with the printed
    // "clean: X/Y pass · Z false positives" line (review-QA on #1200);
    // corpus-wide fp lives on the `precision` row.
    let clean_fp: usize = clean.iter().map(|s| s.fp).sum();
    let fp_total: usize = scored.iter().map(|(_, s)| s.fp).sum();
    let frac = |num: usize, den: usize| -> Option<f64> {
        (den > 0).then(|| num as f64 / den as f64)
    };
    rows.push(row(
        "clean_pass_rate",
        Outcome::NotApplicable,
        frac(clean_pass, clean.len()),
        serde_json::json!({ "passed": clean_pass, "clean_cases": clean.len(), "false_positives": clean_fp }),
    ));

    if scored.iter().any(|(c, _)| c.label.uses_multi()) {
        let expected_bugs: usize = scored.iter().map(|(_, s)| s.expected_bugs).sum();
        let bugs_caught: usize = scored.iter().map(|(_, s)| s.bugs_caught).sum();
        let tp: usize = scored.iter().map(|(_, s)| s.tp).sum();
        let total_findings = tp + fp_total;
        let anchors_ok: usize = scored.iter().map(|(_, s)| s.anchors_ok).sum();
        rows.push(row(
            "recall",
            Outcome::NotApplicable,
            frac(bugs_caught, expected_bugs),
            serde_json::json!({ "caught": bugs_caught, "expected": expected_bugs }),
        ));
        rows.push(row(
            "precision",
            Outcome::NotApplicable,
            frac(tp, total_findings),
            serde_json::json!({ "tp": tp, "findings": total_findings }),
        ));
        rows.push(row(
            "anchor_rate",
            Outcome::NotApplicable,
            frac(anchors_ok, bugs_caught),
            serde_json::json!({ "anchored": anchors_ok, "caught": bugs_caught }),
        ));
    }
    rows
}

/// Assemble + write the artifact; returns the written path. Dialectic mode's
/// debate envelopes (#1222) are written FIRST, beside scores.json, and
/// independently of the scores write succeeding — the debate record is the
/// higher-value audit artifact (every sustained finding's evidence chain)
/// and must never be dropped because the scores serialization failed.
fn write_scores_artifact(
    scored: &[(&Case, CaseScore)],
    meta: &[EnvelopeMeta],
    debates: &[super::dialectic::DebateEnvelope],
    opts: &ReviewBenchOpts,
) -> Result<PathBuf> {
    use super::scores;
    // The artifact key's model: what the envelopes reported (all cases share
    // one dispatch config; take the first reported value).
    let model = meta
        .iter()
        .find_map(|m| m.model.clone())
        .unwrap_or_else(|| "(unknown)".to_string());
    let artifact = scores::ArtifactKey {
        model,
        quant: None,
        backend: None,
        n_ctx: None,
    };
    let rows = build_score_rows(scored, meta, &artifact);
    // Millis granularity so two runs in the same second can't collide on
    // the run id / default dir (review-QA on #1200); ts is RFC3339 per the
    // schema's contract.
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let machine_id = darkmux_types::config_access::machine_id()
        .unwrap_or_else(|| "(unknown)".to_string());
    let mut doc = scores::ScoresDoc::new(
        scores::RunProvenance {
            run_id: format!("review-bench-{ts_ms}"),
            ts: darkmux_flow::ts_utc_now(),
            machine: scores::MachineFingerprint::detect(&machine_id),
            profile: opts.profile_name.clone(),
            ..Default::default()
        },
        rows,
    );
    // The contract is a HARNESS variable (#1119 free-form mode): a freeform
    // row and a strict row of the same artifact are different cells and must
    // never aggregate together. Recorded doc-level — every row in one run
    // shares one mode.
    doc.extras.insert(
        "mode".to_string(),
        serde_json::Value::String(opts.mode.label().to_string()),
    );
    let path = match &opts.scores_out {
        Some(p) => p.clone(),
        None => super::paths::resolve(super::paths::ResolveScope::Auto)
            .runs
            .join(format!("review-bench-{ts_ms}"))
            .join("scores.json"),
    };
    if !debates.is_empty() {
        let dpath = path.with_file_name("debates.json");
        let write = serde_json::to_vec_pretty(debates)
            .map_err(anyhow::Error::from)
            .and_then(|b| {
                if let Some(parent) = dpath.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dpath, b).map_err(anyhow::Error::from)
            });
        match write {
            Ok(()) => eprintln!("debates: {}", dpath.display()),
            Err(e) => eprintln!("debates: WARNING — artifact not written: {e:#}"),
        }
    }
    scores::write_scores(&path, &doc)?;
    Ok(path)
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

    // ── agentic mode (#1197 anchor experiment) ──

    fn tiny_case() -> Case {
        Case {
            id: "c1".into(),
            label: Label {
                kind: "bug".into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: "flag".into(),
                bug_class: None,
                anchor_contains: None,
                expected: Vec::new(),
                notes: None,
            },
            diff: "+ x".into(),
        }
    }

    /// The evidence sentence is mode-load-bearing: diff-only modes must SAY
    /// the diff is all there is; agentic mode must say the repo is checked
    /// out (telling a tool-wearing model it "has only this diff" would fight
    /// its explore-before-concluding role directives — and the reverse claim
    /// on a tool-less model invites fabricated "I read the file" prose).
    #[test]
    fn build_prompt_evidence_sentence_matches_mode() {
        let c = tiny_case();
        for mode in [BenchMode::Strict, BenchMode::FreeForm] {
            let p = build_prompt(&c, mode);
            assert!(
                p.contains("you have only this diff"),
                "{mode:?} must declare diff-only evidence"
            );
            assert!(!p.contains("checked out in your working directory"));
        }
        let p = build_prompt(&c, BenchMode::Agentic);
        assert!(p.contains("checked out in your working directory"));
        assert!(
            !p.contains("you have only this diff"),
            "agentic prompt must not contradict the mounted repo"
        );
    }

    #[test]
    fn bench_mode_role_and_label_mapping() {
        assert_eq!(BenchMode::Strict.role_id(), "pr-reviewer");
        assert_eq!(BenchMode::FreeForm.role_id(), "pr-reviewer-freeform");
        assert_eq!(BenchMode::Agentic.role_id(), "pr-reviewer-agentic");
        assert_eq!(BenchMode::Agentic.label(), "agentic");
    }

    /// The agentic role's marker dialect (`MUST FIX [path] `anchor``) parses
    /// through the same freeform parser — the bracket form has no colon, and
    /// `strip_marker` tolerates that. Pins the dialect compatibility the
    /// agentic mode depends on.
    #[test]
    fn freeform_parser_accepts_agentic_bracket_dialect() {
        let r = parse_freeform_review(
            "Traced the change.\n\n\
             MUST FIX [app/models/billing.ts] `billingEndAt.plus({ days: 1 })`\n\
             The boundary is off by one on the last day of the cycle.\n\n\
             VERDICT: flag",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].anchor.contains("billingEndAt.plus"));
    }

    // ── free-form parsing (#1119 free-form mode) ──

    #[test]
    fn freeform_no_markers_is_a_real_pass_not_degenerate() {
        let r = parse_freeform_review(
            "I traced the changed function end to end. The guard correctly \
             handles the null case the PR description mentions. This looks sound.",
        );
        assert!(r.parsed, "non-empty prose with no markers is a real pass, not degenerate");
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn freeform_empty_text_is_degenerate() {
        assert!(!parse_freeform_review("").parsed);
        assert!(!parse_freeform_review("   \n  ").parsed);
    }

    #[test]
    fn freeform_must_fix_line_is_high_and_flags() {
        let r = parse_freeform_review(
            "I looked through the diff.\n\n\
             MUST FIX: the call to .startOf('day') is not applied to both sides of \
             the range comparison in calc.ts:40, so results are undercounted near a \
             day boundary.\n\n\
             The rest of the change looks fine.",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "high");
        assert!(r.findings[0].title.contains("startOf('day')"));
        assert!(r.findings[0].title.contains("calc.ts:40"));
    }

    #[test]
    fn freeform_consider_line_is_medium_and_does_not_flag() {
        let r = parse_freeform_review("CONSIDER: adding a test for the empty-array case.");
        assert_eq!(r.verdict, "pass", "CONSIDER alone must not flag (mirrors severity=medium)");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "medium");
    }

    #[test]
    fn freeform_multiple_markers_each_become_a_finding() {
        let r = parse_freeform_review(
            "MUST FIX: bug one in foo.rs:1.\n\
             CONSIDER: a style nit in foo.rs:9.\n\
             MUST FIX: bug two in bar.rs:5.",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 3);
        assert_eq!(r.findings.iter().filter(|f| f.severity == "high").count(), 2);
        assert_eq!(r.findings.iter().filter(|f| f.severity == "medium").count(), 1);
    }

    #[test]
    fn freeform_tolerates_bullets_and_bold_markdown() {
        let cases = [
            "- MUST FIX: a bug",
            "* MUST FIX: a bug",
            "**MUST FIX:** a bug",
            "- **MUST FIX:** a bug",
        ];
        for c in cases {
            let r = parse_freeform_review(c);
            assert_eq!(r.findings.len(), 1, "failed on {c:?}");
            assert_eq!(r.findings[0].severity, "high", "failed on {c:?}");
            assert!(r.findings[0].title.contains("a bug"), "failed on {c:?}: {:?}", r.findings[0].title);
        }
    }

    #[test]
    fn freeform_continuation_lines_fold_into_the_finding() {
        let r = parse_freeform_review(
            "MUST FIX: the totals are wrong.\n\
             This is because dailyComplexCharges is never summed alongside\n\
             sumComplexCharges, so the dashboard undercounts.\n\n\
             CONSIDER: a follow-up test.",
        );
        assert_eq!(r.findings.len(), 2);
        assert!(r.findings[0].title.contains("dailyComplexCharges"));
        assert!(r.findings[0].title.contains("sumComplexCharges"));
    }

    #[test]
    fn freeform_non_ascii_prose_near_a_would_be_marker_does_not_panic() {
        // A line starting with multi-byte UTF-8 must never panic strip_marker's
        // byte slicing (str::get, not direct indexing) even when it's short or
        // lands mid-character relative to the marker's byte length.
        let r = parse_freeform_review("😀 the diff looks fine, no MUST FIX here.\n中文 CONSIDER test\n短");
        assert!(r.parsed);
        assert!(r.findings.is_empty(), "marker mid-line without a line-start match must not register");
    }

    #[test]
    fn freeform_scores_through_the_same_multi_finding_matcher() {
        // Proves the free-form path is a drop-in for the existing scorer: the
        // same expected-finding schema, matched via anchor/title substring.
        let label = multi_lbl("bug", vec![ef("calc.ts:40", false)]);
        let r = parse_freeform_review(
            "MUST FIX: calc.ts:40 undercounts near a day boundary because the \
             range comparison isn't day-aligned.",
        );
        let s = score(&label, &r);
        assert!(s.recall);
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 0);
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

    // ─── #1198: scores.json emission ────────────────────────────────

    #[test]
    fn envelope_meta_extracts_model_and_tokens() {
        let stdout = "pulling image...\n{\"result\":\"stop\",\"metrics\":{\"model\":\"m-x\",\"prompt_tokens\":100,\"completion_tokens\":25}}";
        let m = envelope_meta(stdout);
        assert_eq!(m.model.as_deref(), Some("m-x"));
        assert_eq!(m.total_tokens, Some(125));
        // Garbage stdout degrades to None, never errors.
        let g = envelope_meta("not json at all");
        assert!(g.model.is_none() && g.total_tokens.is_none());
    }

    #[test]
    fn build_score_rows_maps_outcomes_and_aggregates() {
        use crate::lab::scores::{ArtifactKey, Outcome};
        let mk_case = |id: &str, kind: &str| Case {
            id: id.into(),
            label: Label {
                kind: kind.into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![],
                notes: None,
            },
            diff: String::new(),
        };
        let clean_case = mk_case("c1", "clean");
        let bug_case = mk_case("b1", "bug");
        let clean_pass = CaseScore {
            correct: true,
            verdict: "pass".into(),
            ..Default::default()
        };
        let bug_degenerate = CaseScore {
            degenerate: true,
            ..Default::default()
        };
        let scored: Vec<(&Case, CaseScore)> =
            vec![(&clean_case, clean_pass), (&bug_case, bug_degenerate)];
        let meta = vec![
            EnvelopeMeta {
                model: Some("m-x".into()),
                total_tokens: Some(500),
            },
            EnvelopeMeta::default(),
        ];
        let artifact = ArtifactKey {
            model: "m-x".into(),
            ..Default::default()
        };
        let rows = build_score_rows(&scored, &meta, &artifact);

        // Two per-case rows + the clean_pass_rate aggregate (no multi schema).
        let case_rows: Vec<_> = rows.iter().filter(|r| r.axis == "case").collect();
        assert_eq!(case_rows.len(), 2);
        assert_eq!(case_rows[0].outcome, Outcome::Pass);
        assert_eq!(case_rows[0].tokens_to_solution, Some(500));
        // A degenerate review is a CAPABILITY failure (the dispatch ran).
        assert_eq!(case_rows[1].outcome, Outcome::CapabilityFail);
        let agg = rows.iter().find(|r| r.axis == "clean_pass_rate").unwrap();
        assert_eq!(agg.value, Some(1.0));
        assert!(
            !rows.iter().any(|r| r.axis == "recall"),
            "multi-schema aggregates only appear when the corpus uses them"
        );
        // Every row carries the artifact key + native source.
        assert!(rows.iter().all(|r| r.artifact.model == "m-x" && r.source == "native"));
    }

    #[test]
    fn build_score_rows_emits_multi_schema_aggregates() {
        use crate::lab::scores::ArtifactKey;
        let bug_case = Case {
            id: "b1".into(),
            label: Label {
                kind: "bug".into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![serde_json::from_str::<ExpectedFinding>(
                    r#"{"anchor_contains":"x"}"#,
                )
                .unwrap()],
                notes: None,
            },
            diff: String::new(),
        };
        let s = CaseScore {
            expected_bugs: 2,
            bugs_caught: 1,
            tp: 1,
            fp: 1,
            anchors_ok: 1,
            ..Default::default()
        };
        let scored: Vec<(&Case, CaseScore)> = vec![(&bug_case, s)];
        let rows = build_score_rows(
            &scored,
            &[EnvelopeMeta::default()],
            &ArtifactKey {
                model: "m".into(),
                ..Default::default()
            },
        );
        let recall = rows.iter().find(|r| r.axis == "recall").unwrap();
        assert_eq!(recall.value, Some(0.5));
        let precision = rows.iter().find(|r| r.axis == "precision").unwrap();
        assert_eq!(precision.value, Some(0.5)); // 1 tp / (1 tp + 1 fp)
        let anchor = rows.iter().find(|r| r.axis == "anchor_rate").unwrap();
        assert_eq!(anchor.value, Some(1.0));
    }
}
