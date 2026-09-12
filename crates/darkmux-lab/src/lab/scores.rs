//! (#1198) `scores.json` — the bench suite's persisted score artifact (#1197).
//!
//! Two score families ride one substrate, distinguished by what their key
//! means: **capability** rows attach to the ARTIFACT `(model, quant, backend,
//! n_ctx)` and are machine-portable (bench once on the biggest machine, reuse
//! fleet-wide); **fit** rows attach to the PAIRING (artifact + machine
//! fingerprint) and are tier-relative by nature. Wall-clock is never stored
//! as a score — its two components are: `tokens_to_solution` (capability) and
//! `seconds_per_token` (fit); a consumer derives wall-clock per machine by
//! multiplying.
//!
//! Design decisions absorbed at birth (see #1197's discussion trail):
//! - **Outcome is three-class**: `pass` / `capability_fail` / `infra_fail`.
//!   A watchdog kill, timeout, or load failure is a RERUN, never a zero on
//!   the model's record — the #1113 lesson (a SIGKILLed dispatch scored
//!   identically to a clean-but-wrong one).
//! - **The machine fingerprint is rich**, not a hostname: "constant hardware"
//!   is what makes fit rows meaningful, and the constant includes the serving
//!   stack (an engine update shifts perf while `machine_id` stays the same).
//! - **`source` marks provenance of the benchmark itself**: `native` for
//!   darkmux-lab benches, or an external harness id (`bfcl`, `ruler`,
//!   `ifeval`, `aider`, …) whose results are ingested as rows —
//!   adopt-don't-rebuild is structural, not aspirational.
//! - **pass^k is native**: every row carries `(trial, k)`; aggregation is the
//!   reader's job (per-trial rows never collapse at write time).
//! - **Loaded-state provenance**: what `lms ps` reported before/after the
//!   run, so a silent mid-run reload at a different context (#1135 class)
//!   is visible in the artifact instead of corrupting the score.
//!
//! Schema versioning follows the repo's minor-bump + lenient-read discipline:
//! all-`Option` where honest, additive fields are minor bumps, readers
//! tolerate unknown fields.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bump per the data-shape semver discipline (additive field = minor).
///
/// 1.1.0 (#2685): added the optional `ScoreRow::infra_classifier` provenance
/// field. Additive + `Option` ⇒ minor; an older reader ignores it.
pub const SCORES_SCHEMA_VERSION: &str = "1.1.0";

/// (#2685) Identifier of the infra-vs-capability rule that produced a row's
/// `Outcome` — stamped into [`ScoreRow::infra_classifier`] so a reader of the
/// artifact can tell WHICH predicate ran instead of having to know which
/// bench wrote the row.
///
/// Bump the suffix whenever [`is_infra_failure`]'s SEMANTICS change (not for
/// a refactor that preserves them). `/1` means: both benches ran the rule
/// below.
///
/// **What an ABSENT value does and does not tell a reader.** `None` means
/// only "written before #2685" — it does NOT say which of the two divergent
/// pre-#2685 rules produced the row, because neither bench stamped anything.
/// A pre-#2685 `review-bench` row was classified by this very rule; a
/// pre-#2685 `tool-bench` row was classified by exit-code-alone; both carry
/// `None`. Telling them apart still requires the fallback this field exists
/// to remove — read `bench`, then know the repo history. A distinct retro
/// value cannot fix that: the rows are already on disk unstamped, and
/// rewriting them would be inventing provenance. So the honest reading of
/// `None` is "provenance unknown, and possibly incommensurable with its
/// neighbor" — which is strictly better than the pre-#2685 state (where a
/// reader had no reason to suspect incommensurability at all), and strictly
/// worse than a stamped row.
pub const INFRA_CLASSIFIER: &str = "envelope-exit/1";

/// Which score family a row belongs to — the load-bearing split (#1197).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreFamily {
    /// Property of the artifact; machine-portable.
    Capability,
    /// Property of the (artifact × machine) pairing; tier-relative.
    Fit,
}

/// Three-class outcome (#1113): infra failures are reruns, never zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    /// The dispatch ran; the model failed the task/contract.
    CapabilityFail,
    /// The harness failed the model: watchdog kill, timeout, load failure,
    /// endpoint unreachable. Excluded from capability aggregation.
    InfraFail,
    /// The row is not a pass/fail observation (an aggregate rate row) — a
    /// naive `count(outcome == pass)` must not be polluted by aggregates
    /// (review-QA finding on #1200).
    NotApplicable,
}

/// What capability scores attach to. `backend` is part of the key — an MLX
/// quant and a GGUF quant of the "same" model are different artifacts, and
/// a remote endpoint is a backend of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArtifactKey {
    /// Model identifier as dispatched (e.g. `qwen3.6-35b-a3b-turboquant-mlx`).
    pub model: String,
    /// Quantization label when known (often embedded in the model id; kept
    /// separate when resolvable so the key is queryable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// Serving backend: `lmstudio-mlx`, `lmstudio-gguf`, `azure-openai`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// The context length the model was (declared) loaded at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u64>,
}

/// Rich machine identity for fit rows + run provenance. Everything
/// best-effort `Option` — a partial fingerprint is still a fingerprint,
/// and lenient-read means older writers never brick newer readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MachineFingerprint {
    /// Operator-facing machine name (`DARKMUX_MACHINE_ID` semantics).
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ram_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// Serving engine + version — the part of "constant hardware" that
    /// changes under your feet (an LMStudio/MLX update shifts perf while
    /// the chip stays the same).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
}

impl MachineFingerprint {
    /// Best-effort detection: hardware via `darkmux-hardware`, OS version via
    /// `sw_vers` (macOS), engine version via `lms --version`. Every probe that
    /// fails leaves `None` rather than erroring — a bench must never die on
    /// fingerprinting.
    pub fn detect(machine_id: &str) -> Self {
        let hw = darkmux_hardware::detect();
        // (#1863, named+tested #2534) A lab run is routinely launched from a
        // git worktree that gets removed later in the same session — if
        // that worktree was the process's cwd, both spawns below would
        // otherwise inherit a now-deleted directory and fail outright
        // rather than degrade to `None`. Neither `sw_vers` nor `lms` reads
        // or writes relative to cwd, so pin both via the one named helper
        // (`darkmux_profiles::lms::pin_cwd` — see its doc comment for why
        // `/`) rather than each repeating `.current_dir("/")` inline.
        let mut sw_vers_cmd = std::process::Command::new("sw_vers");
        darkmux_profiles::lms::pin_cwd(&mut sw_vers_cmd);
        let os_version = sw_vers_cmd
            .arg("-productVersion")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        // `lms --version` emits one plain line (`CLI commit: efce996`);
        // bare `lms version` emits a multi-line ANSI-art banner, which the
        // first tool-bench live run stored verbatim as the fingerprint.
        //
        // (#1939) Resolved through the one `env > config.lms_bin > "lms"`
        // precedence home, not a literal — an operator who set `lms_bin` to
        // a non-default path otherwise gets an `engine_version` fingerprint
        // that describes a DIFFERENT binary than the one that served the
        // run (or a silently absent version if bare `lms` isn't on PATH).
        let mut lms_version_cmd = std::process::Command::new(darkmux_types::config_access::lms_bin());
        darkmux_profiles::lms::pin_cwd(&mut lms_version_cmd);
        let engine_version = lms_version_cmd
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| clean_engine_version(&String::from_utf8_lossy(&o.stdout)));
        Self {
            machine_id: machine_id.to_string(),
            machine_uid: darkmux_hardware::machine_uid().map(str::to_string),
            platform: Some(hw.platform.label().to_string()),
            arch: Some(hw.arch),
            total_ram_gb: Some(hw.total_ram_gb),
            physical_cores: Some(hw.physical_cores),
            os_version,
            engine: engine_version.is_some().then(|| "lmstudio".to_string()),
            engine_version,
        }
    }
}

/// Reduce a version-command's stdout to a fingerprint-worthy string: the
/// first non-empty line, trimmed — rejected outright if it carries ANSI
/// escapes or is banner-length (a version string is short and plain; a
/// styled banner means the probe hit the wrong output shape and storing it
/// would pollute every row's provenance).
fn clean_engine_version(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.contains('\u{1b}') || line.len() > 80 {
        return None;
    }
    Some(line.to_string())
}

/// Per-run provenance shared by every row in a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunProvenance {
    /// The bench run's identifier (a lab run id, or a bench-generated one).
    pub run_id: String,
    /// RFC3339 timestamp, caller-supplied.
    pub ts: String,
    pub machine: MachineFingerprint,
    /// The profile the dispatch named, when threaded (part of reproducing
    /// the run — #1199 makes this explicit on `lab run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Remote dispatches: the endpoint label + what the endpoint SAID served
    /// the request (a deployment named gpt-4o may serve gpt-5.1 — #1191).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    /// `lms ps`-derived loaded state (model, context) captured before/after
    /// the run; `loaded_drift` flags a mismatch (#1135 class). Free-form
    /// JSON — the shape follows `lms ps` rather than a schema of our own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_before: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_after: Option<serde_json::Value>,
    #[serde(default)]
    pub loaded_drift: bool,
}

/// One scored trial of one bench cell. Aggregation (pass^k, means) is the
/// READER's job — per-trial rows never collapse at write time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreRow {
    /// Bench identifier (`review-bench`, `tool-bench`, …) or the external
    /// harness a row was ingested from.
    pub bench: String,
    /// The bench's own version — scores are only comparable within it.
    pub bench_version: String,
    /// `native` for darkmux-lab benches; an external harness id (`bfcl`,
    /// `ruler`, `ifeval`, `aider`, …) for ingested rows.
    pub source: String,
    pub family: ScoreFamily,
    /// The measured axis (`recall`, `precision`, `case`, `chaining@3`, …).
    pub axis: String,
    pub artifact: ArtifactKey,
    pub outcome: Outcome,
    /// Numeric value when the axis is numeric (rates, counts); `None` for
    /// pure pass/fail axes (the outcome carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Trial index within the cell (0-based) + the cell's planned trial
    /// count — the pass^k substrate.
    pub trial: u32,
    pub k: u32,
    /// Capability-side time component (the model's cost in its own currency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_solution: Option<u64>,
    /// Fit-side time component (the machine's cost). Wall-clock = product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_per_token: Option<f64>,
    /// Budgets are denominated in tokens/turns (machine-independent);
    /// recorded so a budget-limited row is interpretable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    /// Bench-specific payload (per-case detail, sampling params, seeds).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
    /// (#2685) Which infra-vs-capability predicate produced `outcome` — see
    /// [`INFRA_CLASSIFIER`]. `scores.json` is a COMPARISON artifact: rows
    /// from different benches sit in one schema and are read as
    /// commensurable, so the classifier has to be on the row rather than
    /// inferred from `bench`. `None` = written before the benches converged;
    /// it does NOT distinguish the two pre-#2685 rules from each other (see
    /// [`INFRA_CLASSIFIER`]'s doc) — only "provenance unknown".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infra_classifier: Option<String>,
}

/// The per-run document: one `scores.json`.
///
/// Lenient-read holds at EVERY level (serde ignores unknown fields by
/// default; no `deny_unknown_fields` anywhere), but forward-compat
/// PRESERVATION on a read-modify-write cycle is top-level only (`extras`) —
/// unknown NESTED fields are dropped on rewrite. The artifact is
/// write-once today; a future rewriting consumer must account for this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoresDoc {
    pub schema_version: String,
    pub provenance: RunProvenance,
    pub rows: Vec<ScoreRow>,
    /// Forward-compat overflow (lenient read).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ScoresDoc {
    pub fn new(provenance: RunProvenance, rows: Vec<ScoreRow>) -> Self {
        Self {
            schema_version: SCORES_SCHEMA_VERSION.to_string(),
            provenance,
            rows,
            extras: serde_json::Map::new(),
        }
    }
}

/// Write the document atomically (temp + rename, mirroring the fixture
/// registry's #543 pattern): a crash leaves either the old file or the new
/// one, never a torn write.
pub fn write_scores(path: &Path, doc: &ScoresDoc) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(doc).context("serializing scores.json")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} into place", path.display()))?;
    Ok(())
}

/// Read a document (lenient: unknown fields tolerated by construction).
pub fn read_scores(path: &Path) -> Result<ScoresDoc> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}


// ─── (#2685) infra-vs-capability classification ─────────────────────────
//
// ONE rule, expressed once, beside the schema it serves. Every bench that
// writes a `ScoreRow` classifies through the predicate below and stamps
// `INFRA_CLASSIFIER` onto the rows it emits.
//
// It lived in `review_bench.rs` until #2685, while `providers::tool_bench`
// carried its own `infra_fail = !dispatch_ok` — so the SAME event (a
// non-zero exit alongside a recovered envelope carrying real tokens, which
// the runtime's escalation arm produces on purpose) was `InfraFail` in one
// bench and `CapabilityFail` in the other, on rows a reader is invited to
// compare. `tool_bench` was aligned UP to this rule rather than this rule
// dropped back to exit-code-alone: the envelope-aware reading is the #1210
// fix, and losing it for the bench that has it would lower the gate.

/// Per-dispatch metadata pulled from the `--json` envelope (best-effort —
/// the score math never depends on it, only the artifact's provenance).
#[derive(Debug, Default, Clone)]
pub(crate) struct EnvelopeMeta {
    pub model: Option<String>,
    pub total_tokens: Option<u64>,
    /// (#1210 MUST-FIX-1) A CLASSIFICATION-ONLY signal: true when
    /// [`envelope_meta_with_exit`] promoted this row to the infra reading
    /// (no envelope recovered AND a non-zero exit code). This must never be
    /// read as a measurement — `total_tokens` stays whatever was actually
    /// parsed (`None` when nothing was), so a crashed/killed container that
    /// may have served real tokens before dying never fabricates a `Some(0)`
    /// in `total_tokens` (and, downstream, in the persisted
    /// `ScoreRow::tokens_to_solution`). Same "`None`, not `Some(0)`: nothing
    /// was measured, not 'zero was measured'" discipline
    /// `runtime/src/main.rs` documents beside its own hardcoded-zero arms.
    pub infra_exit: bool,
}

/// (#2685 frontier-QA) Was an envelope recovered at all, and if so what was
/// in it? The candidate is the last stdout line starting with `{` (falling
/// back to the whole stdout); `None` means it did not parse as JSON.
///
/// Shared by [`envelope_meta`] and [`envelope_meta_with_exit`] so "was an
/// envelope recovered" is decided in exactly ONE place. The exit-promotion
/// below used to approximate it as "no model AND no token count", which is a
/// DIFFERENT predicate: a complete, well-formed envelope that simply carries
/// no `metrics` object satisfies it while having a perfectly good
/// `final_assistant` to read a verdict out of. That approximation was
/// harmless while the promotion was gated on the caller's eligibility bool
/// and wrong the moment it stopped being — see [`is_infra_failure`].
fn parse_envelope(stdout: &str) -> Option<serde_json::Value> {
    let candidate = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or(stdout);
    serde_json::from_str::<serde_json::Value>(candidate.trim()).ok()
}

/// Parse the dispatch envelope (the last stdout line starting with `{` —
/// tolerant of pull-progress noise ahead of it, unlike `extract_reply_text`
/// which parses the whole stdout as one JSON value) for `metrics.model` +
/// token totals. The envelope is compact single-line JSON, so a reply-
/// internal brace can never appear as its own stdout line.
pub(crate) fn envelope_meta(stdout: &str) -> EnvelopeMeta {
    let Some(v) = parse_envelope(stdout) else {
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
        infra_exit: false,
    }
}

/// (#1210) [`envelope_meta`] plus the dispatch's own exit code — closes the
/// gap the issue named as "(or a non-zero exit)" alongside the zero-token
/// envelope case [`is_infra_failure`] already covers.
///
/// `dispatch()` never returns `Err` for a non-zero container exit (see
/// `dispatch_case`'s doc); a genuinely dead container or a runtime crash
/// before it can print its `--json` envelope (a harder failure than the
/// graceful `"result":"error"` envelope a caught 429 produces — that one
/// still writes literal `Some(0)` tokens and is already handled by
/// `envelope_meta` alone) leaves stdout with NO parseable envelope at all.
/// `envelope_meta` alone can't tell that apart from a dialect quirk that
/// left stdout merely malformed-but-present; the exit code is what
/// distinguishes them.
///
/// Only promotes to the infra reading when BOTH are true: no envelope was
/// recovered (the candidate line did not parse — [`parse_envelope`] returns
/// `None`) AND the exit code is non-zero. (#2685 frontier-QA: that used to
/// read "`model` and `total_tokens` both `None`", a proxy that also swept up
/// a complete, parseable envelope carrying no `metrics` object.)
///
/// An envelope WAS recovered — even a `"result":"error"` one with
/// real capability content, however degenerate — is never overridden by
/// exit status; a non-zero exit alongside a fully-parsed envelope is left
/// alone. That combination IS reachable, not out of scope: `runtime/src/
/// main.rs`'s loop-error arm prints a (zeroed) envelope and then returns
/// exit 1, and its escalation arm returns exit 1 *after* the success branch
/// already printed a full envelope carrying real token counts. Both are
/// deliberately left alone here — a recovered envelope is positive
/// capability evidence the exit code doesn't get to overrule. A CLEAN exit
/// (0) with no envelope recovered stays the pre-existing ambiguous case —
/// conservatively NOT reclassified, same "positive evidence only" rule
/// `is_infra_failure` already applies to an unknown token count.
///
/// (#1210 MUST-FIX-1) The promotion sets ONLY the classification signal
/// (`infra_exit: true`); `total_tokens` stays `None`, never a fabricated
/// `Some(0)` — a killed/crashed container may have served real tokens
/// before dying, and this helper has no way to know how many. Zero is a
/// MEASUREMENT (the runtime's own graceful-error envelope really does emit
/// literal 0/0), and this path never measured anything, so it doesn't get
/// to claim zero.
pub(crate) fn envelope_meta_with_exit(stdout: &str, exit_code: i32) -> EnvelopeMeta {
    let m = envelope_meta(stdout);
    // (#2685 frontier-QA) "No envelope was recovered" is decided by
    // [`parse_envelope`] — the candidate line did not parse — NOT by the old
    // proxy "no model and no token count", which also swept up a complete,
    // parseable, merely metrics-less envelope. See that helper's doc.
    if exit_code != 0 && parse_envelope(stdout).is_none() {
        return EnvelopeMeta { model: None, total_tokens: None, infra_exit: true };
    }
    m
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
///
/// The `Some(0)` discriminator matches the REAL quota-failure shape (verified
/// against the runtime, 2026-07-17): the container runtime's `--json` envelope
/// ALWAYS emits numeric `prompt_tokens`/`completion_tokens` — the success path
/// carries the loop's totals, and the error path calls
/// `build_json_envelope("error", ..., 0, 0, ...)` (`runtime/src/main.rs`) with
/// literal zeros — so a quota-dead dispatch parses as `Some(0)`, never `None`.
/// `None` only arises when stdout carried NO parseable envelope at all (a
/// crash before envelope emission), which used to leave `total_tokens` at
/// `None` with no other signal to key off. [`envelope_meta_with_exit`] now
/// carries that case as its own `infra_exit` flag instead of fabricating a
/// `Some(0)` token count (#1210 MUST-FIX-1) — so this predicate treats
/// EITHER positive-zero-tokens evidence OR the exit-promoted flag as infra;
/// neither one touches `total_tokens`, which keeps recording the genuinely
/// unknown case as `None`. (The 2026-07-05 junk rows themselves were
/// operator-cleaned from the corpus, per #1210 — the runtime envelope
/// contract above is the citable evidence.)
///
/// (#2685) The first argument is the CALLER's own "this trial produced no
/// usable output" signal, so the one predicate serves both benches without
/// either bench's score type leaking in here: `review_bench` passes
/// `CaseScore::degenerate` (the review did not parse) and `tool_bench`
/// passes "the reply carried no `ANSWER:`/`BLOCKED:` verdict".
///
/// (#2685 frontier-QA) That eligibility gate applies to the ZERO-TOKEN arm
/// ONLY, and the two arms are asymmetric on purpose because their evidence
/// comes from different places:
///
/// - **`Some(0)` tokens** means an envelope PARSED and reported literal
///   zeros. The reply text the caller judged came out of that same
///   well-formed envelope's own `final_assistant` field, so a parseable
///   verdict in it is trustworthy positive evidence the model ran, and it
///   rightly outranks a token count that may be a metrics quirk. Eligibility
///   gates this arm.
/// - **`infra_exit`** means NO envelope was recovered at all — the candidate
///   line did not PARSE ([`parse_envelope`] returned `None`) — AND the
///   container exited non-zero. That is [`envelope_meta_with_exit`]'s
///   positive evidence that the dispatch DIED, and it leaves no well-formed
///   field to have read a verdict out of: whatever the caller judged was
///   SCRAPED from the same unparseable stdout (`tool_bench`'s
///   `extract_reply` falls back to the raw line and its `extract_answer`
///   accepts a lone nonce anywhere in it; `review_bench`'s freeform parser
///   marks any non-empty text `parsed`). A scrape off a corpse cannot
///   overrule the evidence that it IS a corpse, so this arm is NOT gated.
///
///   (#2685 frontier-QA) The "did not parse" reading is load-bearing for
///   that justification and is why the promotion condition was corrected
///   from its old "no model and no token count" proxy: under the proxy, a
///   complete envelope carrying no `metrics` object was exit-promoted while
///   holding a perfectly well-formed `final_assistant`, so the sentence
///   above would have been false for it. Ungating an arm whose stated
///   premise does not hold is exactly the defect this comment exists to
///   prevent recurring.
///
/// Gating both arms is what let a hard-killed container — a truncated
/// envelope, a lone nonce recovered out of the wreckage — score `Pass` at
/// `value: 1.0` inside `pass_rate`'s numerator AND denominator, i.e. a
/// watchdog kill FLATTERING the model. That is the worst direction this
/// codebase's #1113/#1210 lineage exists to prevent, and it is worse than
/// the exit-code-alone rule `tool_bench` carried before #2685, which at
/// least classified the kill as infra. The conservative reading is the
/// correct one here: an infra row is a RERUN, so a dead dispatch is never
/// scored for or against the model.
pub(crate) fn is_infra_failure(produced_no_usable_output: bool, m: Option<&EnvelopeMeta>) -> bool {
    let Some(m) = m else { return false };
    // Positive evidence the dispatch died. Ungated by design — see above.
    if m.infra_exit {
        return true;
    }
    // The recovered-envelope-with-literal-zeros shape (#1210): gated on the
    // caller's own "this trial produced no usable output" signal.
    produced_no_usable_output && matches!(m.total_tokens, Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_engine_version_accepts_a_plain_line_and_rejects_banners() {
        // The real `lms --version` shape.
        assert_eq!(
            clean_engine_version("CLI commit: efce996\n"),
            Some("CLI commit: efce996".to_string())
        );
        // Leading blank lines tolerated.
        assert_eq!(
            clean_engine_version("\n  1.2.3  \n"),
            Some("1.2.3".to_string())
        );
        // The `lms version` ANSI-art banner (what the first live tool-bench
        // run stored verbatim) must be rejected, not truncated into noise.
        assert_eq!(
            clean_engine_version("\u{1b}[38;5;166m   __   __  ___\u{1b}[0m\nlms is LM Studio's CLI"),
            None
        );
        // Banner-length plain text is still not a version string.
        assert_eq!(clean_engine_version(&"x".repeat(120)), None);
        assert_eq!(clean_engine_version("   \n\n"), None);
    }

    fn sample_row() -> ScoreRow {
        ScoreRow {
            bench: "review-bench".into(),
            bench_version: "1".into(),
            source: "native".into(),
            family: ScoreFamily::Capability,
            axis: "recall".into(),
            artifact: ArtifactKey {
                model: "test-model".into(),
                quant: None,
                backend: Some("lmstudio-mlx".into()),
                n_ctx: Some(32768),
            },
            outcome: Outcome::Pass,
            value: Some(0.67),
            trial: 0,
            k: 1,
            tokens_to_solution: Some(23662),
            seconds_per_token: None,
            budget_turns: None,
            budget_tokens: None,
            detail: serde_json::Value::Null,
            infra_classifier: Some(INFRA_CLASSIFIER.to_string()),
        }
    }

    #[test]
    fn roundtrip_preserves_document() {
        let doc = ScoresDoc::new(
            RunProvenance {
                run_id: "rb-123".into(),
                ts: "2026-07-05T12:00:00Z".into(),
                machine: MachineFingerprint {
                    machine_id: "laptop".into(),
                    total_ram_gb: Some(128),
                    ..Default::default()
                },
                ..Default::default()
            },
            vec![sample_row()],
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        write_scores(&path, &doc).unwrap();
        let back = read_scores(&path).unwrap();
        assert_eq!(back.schema_version, SCORES_SCHEMA_VERSION);
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0].axis, "recall");
        assert_eq!(back.rows[0].outcome, Outcome::Pass);
        assert_eq!(back.rows[0].artifact.n_ctx, Some(32768));
        assert_eq!(back.provenance.machine.total_ram_gb, Some(128));
    }

    #[test]
    fn lenient_read_tolerates_unknown_fields() {
        // A NEWER writer's field must not brick this reader (minor-bump +
        // lenient-read discipline).
        let json = r#"{
            "schema_version": "1.5.0",
            "future_top_level": {"x": 1},
            "provenance": {"run_id": "r", "ts": "t", "machine": {"machine_id": "m"}},
            "rows": []
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        std::fs::write(&path, json).unwrap();
        let doc = read_scores(&path).unwrap();
        assert_eq!(doc.schema_version, "1.5.0");
        assert!(doc.extras.contains_key("future_top_level"));
    }

    #[test]
    fn lenient_read_tolerates_nested_unknown_fields() {
        // Unknown fields INSIDE provenance/machine/rows must not brick the
        // read either (they are ignored, not preserved — documented).
        let json = r#"{
            "schema_version": "1.5.0",
            "provenance": {
                "run_id": "r", "ts": "t", "future_prov_field": true,
                "machine": {"machine_id": "m", "future_hw_field": 9}
            },
            "rows": [{
                "bench": "b", "bench_version": "1", "source": "native",
                "family": "capability", "axis": "case",
                "artifact": {"model": "m", "future_key_part": "x"},
                "outcome": "pass", "trial": 0, "k": 1,
                "future_row_field": [1, 2]
            }]
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        std::fs::write(&path, json).unwrap();
        let doc = read_scores(&path).unwrap();
        assert_eq!(doc.rows.len(), 1);
        assert_eq!(doc.rows[0].artifact.model, "m");
    }

    #[test]
    fn outcome_serializes_snake_case() {
        // The enum encoding is a wire contract — pin it.
        assert_eq!(
            serde_json::to_string(&Outcome::InfraFail).unwrap(),
            "\"infra_fail\""
        );
        assert_eq!(
            serde_json::to_string(&ScoreFamily::Capability).unwrap(),
            "\"capability\""
        );
    }

    #[test]
    fn atomic_write_replaces_not_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        let doc1 = ScoresDoc::new(RunProvenance::default(), vec![sample_row()]);
        write_scores(&path, &doc1).unwrap();
        let mut doc2 = doc1.clone();
        doc2.rows.clear();
        write_scores(&path, &doc2).unwrap();
        let back = read_scores(&path).unwrap();
        assert!(back.rows.is_empty(), "second write replaces the first");
        assert!(!path.with_extension("json.tmp").exists(), "no temp litter");
    }


    // ─── (#2685) the shared infra-vs-capability rule ────────────────
    // Moved here with the code under test (was `review_bench_tests.rs`,
    // where it sat next to a rule `tool_bench` did not call).

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

    /// (#1210) The issue's "(or a non-zero exit)" clause: a dead
    /// container / runtime crash never even writes a `--json` line, so
    /// `envelope_meta` alone sees `None`/`None` — the exit code is what
    /// tells this apart from a merely-malformed-but-present envelope.
    /// Positive evidence only: BOTH "no envelope" AND "non-zero exit"
    /// are required before the infra reading kicks in.
    ///
    /// (#1210 MUST-FIX-1, review-QA) The promotion sets the CLASSIFICATION
    /// flag `infra_exit`, never a fabricated token count — `total_tokens`
    /// stays `None`. A killed/crashed container may have served real
    /// tokens before dying; this helper has no way to know how many, so it
    /// must not claim zero (that's a MEASUREMENT, and none was taken here).
    #[test]
    fn envelope_meta_with_exit_promotes_no_envelope_plus_nonzero_exit_to_infra_flag_not_zero_tokens() {
        // No stdout at all (the runtime never got far enough to print).
        let m = envelope_meta_with_exit("", 1);
        assert_eq!(m.model, None);
        assert_eq!(m.total_tokens, None, "no measurement was taken — never a fabricated zero");
        assert!(m.infra_exit, "non-zero exit + no envelope sets the classification flag");

        // Garbage stdout (docker printed something, but not an envelope).
        let m = envelope_meta_with_exit("panicked at src/main.rs:42", 137);
        assert_eq!(m.total_tokens, None);
        assert!(m.infra_exit);
    }

    /// (#2685 frontier-QA) "No envelope was recovered" means the candidate
    /// line DID NOT PARSE — not "no model and no token count". Those are
    /// different predicates, and the gap between them is a complete,
    /// well-formed envelope that simply carries no `metrics` object: it has
    /// a perfectly good `final_assistant` to read a verdict out of, yet the
    /// old `model.is_none() && total_tokens.is_none()` condition promoted it
    /// to the infra reading anyway.
    ///
    /// That mattered only once the `infra_exit` arm stopped being gated on
    /// the caller's eligibility bool: before, a parsed verdict masked it;
    /// after, it classifies infra unconditionally. The whole justification
    /// for ungating that arm is "there was no well-formed field to read a
    /// verdict out of", which is FALSE for this shape — so the condition is
    /// now the one the justification actually describes.
    ///
    /// No live producer reaches it (`runtime/src/main.rs`'s
    /// `build_json_envelope` always emits a `metrics` object carrying
    /// `model`), which makes this a CONTRACT defect rather than a
    /// misclassification. Pinned so it stays fixed.
    #[test]
    fn envelope_meta_with_exit_never_promotes_a_parseable_envelope_that_carries_no_metrics() {
        let stdout = r#"{"result":"stop","final_assistant":"ANSWER: DMX-QBW4MP5J"}"#;
        let m = envelope_meta_with_exit(stdout, 137);
        assert_eq!(m.model, None, "no metrics object, so no model");
        assert_eq!(m.total_tokens, None, "no metrics object, so no token count");
        assert!(
            !m.infra_exit,
            "the envelope PARSED — a well-formed field carried the verdict, so this is not a dead dispatch"
        );
        // And therefore it is never reclassified: `None` tokens is the
        // documented ambiguous case, kept capability-side on purpose.
        assert!(!is_infra_failure(true, Some(&m)), "unknown tokens is not positive infra evidence");
        assert!(!is_infra_failure(false, Some(&m)));
    }

    /// (#1210 MUST-FIX-2, review-QA) Pins the FIRST of the two conjuncts a
    /// mutation pass found untested: a token-bearing envelope with NO
    /// `model` field, at a non-zero exit, must NOT be promoted — dropping
    /// the `total_tokens.is_none()` conjunct (or turning the `&&` into
    /// `||`) would launder these real, positive tokens into a fabricated
    /// infra zero.
    #[test]
    fn envelope_meta_with_exit_keeps_real_tokens_when_model_is_missing() {
        let stdout = r#"{"result":"stop","metrics":{"prompt_tokens":30000,"completion_tokens":11200}}"#;
        let m = envelope_meta_with_exit(stdout, 137);
        assert_eq!(m.model, None, "no model field in this envelope — a real, if incomplete, dialect");
        assert_eq!(m.total_tokens, Some(41200), "real token count must survive a non-zero exit");
        assert!(!m.infra_exit, "positive token evidence means this was never promoted");
    }

    /// (#1210 MUST-FIX-2, review-QA) Pins the SECOND untested conjunct: a
    /// model-bearing envelope with NO token fields, at a non-zero exit,
    /// must also NOT be promoted — dropping the `model.is_none()` conjunct
    /// (or the same `&&`-to-`||` mutation) would discard this positive
    /// "the model ran" evidence and fabricate an infra classification.
    #[test]
    fn envelope_meta_with_exit_keeps_recovered_model_when_tokens_are_missing() {
        let stdout = r#"{"result":"stop","metrics":{"model":"m-x"}}"#;
        let m = envelope_meta_with_exit(stdout, 137);
        assert_eq!(m.model.as_deref(), Some("m-x"), "recovered model must survive a non-zero exit");
        assert_eq!(m.total_tokens, None, "no token fields in this envelope — genuinely unknown");
        assert!(!m.infra_exit, "positive model evidence means this was never promoted");
    }

    /// (#1210 inverted case) A genuine capability failure — the model RAN,
    /// wrote a real envelope, and the process exited cleanly — must never be
    /// reclassified by this helper. Reclassifying real model failures as
    /// infra would flatter every model's score, which is worse than the bug.
    #[test]
    fn envelope_meta_with_exit_never_overrides_a_recovered_envelope() {
        let stdout = "{\"result\":\"stop\",\"metrics\":{\"model\":\"m-x\",\"prompt_tokens\":180,\"completion_tokens\":20}}";
        // Clean exit, real envelope, real tokens — untouched.
        let ok = envelope_meta_with_exit(stdout, 0);
        assert_eq!(ok.total_tokens, Some(200));
        // Even a non-zero exit alongside a RECOVERED envelope is left
        // alone — the envelope is positive capability evidence the exit
        // code doesn't get to overrule.
        let weird = envelope_meta_with_exit(stdout, 1);
        assert_eq!(weird.total_tokens, Some(200), "a recovered envelope is never overridden by exit status");
    }

    /// (#1210 ambiguous case, stated honestly) A CLEAN exit (0) with no
    /// envelope recovered is left exactly where the pre-existing
    /// `is_infra_failure` "None tokens is NOT infra evidence" rule already
    /// puts it: NOT reclassified. This is the one combination this fix
    /// deliberately does not attribute either way — a clean exit that wrote
    /// no envelope is unexplained by anything the bench can observe, and
    /// guessing it into infra would launder a real capability failure just
    /// as guessing it into capability would poison the corpus. It stays
    /// capability-side (via the existing `degenerate` scoring path), which
    /// is honest: unexplained is not the same claim as "the model is at
    /// fault", but it is also not silently attributed to infra either.
    #[test]
    fn envelope_meta_with_exit_leaves_clean_exit_no_envelope_ambiguous() {
        let m = envelope_meta_with_exit("", 0);
        assert_eq!(m.model, None);
        assert_eq!(m.total_tokens, None, "clean exit + no envelope stays the pre-existing unknown case");
        // `total_tokens` is `None` in both the promoted and un-promoted
        // outcomes now (neither fabricates a token count) — `infra_exit` is
        // what actually distinguishes them, and it's what a mutant dropping
        // the exit-code conjunct entirely (promote on any missing envelope,
        // clean exit included) would flip.
        assert!(!m.infra_exit, "a CLEAN exit must never set the classification flag");
    }

    /// (#1210 gate coverage) `is_infra_failure`'s arms: positive
    /// zero-token evidence (a recovered envelope with literal `0` tokens,
    /// the 429/quota shape) reclassifies, and so does the exit-promoted
    /// `infra_exit` flag (the crashed/killed-container shape) — but plain
    /// unknown (`None`) tokens with neither signal never does. The runtime
    /// envelope always emits numeric token fields on its own success/error
    /// paths (see the fn doc), so `None` means "no parseable envelope" —
    /// kept capability-side deliberately, never guessed into infra.
    ///
    /// (#2685 frontier-QA) Also pins the ASYMMETRY between the two arms:
    /// the eligibility bool gates the zero-token arm and NOT the
    /// `infra_exit` arm. Red-proves against re-collapsing the predicate to
    /// one conjunct over both, which let a scrape off a dead container's
    /// unparseable stdout overrule positive evidence the container died.
    #[test]
    fn is_infra_failure_requires_degenerate_and_positive_zero_token_evidence() {
        // (#2685) The caller's own "produced no usable output" signal,
        // now a plain bool so the one predicate serves every bench:
        // `review_bench` passes `CaseScore::degenerate`, `tool_bench`
        // passes "the reply carried no ANSWER:/BLOCKED: verdict".
        let degen = true;
        let ran_fine = false;
        let zero = EnvelopeMeta { model: None, total_tokens: Some(0), infra_exit: false };
        let served = EnvelopeMeta { model: None, total_tokens: Some(250), infra_exit: false };
        let unknown = EnvelopeMeta::default(); // total_tokens: None, infra_exit: false
        let exit_promoted = EnvelopeMeta { model: None, total_tokens: None, infra_exit: true };

        assert!(is_infra_failure(degen, Some(&zero)), "degenerate + zero tokens = infra");
        assert!(!is_infra_failure(degen, Some(&served)), "model ran = capability degenerate");
        assert!(!is_infra_failure(degen, Some(&unknown)), "None tokens is NOT infra evidence");
        assert!(!is_infra_failure(degen, None), "missing meta row is NOT infra evidence");
        assert!(!is_infra_failure(ran_fine, Some(&zero)), "non-degenerate never reclassifies");
        // (#1210 MUST-FIX-1) The exit-promoted flag reclassifies exactly
        // like positive-zero-token evidence, WITHOUT the token count itself
        // ever being fabricated as `Some(0)`.
        assert!(is_infra_failure(degen, Some(&exit_promoted)), "infra_exit flag alone = infra");
        // (#2685 frontier-QA) …and the eligibility bool does NOT gate this
        // arm. `infra_exit` means no envelope parsed at all, so the caller's
        // "usable output" signal was scraped from that same unparseable
        // stdout — a scrape off a corpse cannot outrank the evidence that it
        // is a corpse. Gating this arm scored a watchdog kill as a `Pass`.
        assert!(
            is_infra_failure(ran_fine, Some(&exit_promoted)),
            "a dead container stays infra even when the caller scraped a verdict out of its wreckage"
        );
    }


    #[test]
    fn fingerprint_detect_is_best_effort_never_panics() {
        let fp = MachineFingerprint::detect("test-machine");
        assert_eq!(fp.machine_id, "test-machine");
        // Hardware fields come from darkmux_hardware::detect() which always
        // returns; the shell-out fields may be None — both are valid.
        assert!(fp.total_ram_gb.is_some());
    }
}
