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
//! same substrate as `darkmux dispatch pr-reviewer` — so a bench run and a
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
    /// (#1222 Phase B packet 7) The review funnel as a bench condition — the
    /// release-guard validation mode: bundles → probe seats ×k draws → dedup
    /// → double-confirm judge (`lab::review::run_review`, fed real bundles
    /// via `ReviewInputs::bundles`), over the SAME labeled corpus every
    /// other mode scores against. `Confirmed`
    /// tier flags become the review's findings (`NeedsCheck`/`Archived`
    /// don't count toward recall/precision, but are recorded in the
    /// per-case `funnels.json` artifact) — scoring is entirely downstream of
    /// the funnel, same discipline as `Dialectic`.
    Funnel,
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
            // The funnel dispatches `review-probe`/`review-judge` seats
            // directly via `single_shot_chat` (no container dispatch, no
            // single role id) — the per-case loop branches to
            // `run_funnel_case` before ever asking the mode for one.
            BenchMode::Funnel => unreachable!("funnel dispatches review-probe/review-judge seats"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            BenchMode::Strict => "strict",
            BenchMode::FreeForm => "freeform",
            BenchMode::Agentic => "agentic",
            BenchMode::Dialectic => "dialectic",
            BenchMode::Funnel => "funnel",
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
    /// (#1222 Phase B packet 7) `Funnel` mode's crew name — `crews.<name>`
    /// in the profile registry, naming the `review-probe`/`review-judge`
    /// seat staffing `lab::review::validate_review_crew` requires. Required
    /// when `mode == Funnel` (checked at bench start, before any dispatch).
    pub crew: Option<String>,
    /// (#1222) `Funnel` mode's model-cycling mode override —
    /// `"sequential"` | `"parallel"` | `"auto"` (default: `auto`, resolved
    /// once against the local hardware tier — see `review::resolve_mode`).
    pub exec_mode: Option<String>,
    /// (#1222) `Funnel` mode's override for every `review-probe` staffing's
    /// draw count `k` — the crew registry's per-staffing `k` applies
    /// unchanged when `None`.
    pub k_override: Option<u32>,
    /// (#1222) `Funnel` mode's external bundler command
    /// (`<cmd> --worktree <dir> --diff <file>`, `lab::bundle::external_bundles`'s
    /// contract) — the built-in Rust bundler (`lab::bundle::build_bundles`)
    /// runs when `None`.
    pub bundler_cmd: Option<String>,
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
    if matches!(opts.mode, BenchMode::Agentic | BenchMode::Dialectic | BenchMode::Funnel) {
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
            "--workdirs only applies to --agentic / --dialectic / --funnel (diff-only \
             modes never read a repo tree; passing one would silently measure nothing)"
        ));
    }
    // (#1222 Phase B packet 7) Funnel mode's preflight: resolve the crew +
    // seat prompts + exec mode ONCE, before any dispatch spends a token —
    // same "fail loud before spending tokens" discipline as the workdirs
    // check above.
    let funnel_ctx: Option<FunnelCtx> = if opts.mode == BenchMode::Funnel {
        Some(resolve_funnel_ctx(&opts)?)
    } else {
        None
    };
    // (#1198 + #1247 Part 2) Resolve the scores artifact's path + the run's
    // ts_ms ONCE, up front — both the per-case incremental funnels.json
    // snapshots below AND the end-of-run `write_scores_artifact` write to
    // the SAME path, and `RunProvenance::run_id` shares the same ts_ms, so
    // resolving early (rather than recomputing at end-of-run, the prior
    // behavior) keeps that coupling intact while additionally letting the
    // run_id reflect when the run STARTED, not when it finished.
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let scores_path = opts.scores_out.clone().unwrap_or_else(|| {
        super::paths::resolve(super::paths::ResolveScope::Auto)
            .runs
            .join(format!("review-bench-{ts_ms}"))
            .join("scores.json")
    });
    // (#1247 Part 1) Funnel mode's per-run-local flow-event sink — see
    // `LocalJsonlEmitter`'s doc for why this is NOT the fleet flow stream.
    let mut funnel_emitter = LocalJsonlEmitter::new(scores_path.with_file_name("funnel-events.jsonl"));
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
    // (#1222 Phase B packet 7) Funnel mode's composite artifacts: one
    // envelope per case, written beside scores.json — same discipline as
    // `debates` above.
    let mut funnels: Vec<super::review::ReviewEnvelope> = Vec::new();
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
        } else if opts.mode == BenchMode::Funnel {
            let ctx = funnel_ctx
                .as_ref()
                .expect("preflight resolves the funnel context before the loop");
            let workdir = workdir_for(&c.id)
                .expect("--funnel preflight requires --workdirs and validates every case's tree");
            let (review, env) =
                run_funnel_case(c, &workdir, ctx, opts.timeout_seconds, &mut funnel_emitter)
                    .with_context(|| format!("funneling case {}", c.id))?;
            let total_tokens: u64 = env.members.iter().map(|m| m.total_tokens).sum();
            meta.push(EnvelopeMeta {
                model: env.members.first().map(|m| m.model.clone()),
                total_tokens: Some(total_tokens),
            });
            funnels.push(env);
            // (#1247 Part 2) Stream funnels.json to disk AS THIS CASE
            // COMPLETES — a killed run (crash, timeout, ctrl-C) keeps every
            // completed case's envelope, not just whatever the last write
            // captured. Best-effort: a snapshot failure is a warning, never
            // a bench failure — the operator still gets the console report
            // and (if the process survives to the end) the final write.
            if let Err(e) = write_funnels_snapshot(&scores_path, &funnels) {
                eprintln!("funnels: WARNING — incremental snapshot not written: {e:#}");
            }
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
        if let (BenchMode::Funnel, Some(env)) = (opts.mode, funnels.last()) {
            println!(
                "{:<34}↳ bundles {} · flags {}→{} · confirmed {} · needs_check {} · archived {}",
                "",
                env.bundles,
                env.raw_flags,
                env.deduped_flags,
                env.confirmed,
                env.needs_check,
                env.archived,
            );
        }
        scored.push((c, s));
    }
    print_summary(&scored, &meta, &opts);
    // (#1198) Persist the run as a scores.json artifact — the bench suite's
    // dual-key score substrate (#1197). Failure to persist is a WARNING, not
    // a bench failure: the operator already has the stdout report.
    match write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_path, ts_ms) {
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
        // Funnel never reaches build_prompt either — the per-case loop hands
        // the case to `run_funnel_case`, which builds the probe/judge prompts.
        BenchMode::Funnel => unreachable!("funnel builds probe/judge prompts"),
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
        BenchMode::Funnel => unreachable!("funnel builds probe/judge prompts"),
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
        // Agentic mode mounts the case's repo tree at /workspace; diff-only
        // modes dispatch with no workdir (a fresh tempdir).
        workdir,
        phase_id: None,
        runtime: Runtime::Internal,
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
        model_base_url_override: None,
    };
    let r = dispatch(d).context("pr-review-bench internal-runtime dispatch")?;
    Ok(r.stdout)
}

// ─── funnel mode (#1222 Phase B packet 7) ──────────────────────────────

/// `Funnel` mode's resolved, per-run context — computed ONCE in
/// [`resolve_funnel_ctx`] before the per-case loop (fail loud before any
/// dispatch spends a token, same discipline as the `--workdirs` preflight).
struct FunnelCtx {
    crew: darkmux_profiles::crews::ResolvedCrew,
    exec_mode: super::review::ExecMode,
    probe_system: String,
    judge_system: String,
    /// (#1260) The verify seat's persona — resolved unconditionally (the
    /// role is embedded); only dispatched when the crew declares the seat.
    verify_system: String,
    bundler_cmd: Option<String>,
}

/// Parse `--exec-mode`'s string value into `review::ExecMode`. `None` (the
/// flag omitted) and the literal `"auto"` both resolve to `Auto` — the
/// funnel's own `resolve_mode` then decides `Sequential` vs `Parallel`
/// against the local hardware tier at run time.
fn parse_exec_mode(s: Option<&str>) -> Result<super::review::ExecMode> {
    match s.map(str::to_ascii_lowercase).as_deref() {
        None | Some("auto") => Ok(super::review::ExecMode::Auto),
        Some("sequential") => Ok(super::review::ExecMode::Sequential),
        Some("parallel") => Ok(super::review::ExecMode::Parallel),
        Some(other) => Err(anyhow!(
            "--exec-mode must be \"sequential\", \"parallel\", or \"auto\" (got \"{other}\")"
        )),
    }
}

/// Resolve `opts` into a [`FunnelCtx`]: load the profile registry, resolve
/// `--crew` against it (`darkmux_profiles::crews::resolve_crew` — the same
/// validation `dispatch` would apply), validate it carries the
/// funnel's own seat requirements (`review::validate_review_crew` — the
/// SAME check `run_review` runs internally, called here too so a
/// misconfigured crew fails at bench START, not at the first case's
/// dispatch), apply `--k` as an override on every `review-probe` staffing's
/// draw count, parse `--exec-mode`, and resolve the `review-probe`/
/// `review-judge` seat system prompts. Every failure here is loud and
/// happens BEFORE any dispatch — a misconfigured crew or a missing seat
/// prompt must never silently corrupt a bench run.
fn resolve_funnel_ctx(opts: &ReviewBenchOpts) -> Result<FunnelCtx> {
    let crew_name = opts.crew.as_deref().ok_or_else(|| {
        anyhow!(
            "--funnel requires --crew <name> (the crews.<name> entry naming the \
             review-probe/review-judge seat staffing)"
        )
    })?;
    let loaded = darkmux_profiles::profiles::load_registry(opts.config_path.as_deref())
        .context("loading profile registry for --funnel")?;
    let mut crew = darkmux_profiles::crews::resolve_crew(&loaded.registry, crew_name)
        .with_context(|| format!("resolving crew \"{crew_name}\" for --funnel"))?;
    super::review::validate_review_crew(&crew)
        .with_context(|| format!("crew \"{crew_name}\" for --funnel"))?;
    if let Some(k) = opts.k_override {
        if let Some(staffings) = crew.seats.get_mut("review-probe") {
            for s in staffings.iter_mut() {
                s.k = k;
            }
        }
    }
    let exec_mode = parse_exec_mode(opts.exec_mode.as_deref())?;
    let probe_system = darkmux_crew::loader::role_prompt("review-probe").ok_or_else(|| {
        anyhow!("darkmux: role \"review-probe\" has no system prompt (missing review-probe.md)")
    })?;
    let judge_system = darkmux_crew::loader::role_prompt("review-judge").ok_or_else(|| {
        anyhow!("darkmux: role \"review-judge\" has no system prompt (missing review-judge.md)")
    })?;
    let verify_system = darkmux_crew::loader::role_prompt("review-verify").ok_or_else(|| {
        anyhow!("darkmux: role \"review-verify\" has no system prompt (missing review-verify.md)")
    })?;
    Ok(FunnelCtx {
        crew,
        exec_mode,
        probe_system,
        judge_system,
        verify_system,
        bundler_cmd: opts.bundler_cmd.clone(),
    })
}

/// Write `diff` to a uniquely-named temp file — `bundle::external_bundles`
/// needs a diff FILE path (its `<cmd> --worktree <dir> --diff <file>`
/// contract), not diff text. Best-effort; a leftover file on an early
/// return is harmless scratch, not user state.
fn write_temp_diff(case_id: &str, diff: &str) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "darkmux-review-bench-{case_id}-{}-{ts}.diff",
        std::process::id()
    ));
    fs::write(&path, diff).with_context(|| format!("writing temp diff for case {case_id}"))?;
    Ok(path)
}

/// Map a funnel envelope's judged flags into the SAME `Review{verdict,
/// findings}` shape every other mode scores through, so `score()` /
/// `finding_matches_expected` applies UNCHANGED. Only `Tier::Confirmed`
/// flags become findings — `NeedsCheck` is the non-blocking tier (recorded
/// in the envelope artifact, never counted toward recall/precision) and
/// `Archived` is a ruled-out flag. `anchor` is the flag's own anchor (empty
/// when dedup found none); `title` folds the judge's `note_for_author`
/// (author-facing) and `decisive_evidence` (the cited code/claim) together
/// so both are available to the anchor/title substring matcher. A
/// degenerate envelope (zero bundles or zero raw flags) maps to
/// `parsed: false` — scored distinctly from a real pass, same as every
/// other mode's degenerate case.
fn review_from_funnel(env: &super::review::ReviewEnvelope) -> Review {
    let findings: Vec<Finding> = env
        .judged
        .iter()
        .filter(|j| j.tier == super::review::Tier::Confirmed)
        .map(|j| {
            let note = j.pass1.note_for_author.trim();
            let evidence = j.pass1.decisive_evidence.trim();
            let title = match (note.is_empty(), evidence.is_empty()) {
                (false, false) => format!("{note} — {evidence}"),
                (false, true) => note.to_string(),
                (true, false) => evidence.to_string(),
                (true, true) => String::new(),
            };
            Finding {
                severity: "high".to_string(),
                anchor: j.flag.anchor.clone().unwrap_or_default(),
                title,
            }
        })
        .collect();
    Review {
        verdict: if findings.is_empty() { "pass".to_string() } else { "flag".to_string() },
        parsed: env.degenerate.is_none(),
        findings,
    }
}

/// (#1247 Part 1, lab-vs-fleet scope boundary) Per-run-local JSONL sink for
/// the funnel driver's observability records — `review-bench --funnel`'s
/// wiring of `review::ReviewEmitter`. Deliberately NOT `darkmux_flow::record`
/// (the real, engagement-scoped flow stream `darkmux mission launch review`
/// writes through): a bench run dispatches many cases in one process and can emit
/// hundreds of per-flag ruling records, and that volume must never spam an
/// operator's real engagement flow stream — see the "lab vs fleet scope
/// boundary" project memory. `path` is `funnel-events.jsonl`, written beside
/// `funnels.json`/`scores.json`; a future "lab observer view" tails it
/// directly. Every record is the SAME [`darkmux_flow::FlowRecord`] shape the
/// fleet sink writes, so a future consumer renders identical vocabulary
/// regardless of which sink produced it.
///
/// Appends one JSON line per record, holding the file handle open for the
/// emitter's lifetime — the parent dir is created and the file opened ONCE,
/// on the first emit (lazily, not at construction: `run_review_bench`
/// constructs the emitter for every mode but only Funnel mode ever emits,
/// and a strict/freeform run must not leave an empty `funnel-events.jsonl`
/// behind), then each record is one `write` + `flush` — enough for a live
/// `tail -f` to see progress in real time, without the per-record
/// mkdir/open/fsync churn of reopening (~hundreds of records per case;
/// review-round item on the #1247 PR). Best-effort throughout: an open or
/// write failure (disk full, a permissions problem) is swallowed rather
/// than aborting the bench — flow observability must never be the reason a
/// bench run fails. A failed open is not retried (`opened` latches), so a
/// persistently-broken path costs one attempt total, not one per record.
struct LocalJsonlEmitter {
    path: PathBuf,
    file: Option<fs::File>,
    /// Latched after the first emit's open attempt — success or failure.
    opened: bool,
}

impl LocalJsonlEmitter {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None, opened: false }
    }
}

impl super::review::ReviewEmitter for LocalJsonlEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        use std::io::Write;
        if !self.opened {
            self.opened = true;
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            self.file = fs::OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        if writeln!(f, "{line}").is_ok() {
            let _ = f.flush();
        }
    }
}

/// Run the review funnel for one case: build bundles (the built-in Rust
/// bundler, or `--bundler <cmd>` when set) over the case's mounted repo
/// tree, feed them into `review::run_review` via the `ReviewInputs::bundles`
/// injection seam (#1222 Phase B packet 5 reconciliation), wired to
/// `darkmux_crew::single_shot::single_shot_chat` + [`review::LmsCycler`],
/// and map the resulting envelope into a scoreable `Review`. Mirrors
/// `super::dialectic::run_debate`'s split: this function owns dispatch +
/// mapping; the caller owns console output + artifact bookkeeping.
fn run_funnel_case(
    c: &Case,
    workdir: &Path,
    ctx: &FunnelCtx,
    timeout_seconds: u32,
    emitter: &mut dyn super::review::ReviewEmitter,
) -> Result<(Review, super::review::ReviewEnvelope)> {
    use super::bundle;
    use super::review;

    let source = bundle::FileSource::worktree(workdir);
    let set = match ctx.bundler_cmd.as_deref() {
        Some(cmd) => {
            let diff_path = write_temp_diff(&c.id, &c.diff)?;
            let result = bundle::external_bundles(cmd, Some(workdir), &diff_path);
            let _ = fs::remove_file(&diff_path);
            result.with_context(|| format!("external bundler for case {}", c.id))?
        }
        None => bundle::build_bundles(&source, &c.diff)
            .with_context(|| format!("building bundles for case {}", c.id))?,
    };
    let bundles: Vec<review::BundleInput> = set
        .bundles
        .iter()
        .map(|b| -> Result<review::BundleInput> {
            Ok(review::BundleInput {
                id: b.id.clone(),
                fact_family: b.fact_family.clone(),
                // Per-seat code formats (#1256): the judge reads
                // slice_code's `// path` raw format; the probe reads
                // slice_code_probe's Phase A fenced format.
                code: bundle::slice_code(&source, &b.code)?,
                probe_code: bundle::slice_code_probe(&source, &b.code)?,
                facts: b.facts.clone(),
                manifest: b.manifest.clone(),
            })
        })
        .collect::<Result<_>>()
        .with_context(|| format!("slicing bundle code for case {}", c.id))?;

    // (#1355 follow-up) Dispatches through the SAME `build_review_graph` +
    // `run_review_graph` engine `darkmux mission launch review` uses in
    // production — not the old sequential `run_review`/`run_review_impl` driver. Bench
    // was already doing REAL dispatch (its retired `chat` closure was a
    // byte-for-byte duplicate of `dispatch_chat`, per its own "Texts
    // identical either way (contract 6)" comment) — the only thing this
    // migration changes is which orchestration code runs the dispatches,
    // so what bench measures is finally what production actually executes.
    let seats = review::validate_review_crew(&ctx.crew)
        .with_context(|| format!("validating crew for case {}", c.id))?;
    let probes: Vec<_> = seats.probes.clone();
    let judge = seats.judge.clone();
    let verify = seats.verify.cloned();
    let judge_identifier = review::seat_identifier(&judge.pm);

    let step_ctx = std::sync::Arc::new(review::ReviewStepContext {
        case_id: c.id.clone(),
        crew: ctx.crew.clone(),
        // Passed through raw — `judge_prompt` does the per-field
        // default/strip itself now (byte-matching judge-runner.py's
        // `judge_one`, #1256), so this caller no longer pre-joins
        // title+body or pre-defaults the body.
        intent_title: c.label.intent_title.clone(),
        intent_body: c.label.intent_body.clone(),
        diff: c.diff.clone(),
        probe_system: ctx.probe_system.clone(),
        judge_system: ctx.judge_system.clone(),
        verify_system: ctx.verify_system.clone(),
        bundles,
        // (#1260) Per-execution remote token allowance, resolved through the
        // one precedence home (`env > config.remote.* > 500000`).
        remote_max_tokens_per_execution: darkmux_types::config_access::remote_max_tokens_per_execution(),
        timeout_seconds,
        chat_override: None,
    });

    let graph = review::build_review_graph(
        step_ctx.clone(),
        judge.clone(),
        verify.clone(),
        &probes,
        "investigate",
        "adjudicate",
        "report",
        darkmux_types::config_access::review_judge_concurrency(),
    )
    .with_context(|| format!("building review graph for case {}", c.id))?;
    let fingerprint_val = review::fingerprint(&judge_identifier, &step_ctx.judge_system);
    let staffing_snapshot =
        review::staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.crew.request_changes);

    // (#1397) A bench run mints no real Mission — lab-vs-fleet boundary —
    // so there is nothing to persist a Step to; `persist` is a no-op here,
    // same as `run_review_graph`'s own tests.
    let (env, _steps) = review::run_review_graph(
        &step_ctx,
        &ctx.crew.name,
        ctx.exec_mode,
        fingerprint_val,
        staffing_snapshot,
        graph,
        emitter,
        &mut |_step| {},
    )
    .with_context(|| format!("running review graph for case {}", c.id))?;
    let review = review_from_funnel(&env);
    Ok((review, env))
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

fn print_summary(scored: &[(&Case, CaseScore)], meta: &[EnvelopeMeta], opts: &ReviewBenchOpts) {
    // (#1210) Partition off infra failures (zero-token dispatches) FIRST — they
    // never ran the model, so they belong to neither the capability tallies nor
    // the degenerate count; they get their own named line.
    let infra: usize = scored
        .iter()
        .enumerate()
        .filter(|(i, (_, s))| is_infra_failure(s, meta.get(*i)))
        .count();
    let capability: Vec<&(&Case, CaseScore)> = scored
        .iter()
        .enumerate()
        .filter(|(i, (_, s))| !is_infra_failure(s, meta.get(*i)))
        .map(|(_, cs)| cs)
        .collect();
    let clean: Vec<&CaseScore> = capability
        .iter()
        .filter(|(c, _)| c.label.kind == "clean")
        .map(|(_, s)| s)
        .collect();
    let bugs: Vec<&CaseScore> = capability
        .iter()
        .filter(|(c, _)| c.label.kind == "bug")
        .map(|(_, s)| s)
        .collect();
    let fp_total: usize = clean.iter().map(|s| s.fp).sum();
    let clean_pass = clean.iter().filter(|s| s.correct).count();
    let recall = bugs.iter().filter(|s| s.recall).count();
    let anchor = bugs.iter().filter(|s| s.anchor_ok).count();
    // Degenerate is now the CAPABILITY-degenerate count (model ran, produced
    // unparseable output) — infra-degenerate cases are reported separately.
    let degenerate = capability.iter().filter(|(_, s)| s.degenerate).count();
    println!("\n── summary ({}) ──", opts.profile_name.as_deref().unwrap_or("default"));
    println!("clean: {}/{} pass · {} false positives", clean_pass, clean.len(), fp_total);
    println!("bug:   {}/{} recall · {}/{} correct anchor", recall, bugs.len(), anchor, bugs.len());
    if degenerate > 0 {
        println!("degenerate: {} (empty/unparseable — model unfit for this role)", degenerate);
    }
    if infra > 0 {
        println!(
            "infra-fail: {} (zero tokens served — rate limit / unreachable endpoint / dead \
             dispatch; excluded from capability scores, rerun) (#1210)",
            infra
        );
    }

    // Corpus-wide parity (#1119) — the measuring stick against the control's
    // labels: per-bug recall + per-finding precision, with the access-gap vs
    // diff-visible split for lever attribution. Printed only when the corpus
    // carries the multi-finding schema.
    if capability.iter().any(|(c, _)| c.label.uses_multi()) {
        let pct = |num: usize, den: usize| -> f64 {
            if den == 0 {
                100.0
            } else {
                100.0 * num as f64 / den as f64
            }
        };
        let expected_bugs: usize = capability.iter().map(|(_, s)| s.expected_bugs).sum();
        let bugs_caught: usize = capability.iter().map(|(_, s)| s.bugs_caught).sum();
        let tp: usize = capability.iter().map(|(_, s)| s.tp).sum();
        let fp_all: usize = capability.iter().map(|(_, s)| s.fp).sum();
        let total_findings = tp + fp_all;
        let anchors_ok: usize = capability.iter().map(|(_, s)| s.anchors_ok).sum();
        let exp_access: usize = capability.iter().map(|(_, s)| s.expected_access).sum();
        let exp_diff: usize = capability.iter().map(|(_, s)| s.expected_diff).sum();
        let caught_access: usize = capability.iter().map(|(_, s)| s.caught_access).sum();
        let caught_diff: usize = capability.iter().map(|(_, s)| s.caught_diff).sum();
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
        let legacy_bug_cases = capability
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

/// (#1210) A degenerate case whose dispatch served ZERO tokens is an INFRA
/// failure, not a capability verdict — the #1113 lesson one level up: an
/// endpoint that produced no tokens (a quota-exhausted / 429 hosted seat, an
/// unreachable endpoint, a dead container) never RAN the model, so scoring it
/// `CapabilityFail` writes a junk capability row (observed live 2026-07-05,
/// Gemini free tier — the whole motivation for this issue). The discriminator
/// is the envelope's own token count: a model that RAN and emitted unparseable
/// output served tokens (`total_tokens > 0`) and stays a genuine capability
/// `degenerate` (#1050); only a zero-token dispatch is reclassified. `None`
/// tokens (metrics absent/unparsed) is deliberately NOT treated as infra —
/// we reclassify only on POSITIVE evidence of zero tokens served.
pub(crate) fn is_infra_failure(s: &CaseScore, m: Option<&EnvelopeMeta>) -> bool {
    s.degenerate && matches!(m.and_then(|m| m.total_tokens), Some(0))
}

/// Build the run's score rows: one capability row per case, plus the
/// corpus-wide aggregates the summary prints (recall / precision / anchor
/// rate on the multi-finding schema; clean-pass + legacy recall otherwise).
/// (#1210) Cases the dispatch never actually ran (zero tokens served) emit an
/// `InfraFail` row and are EXCLUDED from every capability denominator below —
/// an infra failure is a rerun, never a zero against the model.
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

    // (#1210) Which case indices are infra failures (zero-token dispatch) —
    // reused for the per-case outcome AND to exclude them from every
    // capability aggregate denominator below.
    let is_infra = |i: usize, s: &CaseScore| is_infra_failure(s, meta.get(i));

    let mut rows = Vec::new();
    for (i, (c, s)) in scored.iter().enumerate() {
        // (#1210) A degenerate review that served ZERO tokens never ran the
        // model — an INFRA failure (rerun), not a capability verdict. A
        // degenerate review that served tokens (the model RAN and emitted
        // unparseable output, #1050) stays a CAPABILITY failure.
        let outcome = if is_infra(i, s) {
            Outcome::InfraFail
        } else if s.degenerate || !s.correct {
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

    // (#1210) Capability aggregates measure the MODEL — an infra-failed case
    // (zero tokens served) is excluded from every denominator so a
    // rate-limited endpoint can't drag recall/precision/clean-pass down.
    let capability: Vec<(usize, &(&Case, CaseScore))> = scored
        .iter()
        .enumerate()
        .filter(|(i, (_, s))| !is_infra(*i, s))
        .collect();

    let clean: Vec<&CaseScore> = capability
        .iter()
        .filter(|(_, (c, _))| c.label.kind == "clean")
        .map(|(_, (_, s))| s)
        .collect();
    let clean_pass = clean.iter().filter(|s| s.correct).count();
    // Clean-only fp here so this row's detail agrees with the printed
    // "clean: X/Y pass · Z false positives" line (review-QA on #1200);
    // corpus-wide fp lives on the `precision` row.
    let clean_fp: usize = clean.iter().map(|s| s.fp).sum();
    let fp_total: usize = capability.iter().map(|(_, (_, s))| s.fp).sum();
    let frac = |num: usize, den: usize| -> Option<f64> {
        (den > 0).then(|| num as f64 / den as f64)
    };
    rows.push(row(
        "clean_pass_rate",
        Outcome::NotApplicable,
        frac(clean_pass, clean.len()),
        serde_json::json!({ "passed": clean_pass, "clean_cases": clean.len(), "false_positives": clean_fp }),
    ));

    if capability.iter().any(|(_, (c, _))| c.label.uses_multi()) {
        let expected_bugs: usize = capability.iter().map(|(_, (_, s))| s.expected_bugs).sum();
        let bugs_caught: usize = capability.iter().map(|(_, (_, s))| s.bugs_caught).sum();
        let tp: usize = capability.iter().map(|(_, (_, s))| s.tp).sum();
        let total_findings = tp + fp_total;
        let anchors_ok: usize = capability.iter().map(|(_, (_, s))| s.anchors_ok).sum();
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
    funnels: &[super::review::ReviewEnvelope],
    opts: &ReviewBenchOpts,
    scores_path: &Path,
    ts_ms: u128,
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
    // ts_ms / scores_path are resolved ONCE by the caller, up front (before
    // the per-case loop) — see `run_review_bench`'s comment — so the run_id
    // below and the incremental funnels.json snapshots share the same
    // identity/location this final write uses.
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
    // (#1222 Phase B packet 7) Funnel-mode provenance: crew name + resolved
    // exec mode + k override — the cell-identity fields a future comparison
    // needs to know two funnel runs are the same condition.
    if opts.mode == BenchMode::Funnel {
        doc.extras.insert(
            "crew".to_string(),
            serde_json::Value::String(opts.crew.clone().unwrap_or_default()),
        );
        let exec_mode_label = funnels
            .first()
            .map(|e| e.mode.clone())
            .unwrap_or_else(|| opts.exec_mode.clone().unwrap_or_else(|| "auto".to_string()));
        doc.extras.insert("exec_mode".to_string(), serde_json::Value::String(exec_mode_label));
        doc.extras.insert(
            "k".to_string(),
            match opts.k_override {
                Some(k) => serde_json::Value::Number(k.into()),
                None => serde_json::Value::String("(profile default)".to_string()),
            },
        );
    }
    let path = scores_path.to_path_buf();
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
    // (#1222 Phase B packet 7 / #1247 Part 2) Funnel envelopes — written
    // FIRST, beside scores.json, and independently of the scores write
    // succeeding, same discipline as `debates` above: the per-flag judge
    // evidence chain is the higher-value audit artifact and must never be
    // dropped because the scores serialization failed. This is the
    // idempotent FINAL state — `run_review_bench`'s per-case loop already
    // streamed every completed case's envelope here via the same
    // `write_funnels_snapshot` helper as each case finished, so a healthy
    // run's last incremental write and this one are identical.
    if !funnels.is_empty() {
        match write_funnels_snapshot(&path, funnels) {
            Ok(()) => eprintln!("funnels: {}", path.with_file_name("funnels.json").display()),
            Err(e) => eprintln!("funnels: WARNING — artifact not written: {e:#}"),
        }
    }
    scores::write_scores(&path, &doc)?;
    Ok(path)
}

/// (#1247 Part 2) Write `funnels.json` beside `scores_path`, ATOMICALLY —
/// serialize to a sibling `.tmp` file, then `rename` it into place. A
/// rename replacing an existing destination is atomic on POSIX, so a
/// concurrent reader (a live `darkmux lab inspect`, a tailing viewer) never
/// observes a partially-written file. Called after EVERY completed
/// Funnel-mode case (not just at end-of-run) so a killed bench — crash,
/// `--timeout`, ctrl-C — keeps every completed case's envelope on disk, not
/// just whatever the last successful write captured.
fn write_funnels_snapshot(scores_path: &Path, funnels: &[super::review::ReviewEnvelope]) -> Result<()> {
    let fpath = scores_path.with_file_name("funnels.json");
    if let Some(parent) = fpath.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for the funnels snapshot", parent.display()))?;
    }
    let tmp = fpath.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(funnels).context("serializing funnels snapshot")?;
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing temp snapshot {}", tmp.display()))?;
    std::fs::rename(&tmp, &fpath)
        .with_context(|| format!("renaming snapshot into place at {}", fpath.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "review_bench_tests.rs"]
mod tests;
