//! (#1222 Phase B packet 4) Review funnel — the validated review pipeline:
//! bundles → probe seats ×k draws → dedup → double-confirm judge → a
//! three-tier envelope.
//!
//! ```text
//! bundle → probe(k draws × seat, temp 0.2) → dedup → judge pass-1(every flag)
//!        → judge pass-2(pass-1 confirms only) → {confirmed, needs_check, archived}
//! ```
//!
//! This module is the DRIVER: given a resolved crew (packet 1's
//! `darkmux_profiles::crews::resolve_crew`), a diff, and an intent, it runs
//! the whole pipeline and returns a [`FunnelEnvelope`]. Dispatch itself goes
//! through a caller-injected `chat` closure (the container-free single-shot
//! primitive from packet 2, `darkmux_crew::single_shot::single_shot_chat`,
//! in production) and a caller-injected [`ModelCycler`] (real `lms` calls in
//! production, a recording mock in tests) — so the whole pipeline is
//! unit-testable without a live LMStudio or a real dispatch.
//!
//! ## Double-confirm judge (the load-bearing design choice)
//!
//! Every probe flag gets a judge pass-1 ruling. Only a `confirmed` pass-1
//! gets a pass-2 — a FRESH judge call over the identical prompt. Agreement
//! (confirmed → confirmed) promotes the flag to [`Tier::Confirmed`];
//! disagreement demotes it to [`Tier::NeedsCheck`] rather than shipping a
//! coin-flip as a defect report. This mirrors the CLAUDE.md "recheck vs
//! rethink" doctrine at judge scale: a single judge call is one context's
//! opinion; two independent calls voting the same way is real signal.
//!
//! ## Bundling — the packet 3 seam
//!
//! [`BundleInput`] is deliberately this module's OWN shape, decoupled from
//! `darkmux_lab::lab::bundle::{Bundle, BundleSet, build_bundles, slice_code,
//! external_bundles, FileSource}` (Phase B packet 3), which had not landed
//! on `main` when this packet was written. [`bundles_from_diff`] is the
//! PROVISIONAL bundler standing in for the real one — see its doc comment
//! for what it stands in for. Every other piece of this module (probe/
//! dedup/judge/envelope) is written entirely against `BundleInput` and
//! needed no changes once the real bundler landed.
//!
//! **Reconciled in packet 5** (`darkmux pr-review run`, `src/pr_review.rs`
//! in the binary crate): rather than editing `bundles_from_diff`'s body
//! in place, [`FunnelInputs::bundles`] is the injection seam — packet 5
//! builds real bundles via `build_bundles`/`external_bundles` + `slice_code`
//! and passes `Some(..)`; [`run_funnel`]/[`run_judge_only`] use those
//! directly and never call the provisional bundler. `bundles_from_diff`
//! survives only as the `None` fallback this module's own pre-packet-3
//! tests still rely on — no production caller uses it.
//!
//! Parsers and the dedup/double-confirm state machine are pure and
//! unit-tested; dispatching goes through caller-provided closures/traits so
//! the whole chain is testable without containers or a live LMStudio —
//! same discipline as `super::dialectic`.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_profiles::crews::{ResolvedCrew, ResolvedSeatStaffing};
use darkmux_profiles::{lms, swap};
use darkmux_types::{BundleSelector, ProfileModel};
use serde::{Deserialize, Serialize};
use std::time::Instant;

// ─── execution mode ───────────────────────────────────────────────────────

/// How probe/judge models are cycled through LMStudio across the funnel's
/// dispatches. `Auto` resolves once, up front, to `Sequential` or
/// `Parallel` (see [`resolve_mode`]) — the resolved choice is what
/// `FunnelEnvelope::mode` records, so an operator reading the envelope
/// never has to wonder which one actually ran.
///
/// This governs LMStudio RESIDENCY (which models stay loaded), not
/// concurrent network dispatch — `Sequential` loads one member, runs every
/// draw for it, releases it, then moves on; `Parallel` loads every member
/// up front and dispatches each staffing's draws in turn without
/// releasing between them (dispatches themselves still run one at a time
/// through the injected `chat` closure — true concurrent dispatch is a
/// separate, unaddressed concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Sequential,
    Parallel,
    Auto,
}

fn mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Sequential => "sequential",
        ExecMode::Parallel => "parallel",
        // `resolve_mode` always turns `Auto` into one of the above before
        // this is ever read into an envelope; kept for exhaustiveness.
        ExecMode::Auto => "auto",
    }
}

// ─── probe flags ──────────────────────────────────────────────────────────

/// One probe draw's finding, post-parse but pre-dedup. `anchor` starts
/// `None` at construction — [`dedup_flags`] is where anchor extraction
/// happens (it needs the diff to validate a quote against, so doing the
/// extraction there keeps ONE place responsible for both jobs at once).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeFlag {
    pub bundle_id: String,
    pub fact_family: String,
    /// The probe staffing that produced this draw — the darkmux-namespaced
    /// LMStudio identifier (e.g. `darkmux:qwen3.6-35b-a3b`), so a mixed-
    /// model probe seat's flags stay attributable.
    pub member: String,
    pub draw: u32,
    pub charge_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// Bookkeeping [`dedup_flags`] returns alongside the deduped list — the
/// raw/deduped counts an envelope's `raw_flags`/`deduped_flags` fields are
/// sourced from.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DedupStats {
    pub raw: usize,
    pub deduped: usize,
}

// ─── judge rulings ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunnelRuling {
    Confirmed,
    NeedsCheck,
    FalsePositive,
    /// The judge's reply carried no recognizable fenced JSON ruling (after
    /// one retry — see [`judge_pass_with_retry`]).
    Unparsed,
    /// The dispatch itself failed (propagated up from `chat`, wrapped here
    /// rather than aborting the whole docket over one bad call).
    Error,
}

/// One judge call's outcome. `pass` is `1` or `2` (double-confirm); one
/// `JudgeRecord` per actual dispatch — a retried pass-1 produces TWO
/// records internally but only the retry's outcome survives into a
/// [`JudgedFlag`] (the first, unparsed attempt is discarded, not hidden —
/// see `judge_pass_with_retry`'s doc for why that's honest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRecord {
    pub ruling: FunnelRuling,
    pub decisive_evidence: String,
    pub note_for_author: String,
    pub pass: u8,
    pub seconds: f64,
}

/// The three-tier envelope outcome for one flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Confirmed,
    NeedsCheck,
    Archived,
}

/// One flag's full judge record: pass-1 always present, pass-2 present iff
/// pass-1 was `confirmed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgedFlag {
    pub flag: ProbeFlag,
    pub pass1: JudgeRecord,
    pub pass2: Option<JudgeRecord>,
    pub tier: Tier,
    /// `true` iff a pass-1 `confirmed` was demoted to `needs_check` because
    /// pass-2 disagreed — the specific signal an operator scanning the
    /// envelope wants to find first (a flag the judge itself wasn't sure
    /// about, not one the harness is guessing on).
    pub demoted_by_pass2: bool,
}

// ─── telemetry ────────────────────────────────────────────────────────────

/// Per-model resource accounting — one row per probe staffing plus one for
/// the judge seat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberRecord {
    pub model: String,
    pub seat: String,
    pub draws: u32,
    pub wall_ms: u64,
    pub total_tokens: u64,
}

/// One pipeline step's in/out counts + wall time — the issue #1230 bridge:
/// a future flow-record consumer can render the funnel as a step timeline
/// without re-deriving it from the envelope's nested arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    /// `bundle` | `probe` | `dedup` | `judge-pass1` | `judge-pass2`.
    pub step_id: String,
    /// `procedural` (no dispatch — bundling, dedup) | `dispatch` (LMStudio
    /// calls).
    pub kind: String,
    pub items_in: usize,
    pub items_out: usize,
    pub wall_ms: u64,
}

// ─── the envelope ─────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FunnelEnvelope {
    pub case_id: String,
    pub crew: String,
    pub mode: String,
    pub members: Vec<MemberRecord>,
    pub steps: Vec<StepRecord>,
    pub bundles: usize,
    pub raw_flags: usize,
    pub deduped_flags: usize,
    pub flags: Vec<ProbeFlag>,
    pub judged: Vec<JudgedFlag>,
    pub confirmed: usize,
    pub needs_check: usize,
    pub archived: usize,
    /// Set (never silently left empty) when the docket produced zero raw
    /// flags (every probe drew nothing usable) — a degenerate run is a
    /// LOUD, scoreable outcome, never a silent pass. `None` on a normal run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degenerate: Option<String>,
    /// Judge model + temperature + persona hash + protocol version — what
    /// two envelopes need to share before their tiers are comparable.
    pub fingerprint: serde_json::Value,
}

// ─── model cycling ────────────────────────────────────────────────────────

/// Load/release one [`ProfileModel`] into/out of LMStudio. Injected so
/// tests can assert on cycling ORDER via a recording mock without a live
/// LMStudio; production dispatch uses [`LmsCycler`].
pub trait ModelCycler {
    fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()>;
    fn release(&mut self, pm: &ProfileModel) -> Result<()>;
}

/// Production [`ModelCycler`]: real `lms` calls, namespaced under
/// `darkmux:` (the same operator-sovereignty guard `swap::swap` uses — a
/// model NOT in the namespace is user state and is never unloaded) and
/// context-sufficiency aware (a model already loaded with >= the wanted
/// context is left in place, mirroring `swap::ctx_sufficient` — no
/// needless reload-down).
pub struct LmsCycler;

impl ModelCycler for LmsCycler {
    fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
        let identifier = swap::namespaced_identifier(pm);
        let loaded = lms::list_loaded()?;
        if loaded
            .iter()
            .any(|l| l.identifier == identifier && l.context >= u64::from(pm.n_ctx))
        {
            return Ok(());
        }
        lms::load_with_identifier(&pm.id, pm.n_ctx, &identifier, true)
    }

    fn release(&mut self, pm: &ProfileModel) -> Result<()> {
        let identifier = swap::namespaced_identifier(pm);
        if !swap::is_darkmux_owned(&identifier) {
            return Ok(());
        }
        lms::unload(&identifier)
    }
}

// ─── constants ────────────────────────────────────────────────────────────

const PROBE_TEMPERATURE: f32 = 0.2;
const JUDGE_TEMPERATURE: f32 = 0.2;
const DEFAULT_PROBE_MAX_TOKENS: u32 = 4_000;
const DEFAULT_JUDGE_MAX_TOKENS: u32 = 20_000;
const FUNNEL_PROTOCOL: &str = "double-confirm-v1";

/// A hardware-tier concurrency budget for [`resolve_auto`]: the review
/// funnel is a light, occasional dispatch (not throughput-critical
/// infrastructure), so a coarse rule beats a tuned cost model — KISS per
/// CLAUDE.md doctrine. `distinct_models` counts unique model ids across
/// every probe staffing plus the judge — the number that would need to be
/// simultaneously resident under `Parallel`.
fn resolve_auto(distinct_models: usize, hw: &darkmux_hardware::HardwareSpec) -> ExecMode {
    let budget = match hw.ram_tier() {
        darkmux_hardware::RamTier::Xl | darkmux_hardware::RamTier::Large => 3,
        darkmux_hardware::RamTier::Medium => 2,
        darkmux_hardware::RamTier::Small => 1,
    };
    if distinct_models <= budget {
        ExecMode::Parallel
    } else {
        ExecMode::Sequential
    }
}

fn resolve_mode(mode: ExecMode, probes: &[ResolvedSeatStaffing], judge: &ResolvedSeatStaffing) -> ExecMode {
    match mode {
        ExecMode::Auto => {
            let mut ids: Vec<&str> = probes.iter().map(|s| s.pm.id.as_str()).collect();
            ids.push(judge.pm.id.as_str());
            ids.sort_unstable();
            ids.dedup();
            resolve_auto(ids.len(), &darkmux_hardware::detect())
        }
        other => other,
    }
}

// ─── crew validation (funnel-owned seat requirements) ───────────────────

/// Validate `crew` carries what the funnel needs: seat `"review-probe"`
/// with >= 1 staffing, seat `"review-judge"` with EXACTLY 1 staffing.
/// `resolve_crew` (packet 1) validates the crew schema is well-formed and
/// every model resolvable; it deliberately does NOT know about
/// pipeline-specific seat requirements — that's this function's job, and
/// it runs at funnel start so a misconfigured crew fails loud before any
/// dispatch spends a token.
fn validate_funnel_crew(crew: &ResolvedCrew) -> Result<(&Vec<ResolvedSeatStaffing>, &ResolvedSeatStaffing)> {
    let probes = crew
        .seats
        .get("review-probe")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "darkmux: crew \"{}\" is missing seat \"review-probe\" (the review \
                 funnel needs >= 1 staffing) — add one under crews.\"{}\".seats.\"review-probe\"",
                crew.name,
                crew.name
            )
        })?;
    let judges = crew.seats.get("review-judge").ok_or_else(|| {
        anyhow!(
            "darkmux: crew \"{}\" is missing seat \"review-judge\" (the review \
             funnel needs exactly 1 staffing)",
            crew.name
        )
    })?;
    if judges.len() != 1 {
        bail!(
            "darkmux: crew \"{}\" seat \"review-judge\" must have EXACTLY 1 staffing \
             (got {}) — the double-confirm judge is a single seat, unlike \"review-probe\"",
            crew.name,
            judges.len()
        );
    }
    Ok((probes, &judges[0]))
}

// ─── mechanism-family keyword table (for dedup) ──────────────────────────

/// Lowercased alphanumeric word tokens of `text` — the unit
/// [`mechanism_family`] matches on. Splitting on every non-alphanumeric
/// char means `Date.now()` tokenizes as `["date", "now"]` and `copy-paste`
/// as `["copy", "paste"]`, so punctuation variants match without any
/// substring tricks.
fn word_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `seq` appears in `tokens` as CONSECUTIVE whole tokens.
fn contains_token_seq(tokens: &[String], seq: &[&str]) -> bool {
    !seq.is_empty()
        && tokens.len() >= seq.len()
        && tokens
            .windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

/// Classify a charge's prose into a coarse mechanism family for dedup —
/// deliberately coarse (a keyword table, not a classifier): dedup only
/// needs "these two flags are probably the same finding," not a precise
/// taxonomy.
///
/// Matching is WHOLE-TOKEN (word-boundary), never substring — the naive
/// `.contains()` form classified "tenant", "covenant", and "finance" as
/// `null/nan` (all contain "nan"), so two DISTINCT unanchored charges on a
/// billing corpus collapsed in dedup and a real defect was silently
/// dropped (frontier QA should-fix on this packet's PR). Plural/variant
/// forms are listed explicitly rather than stemmed — transparent beats
/// clever for a table this small.
fn mechanism_family(charge_text: &str) -> &'static str {
    const TABLE: &[(&str, &[&[&str]])] = &[
        (
            "timezone/ambient-time",
            &[
                &["timezone"],
                &["timezones"],
                &["time", "zone"],
                &["time", "zones"],
                &["utc"],
                &["date", "now"],
                &["new", "date"],
                &["ambient", "time"],
                &["local", "time"],
                &["dst"],
                &["daylight", "saving"],
                &["daylight", "savings"],
            ],
        ),
        (
            "arity/param",
            &[
                &["argument"],
                &["arguments"],
                &["arg"],
                &["args"],
                &["parameter"],
                &["parameters"],
                &["param"],
                &["params"],
                &["arity"],
                &["wrong", "number", "of"],
            ],
        ),
        (
            "null/nan",
            &[&["null"], &["undefined"], &["nan"], &["none"], &["nil"]],
        ),
        (
            "async/await",
            &[
                &["async"],
                &["await"],
                &["promise"],
                &["promises"],
                &["race", "condition"],
                &["event", "loop"],
                &["callback"],
                &["callbacks"],
                &["unhandled", "rejection"],
            ],
        ),
        (
            "provenance/sibling",
            &[
                &["sibling"],
                &["siblings"],
                &["duplicate", "logic"],
                &["other", "implementation"],
                &["diverge"],
                &["diverges"],
                &["diverged"],
                &["copy", "paste"],
                &["provenance"],
            ],
        ),
    ];
    let tokens = word_tokens(charge_text);
    for (family, keyword_seqs) in TABLE {
        if keyword_seqs.iter().any(|seq| contains_token_seq(&tokens, seq)) {
            return family;
        }
    }
    "other"
}

// ─── anchor extraction (reuses dialectic's matching discipline) ─────────

/// The first backtick-quoted span in `charge_text` that matches a NEW-side
/// diff line (context or `+`; never a deleted `-` line — an anchor should
/// point at code that still exists). Reuses `super::dialectic`'s
/// normalization (leading `+`/`-` strip, whitespace-collapse fallback for
/// a diff-wrapped logical line) so both matchers share ONE discipline
/// rather than re-deriving the wrapped-line/marker-strip fixes twice —
/// including its [`dialectic::MIN_EVIDENCE_SPAN`] floor, so a trivial
/// span (`0`, `}`) is inline code styling, never an anchor / dedup key.
fn extract_new_side_anchor(charge_text: &str, diff: &str) -> Option<String> {
    use super::dialectic::{
        backtick_spans, collapse_ws, diff_line_content, normalize_anchor, MIN_EVIDENCE_SPAN,
    };
    let new_side_lines: Vec<&str> = diff.lines().filter(|l| !l.starts_with('-')).collect();
    let collapsed = collapse_ws(
        &new_side_lines
            .iter()
            .map(|l| diff_line_content(l))
            .collect::<Vec<_>>()
            .join(" "),
    );
    for span in backtick_spans(charge_text) {
        let a = normalize_anchor(&span);
        if a.trim().len() < MIN_EVIDENCE_SPAN {
            continue;
        }
        let found = new_side_lines.iter().any(|l| diff_line_content(l).contains(a))
            || collapsed.contains(&collapse_ws(a));
        if found {
            return Some(span);
        }
    }
    None
}

// ─── dedup ────────────────────────────────────────────────────────────────

/// Dedup raw probe flags. Key = `(bundle_id, anchor-or-none, mechanism
/// family)` — flags from different members/draws that land on the same key
/// collapse to ONE surviving flag (the first seen, in input order).
/// Anchor extraction (see [`extract_new_side_anchor`]) happens HERE,
/// populating `ProbeFlag::anchor` on the surviving flags — `diff` is why
/// this function needs it.
pub fn dedup_flags(flags: Vec<ProbeFlag>, diff: &str) -> (Vec<ProbeFlag>, DedupStats) {
    let raw = flags.len();
    let mut seen: std::collections::HashSet<(String, Option<String>, &'static str)> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    for mut f in flags {
        let anchor = extract_new_side_anchor(&f.charge_text, diff);
        let family = mechanism_family(&f.charge_text);
        let key = (f.bundle_id.clone(), anchor.clone(), family);
        if seen.insert(key) {
            f.anchor = anchor;
            out.push(f);
        }
    }
    let deduped = out.len();
    (out, DedupStats { raw, deduped })
}

// ─── judge prompt + ruling parser ────────────────────────────────────────

const JUDGE_TAIL_INSTRUCTION: &str = "Investigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```\n";

/// Build the judge's prompt: the author's stated case, the code under
/// review, the fact sheet (when non-empty), a MANIFEST of symbols
/// referenced but not defined in the provided code (when non-empty), the
/// flagged item, then the frozen one-fenced-JSON instruction tail.
pub fn judge_prompt(intent: &str, code: &str, facts: &[String], manifest: &[String], charge: &str) -> String {
    let mut out = String::new();
    let intent = intent.trim();
    out.push_str(&format!(
        "Author's stated case for this change:\n{}\n\n",
        if intent.is_empty() { "(no description provided)" } else { intent }
    ));
    out.push_str(&format!("The code under review:\n```\n{code}\n```\n\n"));
    if !facts.is_empty() {
        out.push_str("Fact sheet:\n");
        for f in facts {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    if !manifest.is_empty() {
        out.push_str("Symbols referenced but not defined in the provided code:\n");
        for m in manifest {
            out.push_str(&format!("- {m}\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!("The flagged item:\n{charge}\n\n"));
    out.push_str(JUDGE_TAIL_INSTRUCTION);
    out
}

#[derive(Debug, Deserialize)]
struct RawJudgeRuling {
    ruling: String,
    #[serde(default)]
    decisive_evidence: String,
    #[serde(default)]
    note_for_author: String,
}

/// Candidate JSON substrings, LAST fenced block first (a judge's prose may
/// itself quote code in a fence ahead of its real ruling — trying fences
/// last-to-first, then the whole text, then a first-`{`..last-`}` span,
/// mirrors `dialectic::judge_json_candidates`'s discipline).
fn judge_json_candidates(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else { break };
        let block = &after[..close];
        let inner = block.strip_prefix("json").unwrap_or(block).trim();
        if !inner.is_empty() {
            chunks.push(inner.to_string());
        }
        rest = &after[close + 3..];
    }
    let mut out: Vec<String> = chunks.into_iter().rev().collect();
    let s = text.trim();
    out.push(s.to_string());
    if let (Some(a), Some(b)) = (s.find('{'), s.rfind('}')) {
        if b > a {
            out.push(s[a..=b].to_string());
        }
    }
    out
}

/// Parse a judge reply into `(ruling, decisive_evidence, note_for_author)`.
/// `None` when no candidate carries a recognized `ruling` value — the
/// caller treats that as [`FunnelRuling::Unparsed`].
pub fn parse_judge_ruling(text: &str) -> Option<(FunnelRuling, String, String)> {
    for cand in judge_json_candidates(text) {
        if let Ok(raw) = serde_json::from_str::<RawJudgeRuling>(&cand) {
            let ruling = match raw.ruling.trim().to_ascii_lowercase().as_str() {
                "confirmed" => FunnelRuling::Confirmed,
                "needs_check" => FunnelRuling::NeedsCheck,
                "false_positive" => FunnelRuling::FalsePositive,
                _ => continue,
            };
            return Some((ruling, raw.decisive_evidence, raw.note_for_author));
        }
    }
    None
}

// ─── bundling (packet 3 seam) ─────────────────────────────────────────────

/// One unit the probe seat examines: a bounded code slice plus its fact
/// sheet. Deliberately THIS module's own shape — see the module doc's
/// "Bundling — the packet 3 seam" section for why, and [`bundles_from_diff`]
/// for the reconciliation point.
#[derive(Debug, Clone)]
pub struct BundleInput {
    pub id: String,
    pub fact_family: String,
    pub code: String,
    pub facts: Vec<String>,
    pub manifest: Vec<String>,
}

/// PROVISIONAL bundler standing in for `darkmux_lab::lab::bundle`'s
/// `Bundle`/`BundleSet`/`build_bundles`/`slice_code`/`external_bundles`/
/// `FileSource` (Phase B packet 3), which had not landed on `main` as of
/// this packet. One [`BundleInput`] per changed file — `code` is that
/// file's diff hunks verbatim; `facts`/`manifest` are empty (both need
/// repo-tree reads the real bundler brings). `fact_family` is always
/// `"unscoped"`, so [`BundleSelector::fact_families`] filtering degrades to
/// "no restriction matches" until real fact families exist.
///
/// **Reconciliation seam**: replace this function's body with
/// `build_bundles`/`slice_code`/`external_bundles`/`FileSource` calls once
/// packet 3 lands (either populating `BundleInput` from the real `Bundle`,
/// or promoting `BundleInput` to a thin wrapper around it). Every other
/// piece of this module is written entirely against `BundleInput` and
/// needs no further changes.
fn bundles_from_diff(diff: &str) -> Vec<BundleInput> {
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();
    let flush = |path: &mut Option<String>, lines: &mut Vec<&str>, out: &mut Vec<BundleInput>| {
        if let Some(p) = path.take() {
            if !lines.is_empty() {
                out.push(BundleInput {
                    id: p,
                    fact_family: "unscoped".to_string(),
                    code: lines.join("\n"),
                    facts: Vec::new(),
                    manifest: Vec::new(),
                });
            }
        }
        lines.clear();
    };
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            flush(&mut current_path, &mut current_lines, &mut out);
            current_path = Some(rest.trim().to_string());
        } else if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git") {
            // File-header noise between hunks — not code.
        } else if current_path.is_some() {
            current_lines.push(line);
        }
    }
    flush(&mut current_path, &mut current_lines, &mut out);
    out
}

/// (#1222 Phase B packet 5 reconciliation) `inputs.bundles` when the caller
/// supplied real ones (production), else the provisional [`bundles_from_diff`]
/// (this module's own pre-packet-3 tests only — see [`FunnelInputs::bundles`]).
fn resolve_bundles(inputs: &FunnelInputs) -> Vec<BundleInput> {
    match &inputs.bundles {
        Some(b) => b.clone(),
        None => bundles_from_diff(inputs.diff),
    }
}

/// A staffing with a `bundle_selector` runs only on bundles whose
/// `fact_family` is named in `fact_families` (empty `fact_families` = no
/// restriction), capped at `max_bundles`, prioritizing `"param-flow"`
/// bundles first (stable order otherwise — Rust's `sort_by_key` is a
/// stable sort). A staffing with no selector runs on every bundle.
fn select_bundles_for_staffing<'a>(
    bundles: &'a [BundleInput],
    selector: Option<&BundleSelector>,
) -> Vec<&'a BundleInput> {
    let Some(sel) = selector else {
        return bundles.iter().collect();
    };
    let mut matched: Vec<&BundleInput> = bundles
        .iter()
        .filter(|b| sel.fact_families.is_empty() || sel.fact_families.iter().any(|f| f == &b.fact_family))
        .collect();
    matched.sort_by_key(|b| if b.fact_family == "param-flow" { 0u8 } else { 1u8 });
    if let Some(max) = sel.max_bundles {
        matched.truncate(max as usize);
    }
    matched
}

// ─── dispatch primitive ───────────────────────────────────────────────────

/// One single-shot chat call the funnel wants dispatched. Test closures
/// assert on these fields directly; production wiring turns this into a
/// `darkmux_crew::single_shot::SingleShotRequest` (the caller resolves
/// `base_url`).
pub struct ChatCall<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub temperature: f32,
    pub max_tokens: u32,
}

// ─── funnel inputs ────────────────────────────────────────────────────────

/// Everything [`run_funnel`]/[`run_judge_only`] need beyond the injected
/// `chat`/`cycler`. Role-prompt resolution (`review-probe.md` /
/// `review-judge.md`) is the caller's job — `darkmux-lab` already depends
/// on `darkmux-crew`, but pulling role-manifest resolution INTO this
/// module would couple the pure pipeline to `darkmux_crew::loader`'s
/// filesystem/embedded-role search order for no benefit the caller
/// couldn't provide more simply.
pub struct FunnelInputs<'a> {
    pub case_id: String,
    pub crew: &'a ResolvedCrew,
    pub intent: &'a str,
    pub diff: &'a str,
    pub mode: ExecMode,
    pub probe_system: &'a str,
    pub judge_system: &'a str,
    /// (#1222 Phase B packet 5 reconciliation) Caller-supplied bundles from
    /// the REAL bundler (`darkmux_lab::lab::bundle::build_bundles`/
    /// `external_bundles`, packet 3), already mapped `Bundle` ->
    /// [`BundleInput`] (via `slice_code` for the code text). `None` falls
    /// back to the provisional [`bundles_from_diff`] — kept ONLY so this
    /// module's own tests (written before packet 3 landed) keep working
    /// unchanged. Production callers (`darkmux pr-review run`, packet 5)
    /// always pass `Some` and never invoke the provisional bundler.
    pub bundles: Option<Vec<BundleInput>>,
}

fn fingerprint(judge_identifier: &str, judge_system: &str) -> serde_json::Value {
    serde_json::json!({
        "judge_model": judge_identifier,
        "judge_temperature": JUDGE_TEMPERATURE,
        "judge_persona_blake3": blake3::hash(judge_system.as_bytes()).to_hex().to_string(),
        "protocol": FUNNEL_PROTOCOL,
    })
}

// ─── probe phase ──────────────────────────────────────────────────────────

fn probe_user_message(intent: &str, bundle: &BundleInput) -> String {
    let mut out = String::new();
    let intent = intent.trim();
    if !intent.is_empty() {
        out.push_str(&format!("Change intent: {intent}\n\n"));
    }
    if !bundle.facts.is_empty() {
        out.push_str("Fact sheet:\n");
        for f in &bundle.facts {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    out.push_str("Code:\n```\n");
    out.push_str(&bundle.code);
    out.push_str("\n```\n");
    out
}

/// One probe draw, retried once on empty content, then skipped (`Ok(None)`)
/// — never recorded as a flag. A dispatch-level `Err` propagates
/// immediately (the shared single-shot primitive already carries its own
/// backoff/retry — a second-guessing retry here would be redundant AND
/// would hide a real infra problem behind a "skipped" label).
fn probe_one_draw(
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<Option<(String, u64)>> {
    for _ in 0..2 {
        let call = ChatCall {
            model,
            system,
            user,
            temperature: PROBE_TEMPERATURE,
            max_tokens,
        };
        let reply = chat(&call)?;
        let trimmed = reply.content.trim();
        if !trimmed.is_empty() {
            return Ok(Some((trimmed.to_string(), reply.total_tokens.unwrap_or(0))));
        }
    }
    Ok(None)
}

fn dispatch_probe_staffing(
    s: &ResolvedSeatStaffing,
    bundles: &[BundleInput],
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    flags: &mut Vec<ProbeFlag>,
) -> Result<MemberRecord> {
    let identifier = swap::namespaced_identifier(&s.pm);
    let max_tokens = s.max_tokens.unwrap_or(DEFAULT_PROBE_MAX_TOKENS);
    let selected = select_bundles_for_staffing(bundles, s.selector.as_ref());
    let t0 = Instant::now();
    let mut draws = 0u32;
    let mut tokens = 0u64;
    for bundle in &selected {
        let user = probe_user_message(inputs.intent, bundle);
        for draw in 0..s.k {
            draws += 1;
            if let Some((text, tok)) =
                probe_one_draw(chat, &identifier, inputs.probe_system, &user, max_tokens)?
            {
                tokens += tok;
                flags.push(ProbeFlag {
                    bundle_id: bundle.id.clone(),
                    fact_family: bundle.fact_family.clone(),
                    member: identifier.clone(),
                    draw,
                    charge_text: text,
                    anchor: None,
                });
            }
        }
    }
    Ok(MemberRecord {
        model: identifier,
        seat: "review-probe".to_string(),
        draws,
        wall_ms: t0.elapsed().as_millis() as u64,
        total_tokens: tokens,
    })
}

fn probe_phase(
    bundles: &[BundleInput],
    probes: &[ResolvedSeatStaffing],
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    members: &mut Vec<MemberRecord>,
    mode: ExecMode,
) -> Result<Vec<ProbeFlag>> {
    let mut flags = Vec::new();
    if mode == ExecMode::Parallel {
        for s in probes {
            cycler.ensure_loaded(&s.pm)?;
        }
        for s in probes {
            members.push(dispatch_probe_staffing(s, bundles, inputs, chat, &mut flags)?);
        }
        for s in probes {
            cycler.release(&s.pm)?;
        }
    } else {
        // Sequential (the only other resolved mode by the time this runs —
        // `resolve_mode` never leaves `Auto` unresolved): load member → all
        // its draws → release → next.
        for s in probes {
            cycler.ensure_loaded(&s.pm)?;
            members.push(dispatch_probe_staffing(s, bundles, inputs, chat, &mut flags)?);
            cycler.release(&s.pm)?;
        }
    }
    Ok(flags)
}

// ─── judge phase (double-confirm) ─────────────────────────────────────────

fn run_judge_pass(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (JudgeRecord, u64) {
    let t0 = Instant::now();
    let call = ChatCall {
        model,
        system,
        user: prompt,
        temperature: JUDGE_TEMPERATURE,
        max_tokens,
    };
    match chat(&call) {
        Ok(reply) => {
            let seconds = t0.elapsed().as_secs_f64();
            let tokens = reply.total_tokens.unwrap_or(0);
            match parse_judge_ruling(&reply.content) {
                Some((ruling, decisive_evidence, note_for_author)) => (
                    JudgeRecord { ruling, decisive_evidence, note_for_author, pass, seconds },
                    tokens,
                ),
                None => (
                    JudgeRecord {
                        ruling: FunnelRuling::Unparsed,
                        decisive_evidence: String::new(),
                        note_for_author: String::new(),
                        pass,
                        seconds,
                    },
                    tokens,
                ),
            }
        }
        // A dispatch-level failure is recorded as `Error`, not propagated —
        // one bad judge call must not abort the whole docket (the funnel's
        // job is to be loud PER-FLAG, not to be fragile).
        Err(_) => (
            JudgeRecord {
                ruling: FunnelRuling::Error,
                decisive_evidence: String::new(),
                note_for_author: String::new(),
                pass,
                seconds: t0.elapsed().as_secs_f64(),
            },
            0,
        ),
    }
}

/// One judge pass's resource accounting alongside its surviving record:
/// tokens spent, wall time, and the number of ACTUAL dispatches made
/// (2 when the unparsed-retry fired, else 1) — the member/step telemetry
/// counts real calls, not logical passes (frontier QA minor on this
/// packet's PR).
struct PassOutcome {
    record: JudgeRecord,
    tokens: u64,
    wall_ms: u64,
    calls: u32,
}

/// One judge pass, retried ONCE if the reply was [`FunnelRuling::Unparsed`]
/// (the retry keeps the same `pass` number — a retried pass-1 is still
/// pass-1, just a second attempt at it). Still unparsed after the retry:
/// the retry's record survives (the first attempt's record is discarded,
/// not hidden — it added no information a clean retry didn't already
/// supersede). Tokens/wall/calls account for BOTH attempts.
fn judge_pass_with_retry(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> PassOutcome {
    let t0 = Instant::now();
    let (r1, t1) = run_judge_pass(pass, model, system, prompt, max_tokens, chat);
    if r1.ruling == FunnelRuling::Unparsed {
        let (r2, t2) = run_judge_pass(pass, model, system, prompt, max_tokens, chat);
        PassOutcome {
            record: r2,
            tokens: t1 + t2,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 2,
        }
    } else {
        PassOutcome {
            record: r1,
            tokens: t1,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 1,
        }
    }
}

/// One flag's full double-confirm outcome, with per-pass resource
/// accounting so the envelope's `judge-pass1` / `judge-pass2` step rows
/// carry HONEST per-pass wall times (an all-confirm docket previously
/// booked its whole elapsed under pass-2, reading as pass-1 = 0ms).
struct JudgeOutcome {
    pass1: JudgeRecord,
    pass2: Option<JudgeRecord>,
    tier: Tier,
    demoted_by_pass2: bool,
    tokens: u64,
    pass1_ms: u64,
    pass2_ms: u64,
    /// Actual dispatches made across both passes, unparsed retries
    /// included.
    calls: u32,
}

/// The double-confirm state machine for one flag: pass-1 (with the
/// unparsed-retry above) always runs; a `confirmed` pass-1 gets a pass-2
/// (also with the retry) — agreement → [`Tier::Confirmed`]; ANY other
/// pass-2 outcome (needs_check, false_positive, unparsed, error) demotes
/// to [`Tier::NeedsCheck`], never silently to `confirmed`. A non-confirmed
/// pass-1 needs no pass-2: `needs_check` stays `NeedsCheck`; everything
/// else (`false_positive`, `unparsed`, `error`) is `Archived` — the
/// specific ruling is still preserved on the record (loud), just tiered
/// out of the author-facing report.
fn judge_one_flag(
    prompt: &str,
    model: &str,
    system: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> JudgeOutcome {
    let p1 = judge_pass_with_retry(1, model, system, prompt, max_tokens, chat);
    match p1.record.ruling {
        FunnelRuling::Confirmed => {
            let p2 = judge_pass_with_retry(2, model, system, prompt, max_tokens, chat);
            let (tier, demoted) = if p2.record.ruling == FunnelRuling::Confirmed {
                (Tier::Confirmed, false)
            } else {
                (Tier::NeedsCheck, true)
            };
            JudgeOutcome {
                pass1: p1.record,
                pass2: Some(p2.record),
                tier,
                demoted_by_pass2: demoted,
                tokens: p1.tokens + p2.tokens,
                pass1_ms: p1.wall_ms,
                pass2_ms: p2.wall_ms,
                calls: p1.calls + p2.calls,
            }
        }
        FunnelRuling::NeedsCheck => JudgeOutcome {
            tier: Tier::NeedsCheck,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            pass1: p1.record,
            pass2: None,
        },
        FunnelRuling::FalsePositive | FunnelRuling::Unparsed | FunnelRuling::Error => JudgeOutcome {
            tier: Tier::Archived,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            pass1: p1.record,
            pass2: None,
        },
    }
}

// ─── shared finish (probe→dedup→judge→envelope), reused by run_judge_only ─

fn finish_funnel(
    mut env: FunnelEnvelope,
    raw_flags: Vec<ProbeFlag>,
    bundles: &[BundleInput],
    inputs: &FunnelInputs,
    judge: &ResolvedSeatStaffing,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
) -> Result<FunnelEnvelope> {
    env.raw_flags = raw_flags.len();

    let t_dedup = Instant::now();
    let (deduped, _stats) = dedup_flags(raw_flags, inputs.diff);
    env.steps.push(StepRecord {
        step_id: "dedup".to_string(),
        kind: "procedural".to_string(),
        items_in: env.raw_flags,
        items_out: deduped.len(),
        wall_ms: t_dedup.elapsed().as_millis() as u64,
    });
    env.deduped_flags = deduped.len();

    let judge_identifier = swap::namespaced_identifier(&judge.pm);
    let judge_max_tokens = judge.max_tokens.unwrap_or(DEFAULT_JUDGE_MAX_TOKENS);

    cycler.ensure_loaded(&judge.pm)?;
    let mut judged = Vec::with_capacity(deduped.len());
    let mut pass1_ms = 0u64;
    let mut pass2_ms = 0u64;
    let mut pass2_flags = 0usize;
    let mut judge_calls = 0u32;
    let mut judge_tokens = 0u64;
    for flag in &deduped {
        let bundle = bundles.iter().find(|b| b.id == flag.bundle_id);
        let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
        let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
        let manifest: &[String] = bundle.map(|b| b.manifest.as_slice()).unwrap_or_default();
        let prompt = judge_prompt(inputs.intent, code, facts, manifest, &flag.charge_text);
        let outcome =
            judge_one_flag(&prompt, &judge_identifier, inputs.judge_system, judge_max_tokens, chat);
        judge_tokens += outcome.tokens;
        judge_calls += outcome.calls;
        pass1_ms += outcome.pass1_ms;
        pass2_ms += outcome.pass2_ms;
        if outcome.pass2.is_some() {
            pass2_flags += 1;
        }
        judged.push(JudgedFlag {
            flag: flag.clone(),
            pass1: outcome.pass1,
            pass2: outcome.pass2,
            tier: outcome.tier,
            demoted_by_pass2: outcome.demoted_by_pass2,
        });
    }
    cycler.release(&judge.pm)?;

    env.members.push(MemberRecord {
        model: judge_identifier,
        seat: "review-judge".to_string(),
        // Actual dispatches, unparsed retries included — never fewer calls
        // than the operator paid for.
        draws: judge_calls,
        wall_ms: pass1_ms + pass2_ms,
        total_tokens: judge_tokens,
    });
    env.steps.push(StepRecord {
        step_id: "judge-pass1".to_string(),
        kind: "dispatch".to_string(),
        items_in: deduped.len(),
        items_out: deduped.len(),
        wall_ms: pass1_ms,
    });
    if pass2_flags > 0 {
        env.steps.push(StepRecord {
            step_id: "judge-pass2".to_string(),
            kind: "dispatch".to_string(),
            items_in: pass2_flags,
            items_out: pass2_flags,
            wall_ms: pass2_ms,
        });
    }

    env.confirmed = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
    env.needs_check = judged.iter().filter(|j| j.tier == Tier::NeedsCheck).count();
    env.archived = judged.iter().filter(|j| j.tier == Tier::Archived).count();

    // The judge-dead honesty gate (#1222 packet 5 review): per-flag judge
    // failures are deliberately swallowed to `Error`/`Unparsed` →
    // `Tier::Archived` (one bad call must not abort the docket), but when
    // NO flag got a usable pass-1 ruling the whole judge phase produced no
    // signal — confirmed=0/needs_check=0 would render downstream as an
    // honest-looking "none confirmed" green comment while the judge was
    // dead or off-contract the entire run. Mark the envelope degenerate so
    // synthesis routes it to "degraded" (the workflow's exit-1 path). A
    // genuine all-false-positive docket has usable rulings and keeps the
    // honest comment.
    let usable = judged
        .iter()
        .filter(|j| {
            matches!(
                j.pass1.ruling,
                FunnelRuling::Confirmed | FunnelRuling::NeedsCheck | FunnelRuling::FalsePositive
            )
        })
        .count();
    if !judged.is_empty() && usable == 0 {
        env.degenerate = Some(format!(
            "judge produced no usable ruling on any of {} flags (all errored/unparsed)",
            judged.len()
        ));
    }

    env.flags = deduped;
    env.judged = judged;
    Ok(env)
}

// ─── the driver ───────────────────────────────────────────────────────────

/// Run the full funnel: bundles → probe(k draws × seat) → dedup →
/// double-confirm judge → envelope. `chat` performs one single-shot
/// dispatch and returns its reply (the closure owns model/base-URL
/// resolution — tests script it; production wiring calls
/// `darkmux_crew::single_shot::single_shot_chat`). `cycler` loads/releases
/// models around the dispatches (production: [`LmsCycler`]; tests: a
/// recording mock).
pub fn run_funnel(
    inputs: &FunnelInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
) -> Result<FunnelEnvelope> {
    let (probes, judge) = validate_funnel_crew(inputs.crew)?;
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = FunnelEnvelope {
        case_id: inputs.case_id.clone(),
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Stamped up front so DEGENERATE envelopes (zero bundles / zero
        // flags) carry the same comparability key as a full run — a
        // Null fingerprint on an early return would make the degenerate
        // record untraceable to its judge config.
        fingerprint: fingerprint(&swap::namespaced_identifier(&judge.pm), inputs.judge_system),
        ..Default::default()
    };
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: 1,
        items_out: bundles.len(),
        wall_ms: bundle_ms,
    });
    if bundles.is_empty() {
        env.degenerate = Some("no bundles produced from the diff".to_string());
        return Ok(env);
    }

    let t_probe = Instant::now();
    let raw_flags = probe_phase(&bundles, probes, inputs, &mut chat, cycler, &mut env.members, mode)
        .context("review funnel: probe phase")?;
    env.steps.push(StepRecord {
        step_id: "probe".to_string(),
        kind: "dispatch".to_string(),
        items_in: bundles.len(),
        items_out: raw_flags.len(),
        wall_ms: t_probe.elapsed().as_millis() as u64,
    });
    if raw_flags.is_empty() {
        env.raw_flags = 0;
        env.degenerate = Some("zero flags from all probe draws — never a silent pass".to_string());
        return Ok(env);
    }

    finish_funnel(env, raw_flags, &bundles, inputs, judge, &mut chat, cycler)
}

/// Re-judge a previously-recorded flag list without re-running the probe
/// (the `--charges-file` entry point). Still dedups (a hand-edited or
/// concatenated charges file may carry raw, undeduped flags) and still
/// rebuilds bundles from `inputs.diff` — the judge needs the code each
/// flag's `bundle_id` refers to, and flags alone don't carry it.
pub fn run_judge_only(
    flags: Vec<ProbeFlag>,
    inputs: &FunnelInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
) -> Result<FunnelEnvelope> {
    let (probes, judge) = validate_funnel_crew(inputs.crew)?;
    // Judge-only runs one model, so the mode is telemetry, not behavior —
    // but the envelope still records the CALLER's resolved mode rather
    // than a hardcoded label, so a judge-only re-run of a parallel funnel
    // doesn't misreport its provenance.
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = FunnelEnvelope {
        case_id: inputs.case_id.clone(),
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Same up-front stamp as `run_funnel` — degenerate (zero-flag)
        // envelopes carry the comparability key too.
        fingerprint: fingerprint(&swap::namespaced_identifier(&judge.pm), inputs.judge_system),
        ..Default::default()
    };
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: 1,
        items_out: bundles.len(),
        wall_ms: bundle_ms,
    });
    if flags.is_empty() {
        env.degenerate = Some("--charges-file carried zero flags".to_string());
        return Ok(env);
    }

    finish_funnel(env, flags, &bundles, inputs, judge, &mut chat, cycler)
}

// ═══════════════════════════════════════════════════════════════════════
// tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    // ── fixtures ────────────────────────────────────────────────────

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    fn pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: 32_000, ..Default::default() }
    }

    fn staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: pm(model),
            k,
            max_tokens: None,
            selector: None,
        }
    }

    fn crew_with(seats: Vec<(&str, Vec<ResolvedSeatStaffing>)>) -> ResolvedCrew {
        let mut m = BTreeMap::new();
        for (k, v) in seats {
            m.insert(k.to_string(), v);
        }
        ResolvedCrew { name: "test-crew".to_string(), seats: m }
    }

    fn valid_crew() -> ResolvedCrew {
        crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ])
    }

    fn flag(bundle_id: &str, member: &str, draw: u32, charge_text: &str) -> ProbeFlag {
        ProbeFlag {
            bundle_id: bundle_id.to_string(),
            fact_family: "unscoped".to_string(),
            member: member.to_string(),
            draw,
            charge_text: charge_text.to_string(),
            anchor: None,
        }
    }

    /// Recording [`ModelCycler`] mock: pushes `"load:<id>"` / `"release:<id>"`
    /// into a shared log so cycling ORDER is assertable.
    struct RecordingCycler {
        log: Vec<String>,
    }
    impl RecordingCycler {
        fn new() -> Self {
            Self { log: Vec::new() }
        }
    }
    impl ModelCycler for RecordingCycler {
        fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("load:{}", pm.id));
            Ok(())
        }
        fn release(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("release:{}", pm.id));
            Ok(())
        }
    }

    fn reply(content: &str) -> SingleShotReply {
        SingleShotReply {
            content: content.to_string(),
            total_tokens: Some(10),
            model: None,
        }
    }

    // ── judge ruling parser ──────────────────────────────────────────

    #[test]
    fn parse_judge_ruling_last_fence_wins() {
        let text = "Weighing the flag: the code quotes\n```\nconst days = Math.min(raw, 30)\n```\nwhich looks relevant.\n\n```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"the clamp is bypassed\", \"note_for_author\": \"real bug\"}\n```\n";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::Confirmed);
        assert_eq!(evidence, "the clamp is bypassed");
        assert_eq!(note, "real bug");
    }

    #[test]
    fn parse_judge_ruling_prose_wrapped_still_parses() {
        let text = "Some long reasoning about the code goes here, spanning several\nsentences before the verdict.\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"input is clamped upstream\", \"note_for_author\": \"no action needed\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::FalsePositive);
    }

    #[test]
    fn parse_judge_ruling_needs_check_and_case_insensitive() {
        let text = "```json\n{\"ruling\": \"NEEDS_CHECK\", \"decisive_evidence\": \"outside the bundle\", \"note_for_author\": \"verify manually\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::NeedsCheck);
    }

    #[test]
    fn parse_judge_ruling_unparsed_on_garbage() {
        assert!(parse_judge_ruling("I could not determine a verdict.").is_none());
        assert!(parse_judge_ruling("").is_none());
        // Off-contract ruling value never matches — falls through to None.
        assert!(parse_judge_ruling("```json\n{\"ruling\": \"maybe\"}\n```").is_none());
    }

    // ── dedup ─────────────────────────────────────────────────────────

    #[test]
    fn dedup_same_anchor_and_family_collapses_across_members_and_draws() {
        let flags = vec![
            flag("b1", "member-a", 0, "The clamp at `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 1, "`const end = start.plus(30)` double-counts the boundary day."),
            flag("b1", "member-a", 2, "`const end = start.plus(30)` looks off by one."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.raw, 3);
        assert_eq!(stats.deduped, 1);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
    }

    #[test]
    fn dedup_different_mechanism_family_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "`const end = start.plus(30)` double counts the boundary."),
            flag("b1", "member-b", 0, "`const end = start.plus(30)` — timezone handling is wrong here."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different mechanism family must survive dedup");
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedup_no_anchor_flags_dedup_by_family_only() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk on the branch."),
            flag("b1", "member-b", 0, "A NaN can reach this path unchecked."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 1, "no-anchor flags in the same family collapse");
        assert!(deduped[0].anchor.is_none());
    }

    #[test]
    fn dedup_no_anchor_different_bundle_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk."),
            flag("b2", "member-a", 0, "This is also a null pointer risk."),
        ];
        let (_deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different bundle_id never collapses");
    }

    /// Frontier QA should-fix on this packet's PR: substring matching
    /// classified "tenant", "covenant", and "finance" as `null/nan` (all
    /// contain "nan"), so two DISTINCT unanchored charges on a billing
    /// corpus keyed identically and one real defect was silently dropped
    /// in dedup. Word-boundary matching must not fire on those words.
    #[test]
    fn mechanism_family_does_not_substring_match_inside_words() {
        assert_eq!(
            mechanism_family("The tenant covenant check is skipped for finance accounts."),
            "other",
            "'tenant'/'covenant'/'finance' must not classify as null/nan"
        );
        // The real keywords still classify as whole tokens.
        assert_eq!(mechanism_family("A null value reaches this branch."), "null/nan");
        assert_eq!(mechanism_family("NaN propagates into the total."), "null/nan");
        assert_eq!(mechanism_family("None is returned on the error path."), "null/nan");
        // Punctuation-adjacent tokens still match (tokenizer strips it).
        assert_eq!(mechanism_family("Uses `Date.now()` for the cutoff."), "timezone/ambient-time");
        // "nonexistent" must not token-match "none".
        assert_eq!(mechanism_family("References a nonexistent column."), "other");
    }

    /// Two unanchored flags on the SAME bundle whose charges describe
    /// genuinely different mechanisms must both survive dedup — the
    /// substring bug collapsed them (both misclassified `null/nan`) and
    /// silently dropped a real defect.
    #[test]
    fn dedup_distinct_mechanisms_same_bundle_both_survive() {
        let flags = vec![
            flag(
                "b1",
                "member-a",
                0,
                "The tenant covenant check is skipped when the finance flag is set.",
            ),
            flag("b1", "member-b", 0, "A null value reaches the accumulator unguarded."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "genuinely different mechanisms in one bundle must both survive"
        );
        assert_eq!(deduped.len(), 2);
    }

    // ── double-confirm state machine ────────────────────────────────

    fn scripted_chat(
        script: RefCell<Vec<&'static str>>,
    ) -> impl FnMut(&ChatCall) -> Result<SingleShotReply> {
        move |_call: &ChatCall| {
            let mut s = script.borrow_mut();
            if s.is_empty() {
                return Ok(reply(""));
            }
            Ok(reply(s.remove(0)))
        }
    }

    const CONFIRM_JSON: &str = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const FP_JSON: &str = "```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const NEEDS_CHECK_JSON: &str = "```json\n{\"ruling\": \"needs_check\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";

    #[test]
    fn double_confirm_confirm_then_confirm_is_confirmed_tier() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "one clean dispatch per pass");
    }

    #[test]
    fn double_confirm_confirm_then_false_positive_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::FalsePositive);
        assert_eq!(o.tier, Tier::NeedsCheck, "disagreement demotes, never ships as confirmed");
        assert!(o.demoted_by_pass2);
    }

    #[test]
    fn double_confirm_pass1_needs_check_skips_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![NEEDS_CHECK_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::NeedsCheck);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no pass-2 dispatch, no pass-2 wall time");
    }

    #[test]
    fn double_confirm_pass1_false_positive_archives_without_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::FalsePositive);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
    }

    #[test]
    fn double_confirm_unparsed_retries_then_archives() {
        // Two garbage replies: pass-1 attempt, retry — still unparsed.
        let mut chat = scripted_chat(RefCell::new(vec!["no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Unparsed);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "the unparsed retry is a real dispatch and is counted");
    }

    #[test]
    fn double_confirm_unparsed_retry_recovers() {
        // First attempt garbage, retry succeeds — the retry's ruling wins.
        let mut chat = scripted_chat(RefCell::new(vec!["garbage", CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed, "the retry's clean ruling survives");
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert_eq!(o.calls, 3, "pass-1 attempt + retry + pass-2 = three real dispatches");
    }

    // ── empty probe draw ─────────────────────────────────────────────

    #[test]
    fn probe_one_draw_empty_content_retries_once_then_skips() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(""))
        };
        let out = probe_one_draw(&mut chat, "m", "sys", "user", 100).expect("no dispatch error");
        assert!(out.is_none(), "still empty after retry -> skipped, not a flag");
        assert_eq!(calls, 2, "exactly one retry (two total attempts)");
    }

    #[test]
    fn probe_one_draw_recovers_on_retry() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            if calls == 1 {
                Ok(reply(""))
            } else {
                Ok(reply("a real defect description"))
            }
        };
        let out = probe_one_draw(&mut chat, "m", "sys", "user", 100).unwrap();
        assert_eq!(out.unwrap().0, "a real defect description");
        assert_eq!(calls, 2);
    }

    #[test]
    fn probe_one_draw_propagates_dispatch_error() {
        let mut chat = |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = probe_one_draw(&mut chat, "m", "sys", "user", 100).unwrap_err();
        assert!(err.to_string().contains("network down"));
    }

    // ── selector filtering ───────────────────────────────────────────

    #[test]
    fn selector_filters_by_fact_family() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel =
            BundleSelector { fact_families: vec!["auth".to_string()], ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "a");
    }

    #[test]
    fn selector_no_selector_runs_every_bundle() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), facts: vec![], manifest: vec![] },
        ];
        assert_eq!(select_bundles_for_staffing(&bundles, None).len(), 2);
    }

    #[test]
    fn selector_prioritizes_param_flow_and_respects_max_bundles() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "c".into(), fact_family: "other".into(), code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { max_bundles: Some(2), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].id, "b", "param-flow bundle is prioritized first");
    }

    // ── crew seat-requirement validation ────────────────────────────

    #[test]
    fn validate_funnel_crew_happy_path() {
        let crew = valid_crew();
        let (probes, judge) = validate_funnel_crew(&crew).expect("valid");
        assert_eq!(probes.len(), 1);
        assert_eq!(judge.pm.id, "judge-model");
    }

    #[test]
    fn validate_funnel_crew_missing_probe_seat_rejected() {
        let crew = crew_with(vec![("review-judge", vec![staffing("fast", "j", 1)])]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_funnel_crew_empty_probe_staffing_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![]),
            ("review-judge", vec![staffing("fast", "j", 1)]),
        ]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_funnel_crew_missing_judge_seat_rejected() {
        let crew = crew_with(vec![("review-probe", vec![staffing("fast", "p", 1)])]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-judge"));
    }

    #[test]
    fn validate_funnel_crew_multiple_judge_staffings_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "p", 1)]),
            ("review-judge", vec![staffing("fast", "j1", 1), staffing("fast", "j2", 1)]),
        ]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("EXACTLY 1"));
    }

    // ── sequential cycling order ─────────────────────────────────────

    #[test]
    fn sequential_cycling_loads_and_releases_each_member_before_the_next_then_judge_last() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert!(env.confirmed + env.needs_check + env.archived > 0 || env.deduped_flags == 0);
        let log = &cycler.log;
        let a_load = log.iter().position(|s| s == "load:member-a").unwrap();
        let a_release = log.iter().position(|s| s == "release:member-a").unwrap();
        let b_load = log.iter().position(|s| s == "load:member-b").unwrap();
        let b_release = log.iter().position(|s| s == "release:member-b").unwrap();
        let judge_load = log.iter().position(|s| s == "load:judge-model").unwrap();
        assert!(a_load < a_release, "member A releases before member B loads");
        assert!(a_release < b_load, "member A fully cycled before member B starts");
        assert!(b_load < b_release);
        assert!(b_release < judge_load, "judge loads last, after every probe member");
    }

    // ── envelope counts + steps consistency ──────────────────────────

    #[test]
    fn envelope_counts_and_steps_are_internally_consistent() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                // two probe draws (k=2), both find the same defect
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert!(env.degenerate.is_none());
        assert_eq!(env.bundles, 1, "one changed file in the fixture diff");
        assert_eq!(env.raw_flags, 2, "k=2 draws, both non-empty");
        assert_eq!(env.deduped_flags, 1, "identical anchor+family collapses to one");
        assert_eq!(env.flags.len(), env.deduped_flags);
        assert_eq!(env.judged.len(), env.deduped_flags);
        assert_eq!(
            env.confirmed + env.needs_check + env.archived,
            env.judged.len(),
            "every judged flag lands in exactly one tier"
        );
        let step_ids: Vec<&str> = env.steps.iter().map(|s| s.step_id.as_str()).collect();
        assert!(step_ids.contains(&"bundle"));
        assert!(step_ids.contains(&"probe"));
        assert!(step_ids.contains(&"dedup"));
        assert!(step_ids.contains(&"judge-pass1"));
        assert!(!env.members.is_empty());
        assert!(env.fingerprint.get("protocol").is_some());
    }

    #[test]
    fn degenerate_zero_bundles_never_silently_passes() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "",
            diff: "",
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("unused"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert!(env.degenerate.is_some());
        assert_eq!(env.bundles, 0);
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a degenerate envelope still carries the comparability fingerprint"
        );
    }

    #[test]
    fn degenerate_zero_flags_never_silently_passes() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        // Every probe draw comes back empty — retried, then skipped.
        let mut chat = |_call: &ChatCall| Ok(reply(""));
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert!(env.degenerate.is_some());
        assert_eq!(env.raw_flags, 0);
        assert_eq!(env.judged.len(), 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a zero-flag envelope still carries the comparability fingerprint"
        );
    }

    #[test]
    fn degenerate_all_unparsed_judge_never_renders_as_a_clean_pass() {
        // The judge-dead honesty gate (#1222 packet 5 review): per-flag
        // judge failures are swallowed to Unparsed/Error -> Archived, so a
        // dead or off-contract judge used to produce confirmed=0 /
        // needs_check=0 / degenerate=None — indistinguishable downstream
        // from a genuinely clean "none confirmed" run. Flags judged but
        // ZERO usable pass-1 rulings must mark the envelope degenerate.
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Every judge call (pass-1 AND its unparsed-retry) is
                // off-contract prose — no fenced JSON ruling.
                Ok(reply("I could not reach a verdict on this."))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert_eq!(env.judged.len(), 1, "the flag WAS judged (archived), not dropped");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 1);
        let note = env.degenerate.expect("all-unparsed judge must mark the envelope degenerate");
        assert!(note.contains("no usable ruling"), "{note}");
        assert!(note.contains("1 flags"), "names how many flags got nothing: {note}");
    }

    #[test]
    fn genuine_all_false_positive_docket_is_not_degenerate() {
        // The counterpart: a judge that RULED (false_positive) on every
        // flag produced real signal — zero confirms is then an honest
        // outcome, not a degenerate one.
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(FP_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.archived, 1);
        assert!(
            env.degenerate.is_none(),
            "a ruled-on docket is honest signal, never degenerate: {:?}",
            env.degenerate
        );
    }

    // ── run_judge_only ────────────────────────────────────────────────

    #[test]
    fn run_judge_only_skips_probe_and_judges_supplied_flags() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(CONFIRM_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler).expect("runs");
        assert_eq!(env.raw_flags, 1);
        assert_eq!(env.judged.len(), 1);
        assert!(!cycler.log.iter().any(|s| s.contains("probe-model")), "probe never dispatched");
        assert_eq!(
            env.mode, "sequential",
            "the envelope records the caller's resolved mode, not a hardcoded label"
        );
    }

    #[test]
    fn run_judge_only_records_the_callers_parallel_mode() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(FP_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler).expect("runs");
        assert_eq!(env.mode, "parallel", "a judge-only re-run of a parallel funnel keeps its provenance");
    }

    // ── ExecMode auto-resolution ──────────────────────────────────────

    #[test]
    fn resolve_auto_stays_parallel_within_budget_and_falls_back_sequential_over() {
        let hw_small = darkmux_hardware::HardwareSpec {
            platform: darkmux_hardware::Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 16,
            physical_cores: 8,
            performance_cores: None,
            efficiency_cores: None,
            has_unified_memory: true,
        };
        assert_eq!(resolve_auto(1, &hw_small), ExecMode::Parallel);
        assert_eq!(resolve_auto(2, &hw_small), ExecMode::Sequential, "small tier budget is 1");
        let hw_xl = darkmux_hardware::HardwareSpec { total_ram_gb: 128, ..hw_small };
        assert_eq!(resolve_auto(3, &hw_xl), ExecMode::Parallel, "xl tier budget is 3");
        assert_eq!(resolve_auto(4, &hw_xl), ExecMode::Sequential);
    }

    // ── judge_prompt shape ─────────────────────────────────────────────

    #[test]
    fn judge_prompt_includes_all_sections_when_present() {
        let p = judge_prompt(
            "Add billing window",
            "const end = start.plus(30)",
            &["fact one".to_string()],
            &["helperFn".to_string()],
            "the boundary is double-counted",
        );
        assert!(p.contains("Add billing window"));
        assert!(p.contains("const end = start.plus(30)"));
        assert!(p.contains("Fact sheet:"));
        assert!(p.contains("fact one"));
        assert!(p.contains("Symbols referenced but not defined in the provided code:"));
        assert!(p.contains("helperFn"));
        assert!(p.contains("the boundary is double-counted"));
        assert!(p.contains("```json"));
        assert!(p.contains("\"ruling\""));
    }

    #[test]
    fn judge_prompt_omits_bare_sections() {
        let p = judge_prompt("", "code", &[], &[], "charge");
        assert!(p.contains("(no description provided)"));
        assert!(!p.contains("Fact sheet:"));
        assert!(!p.contains("Symbols referenced but not defined"));
    }

    // ── bundles_from_diff (provisional bundler) ────────────────────────

    #[test]
    fn bundles_from_diff_one_bundle_per_changed_file() {
        let bundles = bundles_from_diff(DIFF);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].id, "billing.ts");
        assert!(bundles[0].code.contains("const end = start.plus(30)"));
    }

    // ═══════════════════════════════════════════════════════════════
    // Phase B coverage packet (#1222) — protocol/dedup/telemetry edges
    // ═══════════════════════════════════════════════════════════════

    // ── judge ruling parser: multi-fence, extras, null values ─────────

    /// A judge reply can carry more than one fenced JSON block (e.g. a
    /// judge that reasons out loud, states a tentative verdict, then
    /// revises it). `judge_json_candidates` tries fences LAST-to-FIRST, so
    /// the LAST fenced block in the text must win — an earlier, superseded
    /// verdict must never leak through.
    #[test]
    fn parse_judge_ruling_multiple_valid_fences_last_wins() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"first pass\", \"note_for_author\": \"n1\"}\n```\nOn reflection, revising the verdict:\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"second pass\", \"note_for_author\": \"n2\"}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::FalsePositive, "the LAST fenced JSON wins, not the first");
        assert_eq!(evidence, "second pass", "the first fence's evidence must be ignored");
        assert_eq!(note, "n2");
    }

    /// `RawJudgeRuling` has no `deny_unknown_fields` — extra keys a judge
    /// bolts onto its ruling (confidence scores, nested detail) must not
    /// break parsing.
    #[test]
    fn parse_judge_ruling_tolerates_unknown_extra_fields() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\", \"confidence\": 0.87, \"extra\": {\"nested\": true}}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("unknown fields must not break parsing");
        assert_eq!(ruling, FunnelRuling::Confirmed);
        assert_eq!(evidence, "e");
        assert_eq!(note, "n");
    }

    /// `decisive_evidence`/`note_for_author` are `String`, not
    /// `Option<String>`, and `ruling` is a plain `String` matched against a
    /// closed set. A JSON `null` on any of these is a TYPE mismatch for
    /// serde (not a missing-field default), so every candidate in
    /// `judge_json_candidates` fails to deserialize and the whole reply
    /// falls through to `None` (Unparsed) rather than null silently
    /// standing in for an empty string or a bogus ruling.
    #[test]
    fn parse_judge_ruling_null_values_fail_to_parse_not_treated_as_empty() {
        let evidence_null = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": null, \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(evidence_null).is_none(),
            "null decisive_evidence must not silently parse as an empty string"
        );

        let ruling_null = "```json\n{\"ruling\": null, \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(ruling_null).is_none(),
            "a null ruling value must not silently match a variant"
        );
    }

    // ── dedup: whitespace-only anchor variance ─────────────────────────

    /// `extract_new_side_anchor` NORMALIZES (marker-strip + whitespace
    /// collapse) only to decide whether a quoted span is a legitimate
    /// anchor — the stored/returned anchor is the model's VERBATIM quote.
    /// Two flags whose backtick-quoted anchors are semantically identical
    /// but differ in internal whitespace both validate against the diff
    /// (via the collapsed fallback), yet the raw strings differ, so the
    /// dedup key `(bundle_id, anchor, family)` differs and they do NOT
    /// collapse. Characterizes current behavior — not asserted as a bug,
    /// since `dedup_flags`'s doc makes no whitespace-insensitivity promise
    /// on the key itself.
    #[test]
    fn dedup_anchors_differing_only_by_internal_whitespace_do_not_collapse() {
        let flags = vec![
            flag("b1", "member-a", 0, "The `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 0, "The `const  end = start.plus(30)` double counts."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "whitespace-differing anchors both validate against the diff but do not share a dedup key"
        );
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
        assert_eq!(
            deduped[1].anchor.as_deref(),
            Some("const  end = start.plus(30)"),
            "the stored anchor is the model's verbatim quote, not the normalized/collapsed form"
        );
    }

    // ── mechanism_family word-boundary regression suite (expanded) ─────

    /// Expands the substring-vs-token regression beyond the "tenant" case
    /// already covered: every table keyword must match as a whole token
    /// and must NOT fire on a longer/different word that merely contains
    /// it as a substring.
    #[test]
    fn mechanism_family_word_boundary_regression_suite() {
        // Real keywords match as standalone tokens.
        assert_eq!(mechanism_family("This has an async issue."), "async/await");
        assert_eq!(mechanism_family("Watch the dst transition."), "timezone/ambient-time");
        assert_eq!(mechanism_family("Provenance information is missing."), "provenance/sibling");
        assert_eq!(mechanism_family("Check the arg count."), "arity/param");

        // Longer/different words that merely CONTAIN a keyword as a
        // substring must not false-match — word-boundary, never substring.
        assert_eq!(
            mechanism_family("The function is asynchronous by design."),
            "other",
            "'asynchronous' must not token-match 'async'"
        );
        assert_eq!(
            mechanism_family("A windstorm knocked out power."),
            "other",
            "'windstorm' must not token-match 'dst'"
        );
        assert_eq!(
            mechanism_family("This proves the claim is unproven."),
            "other",
            "'proves'/'unproven' must not token-match 'provenance'"
        );
        assert_eq!(
            mechanism_family("The margarine recipe changed."),
            "other",
            "'margarine' must not token-match 'arg'"
        );
    }

    // ── double-confirm: pass-2 unparsed ─────────────────────────────────

    /// A `confirmed` pass-1 followed by a pass-2 that stays `Unparsed`
    /// (even after its own retry) is still ANY-other-than-confirmed —
    /// `judge_one_flag`'s doc is explicit this must demote, never silently
    /// promote to `Confirmed` on a garbled second call.
    #[test]
    fn double_confirm_confirm_then_pass2_unparsed_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, "no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, FunnelRuling::Unparsed);
        assert_eq!(o.tier, Tier::NeedsCheck, "an unparsed pass-2 must demote, never silently confirm");
        assert!(o.demoted_by_pass2);
        assert_eq!(o.calls, 3, "pass-1 (1 call) + pass-2 attempt + pass-2's own unparsed-retry (2 calls)");
    }

    // ── ModelCycler load-failure propagation ────────────────────────────

    /// Recording [`ModelCycler`] mock that fails `ensure_loaded` for one
    /// named model id, so cycling order AND the abort point are both
    /// assertable.
    struct FailingLoadCycler {
        fail_on: String,
        log: Vec<String>,
    }
    impl FailingLoadCycler {
        fn new(fail_on: &str) -> Self {
            Self { fail_on: fail_on.to_string(), log: Vec::new() }
        }
    }
    impl ModelCycler for FailingLoadCycler {
        fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("load:{}", pm.id));
            if pm.id == self.fail_on {
                bail!("simulated load failure for {}", pm.id);
            }
            Ok(())
        }
        fn release(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("release:{}", pm.id));
            Ok(())
        }
    }

    /// Sequential mode loads/dispatches/releases one member fully before
    /// moving to the next. A load failure on the SECOND member aborts the
    /// whole probe phase via `?` — the first member's already-gathered
    /// flags are discarded (never surfaced, since `run_funnel` returns
    /// `Err` and the partially-built envelope is dropped), the failed
    /// member is never released, and the judge never loads at all.
    #[test]
    fn probe_phase_sequential_load_failure_aborts_remaining_members_and_drops_prior_flags() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let err = run_funnel(&inputs, &mut chat, &mut cycler).unwrap_err();
        assert!(
            err.to_string().contains("probe phase"),
            "run_funnel wraps the propagated load error with phase context"
        );
        assert_eq!(
            cycler.log,
            vec!["load:member-a", "release:member-a", "load:member-b"],
            "member-a fully cycled before member-b's load failure aborts — no release for member-b, no judge load at all"
        );
    }

    /// Parallel mode loads EVERY member up front, before dispatching any of
    /// them. A load failure partway through that up-front loop aborts
    /// before a single dispatch happens — member-a's draw never runs even
    /// though its own load succeeded.
    #[test]
    fn probe_phase_parallel_load_failure_aborts_before_any_dispatch() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut dispatch_count = 0u32;
        let mut chat = |_call: &ChatCall| {
            dispatch_count += 1;
            Ok(reply("a real defect `const end = start.plus(30)`"))
        };
        let err = run_funnel(&inputs, &mut chat, &mut cycler).unwrap_err();
        assert!(err.to_string().contains("probe phase"));
        assert_eq!(
            dispatch_count, 0,
            "parallel mode loads every member before dispatching any — the failure aborts before member-a's draw ever runs"
        );
        assert_eq!(cycler.log, vec!["load:member-a", "load:member-b"]);
    }

    // ── selector edge cases ──────────────────────────────────────────

    /// `max_bundles` is taken literally — `0` means the staffing gets ZERO
    /// bundles (a degenerate, silent no-op selection), not "unlimited".
    #[test]
    fn selector_max_bundles_zero_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { fact_families: vec![], max_bundles: Some(0), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(selected.is_empty(), "max_bundles: 0 must select nothing, not \"unlimited\"");
    }

    /// A `fact_families` restriction naming a family no bundle carries
    /// degrades to an empty selection (zero bundles for that staffing),
    /// never falls back to "no restriction matches everything."
    #[test]
    fn selector_fact_families_naming_unknown_family_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector {
            fact_families: vec!["nonexistent-family".to_string()],
            max_bundles: None,
            ..Default::default()
        };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(
            selected.is_empty(),
            "an unmatched fact_families restriction must select zero bundles, not fall back to 'no restriction'"
        );
    }

    // ── step telemetry consistency ───────────────────────────────────

    /// The `probe` step's wall_ms wraps the ENTIRE `probe_phase` call
    /// (cycler load/release overhead + every member's dispatch time), so it
    /// must be >= the sum of the probe seats' own `MemberRecord.wall_ms`
    /// (which excludes cycler overhead). A small real sleep in the mocked
    /// `chat` makes the timing comparison meaningful instead of two zeros.
    #[test]
    fn step_telemetry_probe_wall_ms_encompasses_member_wall_ms() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            std::thread::sleep(std::time::Duration::from_millis(2));
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        let probe_step = env.steps.iter().find(|s| s.step_id == "probe").expect("probe step recorded");
        let probe_member_ms: u64 = env
            .members
            .iter()
            .filter(|m| m.seat == "review-probe")
            .map(|m| m.wall_ms)
            .sum();
        assert!(
            probe_step.wall_ms >= probe_member_ms,
            "probe step ({}) must wrap at least as much wall time as its members' dispatch time ({})",
            probe_step.wall_ms,
            probe_member_ms
        );
    }

    /// The judge's `MemberRecord.wall_ms` is set to EXACTLY `pass1_ms +
    /// pass2_ms` (`finish_funnel`), and the `judge-pass1`/`judge-pass2`
    /// step rows carry those same two values — so their sum must equal the
    /// judge member's wall_ms EXACTLY, not just approximately (both are
    /// derived from the same accumulator variables, so this holds
    /// regardless of real elapsed time).
    #[test]
    fn step_telemetry_judge_steps_sum_equals_judge_member_wall_ms() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent: "add a feature",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Both judge passes confirm, so both judge-pass1 and
                // judge-pass2 step rows get recorded.
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler).expect("funnel runs");
        let judge_member = env
            .members
            .iter()
            .find(|m| m.seat == "review-judge")
            .expect("judge member recorded");
        let step_sum: u64 = env
            .steps
            .iter()
            .filter(|s| s.step_id.starts_with("judge-"))
            .map(|s| s.wall_ms)
            .sum();
        assert_eq!(
            step_sum, judge_member.wall_ms,
            "judge-pass1 + judge-pass2 step wall_ms must sum EXACTLY to the judge MemberRecord's wall_ms"
        );
    }

    // ── envelope serde round trip through a file ─────────────────────

    /// `FunnelEnvelope` derives `Serialize` only (no `Deserialize`), so a
    /// literal `FunnelEnvelope -> FunnelEnvelope` round trip isn't
    /// expressible. This writes a fully-populated envelope (covering all
    /// three `Tier` variants) to a real file, reads it back, and checks
    /// value-level equality through `serde_json::Value` — the strongest
    /// round-trip check available against the current shape.
    #[test]
    fn envelope_serde_round_trips_through_a_file_with_all_tier_variants() {
        use std::io::Write;

        let flag_confirmed = flag("b1", "member-a", 0, "confirmed charge");
        let flag_needs_check = flag("b1", "member-a", 1, "needs-check charge");
        let flag_archived = flag("b1", "member-a", 2, "archived charge");

        let judged = vec![
            JudgedFlag {
                flag: flag_confirmed.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e1".into(),
                    note_for_author: "n1".into(),
                    pass: 1,
                    seconds: 0.5,
                },
                pass2: Some(JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e1b".into(),
                    note_for_author: "n1b".into(),
                    pass: 2,
                    seconds: 0.4,
                }),
                tier: Tier::Confirmed,
                demoted_by_pass2: false,
            },
            JudgedFlag {
                flag: flag_needs_check.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e2".into(),
                    note_for_author: "n2".into(),
                    pass: 1,
                    seconds: 0.3,
                },
                pass2: Some(JudgeRecord {
                    ruling: FunnelRuling::FalsePositive,
                    decisive_evidence: "e2b".into(),
                    note_for_author: "n2b".into(),
                    pass: 2,
                    seconds: 0.2,
                }),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
            },
            JudgedFlag {
                flag: flag_archived.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::FalsePositive,
                    decisive_evidence: "e3".into(),
                    note_for_author: "n3".into(),
                    pass: 1,
                    seconds: 0.1,
                },
                pass2: None,
                tier: Tier::Archived,
                demoted_by_pass2: false,
            },
        ];

        let env = FunnelEnvelope {
            case_id: "case-42".to_string(),
            crew: "test-crew".to_string(),
            mode: "sequential".to_string(),
            members: vec![
                MemberRecord {
                    model: "darkmux:probe-model".to_string(),
                    seat: "review-probe".to_string(),
                    draws: 3,
                    wall_ms: 1200,
                    total_tokens: 900,
                },
                MemberRecord {
                    model: "darkmux:judge-model".to_string(),
                    seat: "review-judge".to_string(),
                    draws: 5,
                    wall_ms: 800,
                    total_tokens: 600,
                },
            ],
            steps: vec![
                StepRecord { step_id: "bundle".to_string(), kind: "procedural".to_string(), items_in: 1, items_out: 1, wall_ms: 2 },
                StepRecord { step_id: "probe".to_string(), kind: "dispatch".to_string(), items_in: 1, items_out: 3, wall_ms: 1200 },
                StepRecord { step_id: "dedup".to_string(), kind: "procedural".to_string(), items_in: 3, items_out: 3, wall_ms: 1 },
                StepRecord { step_id: "judge-pass1".to_string(), kind: "dispatch".to_string(), items_in: 3, items_out: 3, wall_ms: 500 },
                StepRecord { step_id: "judge-pass2".to_string(), kind: "dispatch".to_string(), items_in: 2, items_out: 2, wall_ms: 300 },
            ],
            bundles: 1,
            raw_flags: 3,
            deduped_flags: 3,
            flags: vec![flag_confirmed, flag_needs_check, flag_archived],
            judged,
            confirmed: 1,
            needs_check: 1,
            archived: 1,
            degenerate: None,
            fingerprint: fingerprint("darkmux:judge-model", "judge sys"),
        };

        let json = serde_json::to_string_pretty(&env).expect("serialize");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("envelope.json");
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(json.as_bytes()).expect("write");
        }
        let read_back = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&read_back).expect("valid json");

        assert_eq!(value["case_id"], "case-42");
        assert_eq!(value["crew"], "test-crew");
        assert_eq!(value["mode"], "sequential");
        assert_eq!(value["bundles"], 1);
        assert_eq!(value["raw_flags"], 3);
        assert_eq!(value["deduped_flags"], 3);
        assert_eq!(value["confirmed"], 1);
        assert_eq!(value["needs_check"], 1);
        assert_eq!(value["archived"], 1);
        assert!(value.get("degenerate").is_none(), "a None degenerate must be omitted, not written as null");
        assert_eq!(value["fingerprint"]["protocol"], "double-confirm-v1");

        let tiers: Vec<String> = value["judged"]
            .as_array()
            .expect("judged array")
            .iter()
            .map(|j| j["tier"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            tiers,
            vec!["confirmed", "needs_check", "archived"],
            "all three Tier variants must survive the file round trip verbatim"
        );

        assert_eq!(value["members"].as_array().unwrap().len(), 2);
        assert_eq!(value["steps"].as_array().unwrap().len(), 5);
        assert_eq!(value["judged"][1]["demoted_by_pass2"], true);
        assert!(value["judged"][2]["pass2"].is_null(), "no pass-2 dispatch serializes pass2 as null, not omitted");
    }

    // ── judge_prompt: independent section gating ──────────────────────

    /// Facts and manifest sections are gated INDEPENDENTLY — a bundle with
    /// a manifest but no fact sheet must show the manifest section and
    /// omit the fact-sheet section (the existing "all present" / "all
    /// absent" tests don't isolate this mixed case).
    #[test]
    fn judge_prompt_manifest_present_facts_absent() {
        let p = judge_prompt("intent", "code", &[], &["helperFn".to_string()], "charge");
        assert!(!p.contains("Fact sheet:"), "no facts supplied -> no fact-sheet section");
        assert!(p.contains("Symbols referenced but not defined in the provided code:"));
        assert!(p.contains("helperFn"));
    }

    /// The mirror case: facts present, manifest absent.
    #[test]
    fn judge_prompt_facts_present_manifest_absent() {
        let p = judge_prompt("intent", "code", &["fact one".to_string()], &[], "charge");
        assert!(p.contains("Fact sheet:"));
        assert!(p.contains("fact one"));
        assert!(!p.contains("Symbols referenced but not defined"), "no manifest supplied -> no manifest section");
    }
}
