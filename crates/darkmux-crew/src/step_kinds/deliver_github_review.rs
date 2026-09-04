//! `deliver.github_review` (#2310 P4b) — findings + mods + a diff → a
//! GitHub review payload (`{event, body, comments:[{path,line,body}]}`,
//! DESIGN.md "Code review as a second config on the crawl's building
//! blocks" — "Delivery. Review's own kind: mods and findings in, the
//! GitHub review payload out. Pure render, no model, so the harness covers
//! it.").
//!
//! **Mission-agnostic by construction.** A crawl over a repo can deliver
//! its findings the same way a review does — this module never imports a
//! review-pipeline type (no `ProbeFlag`, no `JudgedFlag`, no
//! `ReviewEnvelope`) and never depends on `darkmux-lab`'s `review_render`
//! (this crate has no `darkmux-lab` dependency and never will — see
//! `step_kinds::patterns`'s own module doc for why). Its only inputs are
//! the shared finding/mod record types ([`crate::findings::FindingRecord`],
//! [`crate::mods::ModRecord`]) plus a diff and a scope summary — nothing
//! that only a review pipeline would have.
//!
//! **Tier 1 (#1352), physically its own file.** The mapping from a finding
//! (plus its mods) to a delivery form is a FIXED procedure — DESIGN.md
//! "Confirmation is a mod, a search, or a question" names exactly three
//! forms and the rule that picks between them; no caller supplies a
//! DIFFERENT algorithm, so there is no pluggable strategy for a Tier 2
//! pattern (`step_kinds::patterns`) to abstract over, and it belongs with
//! Tier 1's generic, config-driven kinds. Not folded into `builtins.rs`
//! itself (already ~4200 lines — this project's own monolith-avoidance
//! discipline) or into `StepKindRegistry::with_builtins()`'s always-on
//! five: [`register_deliver_kind`] is a dedicated registration function,
//! the same shape `darkmux-lab`'s `register_review_kinds`/
//! `register_crawl_kinds` use for kinds a caller opts into explicitly —
//! the #2310 P4b brief is explicit that this kind is not wired into any
//! mission config yet, and `with_builtins()`'s own test pins an EXACT
//! five-kind set that a sixth, always-on kind would force widening for a
//! caller that doesn't exist yet.
//!
//! **The render is a pure function** ([`render_github_review`]) over typed
//! inputs, independent of [`DeliverGithubReviewStepKind`]'s own
//! `Step`/`Task` plumbing — this is what the golden tests exercise
//! directly, and what a caller embeds without going through the scheduler
//! at all.

use crate::findings::FindingRecord;
use crate::mods::ModRecord;
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{Port, StepKind, StepOutcome};
use crate::types::{Step, Task};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

pub const DELIVER_GITHUB_REVIEW_KIND: &str = "deliver.github_review";

/// One mod plus whether it passed its gate — the fact a bare [`ModRecord`]
/// does not carry. A mod record is the proposed change (#2265's own
/// doctrine: "a mod is a KIT... darkmux never types a kit and never opens
/// it"); whether it passed review's gate is a judgment a downstream verify
/// step makes, the same separation the coder-phase pipeline draws between
/// a coder's diff and its own verify step's outcome. `None` — never run —
/// is a distinct fact from `Some(false)` — ran and failed — but this
/// module treats both the same way at delivery: DESIGN.md's own rule is
/// "a gate-failed mod", and a mod nothing ever gated has not passed one
/// either.
///
/// No `PartialEq` derive: `ModRecord` itself doesn't implement it (a mod's
/// `kit` is opaque, never-compared data by design), so a `GatedMod`
/// doesn't either — nothing in this module needs to compare two mods for
/// equality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatedMod {
    #[serde(flatten)]
    pub record: ModRecord,
    #[serde(default)]
    pub gate_passed: Option<bool>,
}

/// The run's scope summary — DESIGN.md "The honest limit": "The review's
/// summary must state its scope (rules run, windows covered, what it did
/// not attempt) so a narrow review never reads as complete." Handed in by
/// the caller (a future review/crawl plan+unit pipeline's own bookkeeping,
/// #2310 P4c) rather than derived here — this module has no visibility
/// into what ran upstream beyond what it's told.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeliverScope {
    #[serde(default)]
    pub rules_run: Vec<String>,
    #[serde(default)]
    pub hunks_covered: usize,
    #[serde(default)]
    pub hunks_total: usize,
    /// Findings the run refused or rejected — counted here, never
    /// enumerated (DESIGN.md "Refused and rejected findings are counted in
    /// the summary line and never posted") — `findings` below holds only
    /// ACCEPTED, materialized records (a `FindingRecord` cannot represent
    /// a refusal at all — see that type's own doc), so this scalar is the
    /// caller's own count, not derived from `findings.len()`.
    #[serde(default)]
    pub refused: usize,
    /// What this run did not attempt — named explicitly so a reader
    /// cannot mistake a narrow run for a complete one.
    #[serde(default)]
    pub not_attempted: Vec<String>,
    /// (#2310 P4c-2b PR #2357 review MUST FIX D) Human-readable names of
    /// units/plans that ended `Error`/`Abandoned` this run — distinct from
    /// [`Self::not_attempted`] (which is what NEVER RAN, e.g. a rule whose
    /// plan step failed) and from [`Self::refused`] (runtime-boundary
    /// rejections, never a step-level failure). A non-empty list here is
    /// what tells [`render_github_review`] this run is `"degraded"`, never
    /// a clean `"noop"`, even when it produced zero findings.
    #[serde(default)]
    pub errored: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubReviewComment {
    pub path: String,
    /// The comment's END line (GitHub's own convention — `line` is always
    /// the range's end, whether or not `start_line` is present).
    pub line: u32,
    /// (#2310 P4b review, M-B) Present only for a MULTI-line suggestion —
    /// a single-line one carries `line` alone, matching GitHub's own API
    /// (a `start_line` equal to `line` is rejected as redundant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// `"RIGHT"` for every comment this kind emits (anchored to the PR's
    /// new/current side) — matching `review_render.rs`'s existing anchors
    /// (`{"side": "RIGHT", ...}`) even though this module has no
    /// dependency on that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    pub body: String,
}

/// `{event, body, comments}` — the shape the review workflow already posts
/// (DESIGN.md names it verbatim). `event` is always `"COMMENT"` here: this
/// kind has no notion of a blocking `REQUEST_CHANGES` review — that policy
/// question belongs to whatever builds the mission config that wires this
/// kind in (#2310 P4c), not to a mission-agnostic render.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubReviewPayload {
    pub event: String,
    pub body: String,
    pub comments: Vec<GithubReviewComment>,
}

/// What [`render_github_review`] returns. `mode` is `"review"` when there
/// is anything at all to say (findings, or mods); `"noop"` ONLY for a
/// genuinely CLEAN run — nothing found AND nothing went wrong
/// (`scope.errored` empty); `"degraded"` (#2310 P4c-2b PR #2357 review
/// MUST FIX D) when nothing was found to say but `scope.errored` is
/// non-empty — an errored/abandoned unit or plan step means this run is
/// NOT a clean pass, even with zero findings, and must never read as one.
/// Mirrors the review pipeline's own `mode` vocabulary in spirit (a
/// distinct outcome, never silently folded into `"review"`) without
/// depending on its type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliverOutcome {
    pub mode: String,
    pub review: Option<GithubReviewPayload>,
}

/// Render findings + mods + a diff + a scope summary into a
/// [`DeliverOutcome`]. Pure — no I/O, no model dispatch — so this is what
/// this packet's golden tests call directly.
///
/// Delivery-form rule (DESIGN.md "Confirmation is a mod, a search, or a
/// question", read off each finding's `context.form` — `"search"` |
/// `"question"` | absent/anything else means `"mod"`, the default):
///
/// - **mod-form, a mod names this finding's key, `gate_passed == Some(true)`,
///   and the mod's window sits inside the diff's touched lines** — an
///   INLINE `comments[]` entry with a ` ```suggestion ` block.
/// - **mod-form, same gate, but outside the diff's lines** — a general
///   `body` bullet carrying the mod's kit as a fenced patch, not a
///   suggestion block (GitHub suggestions only resolve inside a diff).
/// - **mod-form, no mod names this finding, or the naming mod's gate is
///   `Some(false)`/`None`** — a "worth a double check" `body` bullet:
///   finding key, window, claim (`why`), what to check (`evidence`).
/// - **search-form** — a `body` bullet listing the instances, which
///   DESIGN.md says live in the finding's own `evidence` field.
/// - **question-form** — a `body` bullet phrased as a question, with
///   candidates (also `evidence`).
///
/// Refused/rejected findings never reach this function at all (see
/// [`DeliverScope::refused`]'s doc) — only their count feeds the summary
/// line.
pub fn render_github_review(
    findings: &[FindingRecord],
    mods: &[GatedMod],
    diff: &str,
    scope: &DeliverScope,
    attribution: Option<&str>,
) -> DeliverOutcome {
    let touched = diff_touched_lines(diff);

    let mut comments: Vec<GithubReviewComment> = Vec::new();
    let mut mod_bullets: Vec<String> = Vec::new();
    let mut search_bullets: Vec<String> = Vec::new();
    let mut question_bullets: Vec<String> = Vec::new();
    let mut double_check_bullets: Vec<String> = Vec::new();

    for finding in findings {
        let window = FindingWindow::from(finding);
        match delivery_form(finding) {
            DeliveryForm::Search => {
                search_bullets.push(format!(
                    "- `{}` — {}",
                    window.display(),
                    window.evidence.as_deref().unwrap_or("(no instances recorded)")
                ));
            }
            DeliveryForm::Question => {
                question_bullets.push(format!(
                    "- `{}` — {}? Candidates: {}",
                    window.display(),
                    window.why.as_deref().unwrap_or("did you check whether this already exists"),
                    window.evidence.as_deref().unwrap_or("(none recorded)")
                ));
            }
            DeliveryForm::Mod => {
                // (#2310 P4b review, CONSIDER) Prefer a GATE-PASSED mod
                // over an earlier gate-failed one naming the same
                // finding: without this, a coder's second (successful)
                // attempt at a finding could lose to its own first
                // failed one just because it landed later in `mods`.
                // Falls back to the first match at all (any gate state)
                // so the double-check branch below still has something
                // to describe when nothing passed.
                let matches: Vec<&GatedMod> =
                    mods.iter().filter(|m| m.record.r#for.iter().any(|k| k == &finding.key)).collect();
                let gated =
                    matches.iter().find(|m| m.gate_passed == Some(true)).copied().or_else(|| matches.first().copied());
                match gated {
                    Some(m) if m.gate_passed == Some(true) => {
                        render_gated_mod(m, &window, &touched, &mut comments, &mut mod_bullets);
                    }
                    _ => {
                        double_check_bullets.push(format!(
                            "- `{}` — {} _Worth checking: {}_",
                            window.display(),
                            window.why.as_deref().unwrap_or("(no claim recorded)"),
                            window.evidence.as_deref().unwrap_or("the cited line")
                        ));
                    }
                }
            }
        }
    }

    // Captured before `body` takes ownership of each bullet list below —
    // whether there was anything to say at all decides `mode`.
    let nothing_to_say = comments.is_empty()
        && mod_bullets.is_empty()
        && search_bullets.is_empty()
        && question_bullets.is_empty()
        && double_check_bullets.is_empty();
    if nothing_to_say {
        if scope.errored.is_empty() {
            // A genuinely CLEAN run — nothing to say because nothing went
            // wrong and nothing was found. The only `mode` this applies
            // to.
            return DeliverOutcome { mode: "noop".to_string(), review: None };
        }
        // (#2310 P4c-2b PR #2357 review MUST FIX D, proven live) Before
        // this fix, an errored run with zero findings ALSO rendered
        // `"noop"` — the same "an errored run renders nothing" defect
        // this arc exists to end, just one layer up from #975's own
        // finding. A run that had a real error is never a clean noop,
        // even with nothing to say about findings: the scope line (with
        // its own `Errored:` section, `scope_line`'s own doc) IS the
        // payload.
        let mut body = vec!["### darkmux review".to_string(), String::new(), scope_line(scope, findings.len())];
        if let Some(a) = attribution.filter(|a| !a.trim().is_empty()) {
            body.push(String::new());
            body.push(format!("_{a}_"));
        }
        return DeliverOutcome {
            mode: "degraded".to_string(),
            review: Some(GithubReviewPayload { event: "COMMENT".to_string(), body: body.join("\n"), comments: Vec::new() }),
        };
    }

    let mut body = vec!["### darkmux review".to_string(), String::new()];
    body.push(scope_line(scope, findings.len()));
    if !mod_bullets.is_empty() {
        body.push(String::new());
        body.push("**Proposed changes outside the diff:**".to_string());
        body.extend(mod_bullets);
    }
    if !search_bullets.is_empty() {
        body.push(String::new());
        body.push("**Worth enumerating (search-confirmed):**".to_string());
        body.extend(search_bullets);
    }
    if !question_bullets.is_empty() {
        body.push(String::new());
        body.push("**Questions:**".to_string());
        body.extend(question_bullets);
    }
    if !double_check_bullets.is_empty() {
        body.push(String::new());
        body.push("**Worth a double check** (not merge-blocking):".to_string());
        body.extend(double_check_bullets);
    }
    if let Some(a) = attribution.filter(|a| !a.trim().is_empty()) {
        body.push(String::new());
        body.push(format!("_{a}_"));
    }

    DeliverOutcome {
        mode: "review".to_string(),
        review: Some(GithubReviewPayload { event: "COMMENT".to_string(), body: body.join("\n"), comments }),
    }
}

/// DESIGN.md "rules run, hunks covered / total, findings by delivery form,
/// refused count, and what the review did not attempt. Never reads as
/// complete."
fn scope_line(scope: &DeliverScope, findings_considered: usize) -> String {
    let mut line = format!(
        "review ran: {} rule(s), {}/{} hunks covered, {} finding(s) considered, {} refused.",
        scope.rules_run.len(),
        scope.hunks_covered,
        scope.hunks_total,
        findings_considered,
        scope.refused,
    );
    if !scope.not_attempted.is_empty() {
        line.push_str(&format!(" Not attempted: {}.", scope.not_attempted.join(", ")));
    }
    // (#2310 P4c-2b PR #2357 review MUST FIX D) Named so a `"degraded"`
    // run's scope line (its whole payload, when there is nothing else to
    // say) actually names what broke, not just that something did.
    if !scope.errored.is_empty() {
        line.push_str(&format!(" Errored: {}.", scope.errored.join(", ")));
    }
    line
}

/// (#2310 P4b review, M-B) A gate-passed mod's kit becomes either inline
/// GitHub suggestion(s) or a fenced-patch body bullet — NEVER a
/// suggestion for an opaque kit. DESIGN.md: "darkmux never opens a kit".
/// Pasting an opaque kit verbatim into a ```suggestion block was the bug
/// this function fixes: the common kit shape is itself a unified diff, so
/// "Commit suggestion" would have replaced the anchored line with raw
/// `+++`/`@@` text, and a multi-line kit collapsed to a single-line
/// suggestion (no `start_line`) that duplicated the lines below it.
///
/// Only a mod whose proposer explicitly declared `kit_kind:
/// "unified-diff"` gets parsed — through the SAME shared
/// `crate::diff::parse_diff` this crate and `darkmux-lab`'s bundler both
/// use, never a second parser (see that module's own doc) — and only a
/// HUNK whose OLD range sits ENTIRELY inside the PR diff's own touched
/// lines becomes a suggestion (a kit's OLD side names the lines it
/// replaces in the file's CURRENT state, which is the same coordinate
/// space the PR diff's NEW side already occupies). Every other case —
/// `kit_kind` unset or not `"unified-diff"`, an unparseable kit, a
/// pure-insertion hunk with no OLD range to anchor a replacement against,
/// or a hunk outside the PR diff — falls back to the opaque fenced-patch
/// bullet this branch has always rendered.
fn render_gated_mod(
    m: &GatedMod,
    window: &FindingWindow,
    touched: &BTreeMap<String, BTreeSet<u32>>,
    comments: &mut Vec<GithubReviewComment>,
    mod_bullets: &mut Vec<String>,
) {
    let kit = m.record.kit.as_deref().unwrap_or("");
    if m.record.kit_kind.as_deref() != Some("unified-diff") {
        mod_bullets.push(fenced_patch_bullet(window, kit, "the kit is not a typed unified diff, so it cannot be a suggestion"));
        return;
    }
    let hunks = crate::diff::parse_diff(kit);
    if hunks.is_empty() {
        // Declared unified-diff but nothing parsed — an unparseable kit.
        // Never guess at intent; render it opaque, same as any other kind.
        mod_bullets.push(fenced_patch_bullet(window, kit, "the kit did not parse as a unified diff"));
        return;
    }
    for (path, file_hunks) in &hunks {
        for h in file_hunks {
            if h.old_block.is_empty() {
                // A pure-insertion hunk has no OLD range to anchor a
                // REPLACEMENT suggestion against — GitHub suggestions
                // replace an existing line range; they cannot insert
                // between two lines with no line of their own.
                mod_bullets.push(fenced_hunk_bullet(window, path, h, "a pure insertion has no lines to replace"));
                continue;
            }
            let old_start = h.old_start;
            let old_end = old_start + h.old_block.len() as u32 - 1;
            let inside = (old_start..=old_end).all(|l| line_touched(touched, path, Some(l)));
            if inside {
                comments.push(GithubReviewComment {
                    path: path.clone(),
                    line: old_end,
                    start_line: if old_start != old_end { Some(old_start) } else { None },
                    side: Some("RIGHT".to_string()),
                    body: format!("```suggestion\n{}\n```", h.new_block.join("\n")),
                });
            } else {
                mod_bullets.push(fenced_hunk_bullet(window, path, h, "the hunk sits outside the diff's lines"));
            }
        }
    }
}

fn fenced_patch_bullet(window: &FindingWindow, kit: &str, reason: &str) -> String {
    format!("- `{}` — proposed change ({reason}):\n\n```\n{kit}\n```", window.display())
}

fn fenced_hunk_bullet(window: &FindingWindow, path: &str, h: &crate::diff::Hunk, reason: &str) -> String {
    let old_len = h.old_block.len() as u32;
    let span = if old_len == 0 {
        format!("inserts after line {} of `{path}`", h.old_start)
    } else {
        format!("replaces lines {}–{} of `{path}`", h.old_start, h.old_start + old_len - 1)
    };
    format!(
        "- `{}` — proposed change, {span} ({reason}):\n\n```\n{}\n```",
        window.display(),
        h.new_block.join("\n")
    )
}

enum DeliveryForm {
    Mod,
    Search,
    Question,
}

/// (#2310 P4c-2b fix) Reads `context.confirm` — the SAME key
/// `crawl::unit_step::run` host-stamps on every real finding's
/// `record_context` (`"confirm": single_confirm(&rules_by_id, &ctx.
/// rule_ids)`, itself copied straight from a `Rule`'s own `confirm` field:
/// `templates/builtin/rules/*.json`'s `"confirm": "mod"|"search"|
/// "question"`). This function used to read `context.get("form")` — a key
/// NOTHING in the codebase ever stamped (P4b wrote this module before P4c
/// decided the crawl unit's own stamped key name), so every real finding
/// silently rendered as `DeliveryForm::Mod` regardless of its rule's
/// declared confirm form until #2310 P4c-2b's own end-to-end wiring
/// surfaced the mismatch. `deliver_github_review`'s OWN tests below
/// updated to the real key in the same fix.
fn delivery_form(finding: &FindingRecord) -> DeliveryForm {
    match finding.context.get("confirm").and_then(|v| v.as_str()) {
        Some("search") => DeliveryForm::Search,
        Some("question") => DeliveryForm::Question,
        _ => DeliveryForm::Mod,
    }
}

/// A finding's window + claim, projected out of its opaque `emitted`
/// (`create_finding`'s fixed tool-arg shape: `{file, line, pattern,
/// evidence, why}` — `runtime/src/tools/mod.rs`'s own `Tool::CreateFinding`
/// description) — reading NAMED fields out of an emission is a consumer's
/// job (this module's), never the finding store's own (`findings.rs`'s
/// doctrine: "darkmux does not interpret the emission" is about the
/// STORE, not every reader downstream of it).
struct FindingWindow {
    key: String,
    file: Option<String>,
    line: Option<u32>,
    evidence: Option<String>,
    why: Option<String>,
}

impl FindingWindow {
    fn from(finding: &FindingRecord) -> Self {
        let get = |k: &str| finding.emitted.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Self {
            key: finding.key.clone(),
            file: get("file"),
            line: finding.emitted.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
            evidence: get("evidence"),
            why: get("why"),
        }
    }

    fn display(&self) -> String {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{} ({}:{})", self.key, f, l),
            (Some(f), None) => format!("{} ({f})", self.key),
            _ => self.key.clone(),
        }
    }
}

/// Whether `(file, line)` falls within any hunk `diff_touched_lines`
/// recorded for `file` — the "sits inside the diff's lines" test DESIGN.md
/// names for a suggestion block.
fn line_touched(touched: &BTreeMap<String, BTreeSet<u32>>, file: &str, line: Option<u32>) -> bool {
    let Some(line) = line else { return false };
    touched.get(file).is_some_and(|lines| lines.contains(&line))
}

/// path -> new-side line numbers (context AND added — `Hunk::new_lines`'s
/// own doc: a changed function is locatable via a context line just as
/// well as an added one) a unified diff's hunks touch.
///
/// (#2310 P4b) Built on `crate::diff::parse_diff` — the ONE unified-diff
/// parser this crate and `darkmux-lab`'s bundler now share (moved here
/// from `darkmux-lab`'s `bundle::diff`, which re-exports it verbatim; see
/// `crate::diff`'s own module doc). An earlier version of this function
/// hand-rolled a second parser under the mistaken belief that the crate
/// boundary forced a duplicate — it forced a MOVE instead, which is what
/// this module doc now records so the mistake isn't repeated.
fn diff_touched_lines(diff_text: &str) -> BTreeMap<String, BTreeSet<u32>> {
    crate::diff::parse_diff(diff_text)
        .into_iter()
        .map(|(path, hunks)| {
            let mut lines: BTreeSet<u32> = BTreeSet::new();
            for h in &hunks {
                lines.extend(h.new_lines.iter().copied());
            }
            (path, lines)
        })
        .collect()
}

// ─── The StepKind wrapper ───────────────────────────────────────────────

/// `Step.config` shape: `{"findings": [FindingRecord...], "mods":
/// [GatedMod...], "diff": "<unified diff text>", "scope": DeliverScope,
/// "attribution": "<optional string>", "emit": "<path, or \"-\"/absent
/// for stdout>"}`. Every field but `findings`/`mods`/`diff` is optional —
/// EXCEPT that `findings`/`mods`/`diff`/`scope` may ALSO be supplied as a
/// group by a same-task predecessor step's output instead of embedded
/// literally (see [`DeliverConfig::from_step`]'s second branch below).
///
/// Reads its inputs from `Step.config` directly rather than through typed
/// graph ports (#2301's `Output<T>` envelope convention) for the ORIGINAL
/// three fields — no producer for "findings as one bulk typed value"
/// existed when this module was written (a finding is read from the
/// finding STORE by key, per `crawl.summary`'s own "the same one read of
/// the dispatch's findings.jsonl" pattern, DESIGN.md's record table) —
/// inventing four speculative port kinds nothing would ever produce was
/// worse than the config-embedded shape. #2310 P4c-2b is the future caller
/// that module doc predicted: `records.gather` (`step_kinds::
/// records_gather`) IS that store read, and this struct now accepts its
/// output too, over the SAME `Data`-port `Step.output` -> `gather_inputs`
/// wiring `Port`'s own doc describes — never a NEW mechanism.
struct DeliverConfig {
    findings: Vec<FindingRecord>,
    mods: Vec<GatedMod>,
    diff: String,
    scope: DeliverScope,
    attribution: Option<String>,
    emit: Option<PathBuf>,
}

impl DeliverConfig {
    /// `input` is the step's own `gather_inputs` map (unused by every
    /// existing caller — every current test embeds `findings`/`mods`/
    /// `diff` literally in `step.config`, which this function still
    /// prefers outright when present, so NOTHING about their behavior
    /// changes). When `config.findings` is absent, this looks instead for
    /// a [`super::records_gather::GatherOutput`] envelope among `input`'s
    /// values — the shape a `records.gather` step run as this step's
    /// immediately-previous SAME-TASK step produces (`scheduler::
    /// gather_inputs`'s documented same-task-predecessor entry, keyed by
    /// that step's own id) — and pulls `findings`/`mods`/`diff`/`scope`
    /// from it as a group. `attribution`/`emit` are launch-time strings,
    /// never data, and always come from `step.config` either way.
    fn from_step(step: &Step, input: &BTreeMap<String, String>) -> Result<Self> {
        let attribution = step.config.get("attribution").and_then(|v| v.as_str()).map(str::to_string);
        let emit = step.config.get("emit").and_then(|v| v.as_str()).map(PathBuf::from);

        if step.config.get("findings").is_some() {
            let field = |key: &str| -> Result<serde_json::Value> {
                step.config.get(key).cloned().ok_or_else(|| {
                    anyhow!("step `{}`: `{DELIVER_GITHUB_REVIEW_KIND}` requires config.{key}", step.id)
                })
            };
            let findings: Vec<FindingRecord> = serde_json::from_value(field("findings")?)
                .with_context(|| format!("step `{}`: config.findings", step.id))?;
            let mods: Vec<GatedMod> = serde_json::from_value(field("mods")?)
                .with_context(|| format!("step `{}`: config.mods", step.id))?;
            let diff = field("diff")?
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("step `{}`: config.diff must be a string", step.id))?;
            let scope: DeliverScope = match step.config.get("scope") {
                Some(v) => serde_json::from_value(v.clone())
                    .with_context(|| format!("step `{}`: config.scope", step.id))?,
                None => DeliverScope::default(),
            };
            return Ok(Self { findings, mods, diff, scope, attribution, emit });
        }

        let gathered = input.values().find_map(|raw| {
            crate::step_output::Output::<super::records_gather::GatherOutput>::read(
                raw,
                super::records_gather::RECORDS_GATHER_OUTPUT_KIND,
            )
            .ok()
        });
        let Some(gathered) = gathered else {
            bail!(
                "step `{}`: `{DELIVER_GITHUB_REVIEW_KIND}` requires config.findings (embedded) or a \
                 `{}` step's output among its inputs (present: {}) — neither was found",
                step.id,
                super::records_gather::RECORDS_GATHER_KIND,
                if input.is_empty() { "none".to_string() } else { input.keys().cloned().collect::<Vec<_>>().join(", ") }
            );
        };
        let body = gathered.body;
        Ok(Self { findings: body.findings, mods: body.mods, diff: body.diff, scope: body.scope, attribution, emit })
    }
}

pub struct DeliverGithubReviewStepKind;

impl StepKind for DeliverGithubReviewStepKind {
    fn id(&self) -> &'static str {
        DELIVER_GITHUB_REVIEW_KIND
    }

    fn display_name(&self) -> &'static str {
        "Deliver"
    }

    fn provides(&self) -> &'static [Port] {
        &[]
    }

    /// (#1979) `None` — this kind performs no model work and owns no
    /// dispatch session. Same documented no-dispatch opt-out
    /// `procedural.shell`/`procedural.noop` use.
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    fn run(&self, step: &Step, _task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = DeliverConfig::from_step(step, input)?;
        let outcome =
            render_github_review(&cfg.findings, &cfg.mods, &cfg.diff, &cfg.scope, cfg.attribution.as_deref());
        let payload = serde_json::to_string(&outcome).context("serializing the deliver outcome")?;
        match cfg.emit.as_deref() {
            Some(p) if p == std::path::Path::new("-") => println!("{payload}"),
            Some(p) => std::fs::write(p, &payload).with_context(|| format!("writing {}", p.display()))?,
            None => println!("{payload}"),
        }
        let dest = cfg.emit.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".to_string());
        Ok(StepOutcome { output: dest, flow_records: Vec::new() })
    }
}

/// Register `deliver.github_review` onto `registry` — the same shape
/// `darkmux-lab`'s `review::register_review_kinds`/`crawl::plan_step::
/// register_crawl_kinds` use: a dedicated function a caller opts into,
/// never folded into `StepKindRegistry::with_builtins()`'s always-on set
/// (see this module's own doc for why). No caller registers this yet
/// (#2310 P4c wires a mission config that does); this function exists so
/// one can, and is exercised by this module's own tests.
pub fn register_deliver_kind(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(DeliverGithubReviewStepKind)).context("registering deliver.github_review")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn finding(key: &str, file: &str, line: u32, evidence: &str, why: &str, form: Option<&str>) -> FindingRecord {
        let mut context = json!({});
        if let Some(f) = form {
            // (#2310 P4c-2b fix) `confirm`, not `form` — see `delivery_form`'s own doc.
            context = json!({ "confirm": f });
        }
        FindingRecord {
            key: key.to_string(),
            dispatch: key.split('/').next().unwrap().to_string(),
            seq: key.split('/').nth(1).unwrap().parse().unwrap(),
            ts: "2026-09-04T00:00:00Z".to_string(),
            tool_name: "create_finding".to_string(),
            proposer: crate::findings::Proposer { handle: "reviewer".to_string(), model: "test".to_string(), machine_id: None },
            mission_id: None,
            phase_id: None,
            step_id: None,
            context,
            emitted: json!({ "file": file, "line": line, "pattern": "test", "evidence": evidence, "why": why }),
            schema_version: crate::findings::FINDING_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    fn gated_mod(for_key: &str, kit: &str, gate_passed: Option<bool>) -> GatedMod {
        gated_mod_kind(for_key, kit, None, gate_passed)
    }

    /// (#2310 P4b review, M-B) Same as [`gated_mod`], with an explicit
    /// `kit_kind` — used by the unified-diff suggestion tests.
    fn gated_mod_kind(for_key: &str, kit: &str, kit_kind: Option<&str>, gate_passed: Option<bool>) -> GatedMod {
        GatedMod {
            record: ModRecord {
                key: "mod-1-abcdef".to_string(),
                ts: "2026-09-04T00:00:01Z".to_string(),
                by: "coder".to_string(),
                r#for: vec![for_key.to_string()],
                kit: Some(kit.to_string()),
                kit_looks_json: false,
                kit_kind: kit_kind.map(str::to_string),
                attachments: Vec::new(),
                context: Default::default(),
                warnings: Vec::new(),
                mission_id: None,
                phase_id: None,
                step_id: None,
                gate: None,
                gate_skipped_reason: None,
                schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            },
            gate_passed,
        }
    }

    const DIFF: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,2 +1,3 @@\n function f() {\n+  const x = 1;\n }\n";

    #[test]
    fn a_gated_mod_inside_the_diff_becomes_a_suggestion_comment() {
        // (#2310 P4b review, M-B) The kit is a REAL unified diff, declared
        // as such via `kit_kind: "unified-diff"` — an opaque prose kit
        // (the pre-fix shape of this test) must NEVER become a suggestion;
        // see `an_opaque_kit_is_never_pasted_into_a_suggestion_block`
        // below for that proof.
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        let findings = vec![finding("s/1", "src/a.ts", 2, "const x = 1;", "reimplements a helper", None)];
        let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
        let scope = DeliverScope { rules_run: vec!["r1".into()], hunks_covered: 1, hunks_total: 1, ..Default::default() };
        let out = render_github_review(&findings, &mods, DIFF, &scope, None);
        assert_eq!(out.mode, "review");
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_eq!(review.comments[0].path, "src/a.ts");
        assert_eq!(review.comments[0].line, 2, "the hunk's old-range END line");
        assert_eq!(review.comments[0].start_line, None, "a single-line range carries no start_line");
        assert_eq!(review.comments[0].side.as_deref(), Some("RIGHT"));
        assert!(review.comments[0].body.starts_with("```suggestion\n"), "{}", review.comments[0].body);
        assert!(review.comments[0].body.contains("clamp(1)"));
    }

    #[test]
    fn an_opaque_kit_is_never_pasted_into_a_suggestion_block() {
        // (#2310 P4b review, M-B — the bug this fix removes) The EXACT
        // same kit text and the EXACT same in-diff finding as the test
        // above, but with no `kit_kind` at all — must render as a fenced
        // patch bullet, never a suggestion, even though the text alone
        // looks like it could be pasted in. This is the regression the
        // review flagged: an opaque kit (the common shape is itself a
        // unified diff) pasted verbatim into a suggestion block would let
        // "Commit suggestion" replace the anchored line with raw
        // `+++`/`@@` diff syntax.
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        let findings = vec![finding("s/1", "src/a.ts", 2, "const x = 1;", "reimplements a helper", None)];
        let mods = vec![gated_mod("s/1", KIT, Some(true))];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "an undeclared kit kind must never become an inline suggestion: {review:?}");
        assert!(review.body.contains("@@ -2,1 +2,1 @@"), "the raw diff syntax lands in the fenced bullet, untouched");
    }

    #[test]
    fn a_unified_diff_kit_hunk_outside_the_pr_diff_falls_back_to_a_fenced_bullet() {
        // (#2310 P4b review, M-B) `kit_kind: "unified-diff"` AND a real
        // parseable hunk, but the hunk's old-range (line 99) is nowhere
        // in `DIFF`'s touched lines (1-3) — must NOT become a suggestion.
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -99,1 +99,1 @@\n-old\n+new\n";
        let findings = vec![finding("s/1", "src/a.ts", 99, "old", "unrelated to the PR's own hunk", None)];
        let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "{review:?}");
        assert!(review.body.contains("new"), "the hunk's own new-side text lands in the fenced bullet");
    }

    #[test]
    fn a_unified_diff_kit_that_fails_to_parse_falls_back_to_a_fenced_bullet() {
        // (#2310 P4b review, M-B) `kit_kind: "unified-diff"` but the text
        // is not actually a diff — `parse_diff` yields zero hunks, so
        // this must never guess; opaque fallback, same as any other kind.
        let findings = vec![finding("s/1", "src/a.ts", 2, "const x = 1;", "not really a diff", None)];
        let mods = vec![gated_mod_kind("s/1", "this is not a unified diff at all", Some("unified-diff"), Some(true))];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty());
        assert!(review.body.contains("this is not a unified diff at all"));
    }

    #[test]
    fn a_never_gated_mod_gate_passed_none_becomes_a_worth_a_double_check_thread() {
        // (#2310 P4b review, M-A — proven-vacuous MUST FIX) No prior
        // fixture ever planted `gate_passed: None`, so a mutation from
        // `== Some(true)` to `!= Some(false)` slipped past every existing
        // test (`None != Some(false)` is ALSO true). This test plants
        // exactly that shape and asserts the double-check outcome; see
        // the mutation self-check in this packet's own report for the
        // red-prove against the `!= Some(false)` mutation.
        let findings = vec![finding("s/9", "src/a.ts", 2, "ev", "a mod exists but nothing ever gated it", None)];
        let mods = vec![gated_mod("s/9", "some proposed kit text", None)];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "never-gated must not become a suggestion: {review:?}");
        assert!(review.body.contains("Worth a double check"));
        assert!(review.body.contains("s/9"));
        assert!(
            !review.body.contains("some proposed kit text"),
            "a never-gated mod's kit must not render at all, only the finding's own claim/evidence"
        );
    }

    #[test]
    fn a_gate_passed_mod_wins_over_an_earlier_gate_failed_mod_for_the_same_finding() {
        // (#2310 P4b review, CONSIDER) Two mods name the same finding, the
        // gate-failed one listed FIRST — the gate-passed one (declared
        // second) must still win, proving the lookup isn't a bare
        // first-match `.find()`.
        let findings = vec![finding("s/10", "src/a.ts", 2, "const x = 1;", "reimplements a helper", None)];
        let mods = vec![
            gated_mod("s/10", "an earlier failed attempt", Some(false)),
            gated_mod_kind(
                "s/10",
                "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n",
                Some("unified-diff"),
                Some(true),
            ),
        ];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "the gate-passed mod renders, not a double-check: {review:?}");
        assert!(!review.body.contains("an earlier failed attempt"));
    }

    #[test]
    fn a_gated_mod_outside_the_diff_becomes_a_body_patch_not_a_suggestion() {
        let findings = vec![finding("s/2", "src/other.ts", 5, "irrelevant", "unrelated", None)];
        let mods = vec![gated_mod("s/2", "the patch text", Some(true))];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "not an inline suggestion: {review:?}");
        assert!(review.body.contains("the patch text"));
        assert!(!review.body.contains("```suggestion"));
    }

    #[test]
    fn a_finding_with_no_mod_becomes_a_worth_a_double_check_thread() {
        let findings = vec![finding("s/3", "src/a.ts", 2, "the cited line", "might duplicate an existing helper", None)];
        let out = render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty());
        assert!(review.body.contains("Worth a double check"));
        assert!(review.body.contains("s/3"));
        assert!(review.body.contains("might duplicate an existing helper"));
    }

    #[test]
    fn a_gate_failed_mod_becomes_a_worth_a_double_check_thread_not_a_suggestion() {
        let findings = vec![finding("s/4", "src/a.ts", 2, "ev", "claim", None)];
        let mods = vec![gated_mod("s/4", "kit text", Some(false))];
        let out = render_github_review(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty());
        assert!(review.body.contains("Worth a double check"));
        assert!(!review.body.contains("kit text"));
    }

    #[test]
    fn a_search_form_finding_lists_its_instances() {
        let findings =
            vec![finding("s/5", "src/mw.ts", 10, "14 endpoints use this middleware", "shared auth changed", Some("search"))];
        let out = render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.body.contains("14 endpoints use this middleware"));
        assert!(review.body.contains("search-confirmed"));
    }

    #[test]
    fn a_question_form_finding_renders_as_a_question_with_candidates() {
        let findings = vec![finding(
            "s/6",
            "src/x.ts",
            1,
            "Status enum, Kind enum",
            "did you check whether the repo already has this",
            Some("question"),
        )];
        let out = render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.body.contains("Status enum, Kind enum"));
        assert!(review.body.contains("did you check whether the repo already has this"));
    }

    #[test]
    fn refused_findings_never_render_only_the_scope_count_shows() {
        let scope = DeliverScope { refused: 3, rules_run: vec!["r1".into()], ..Default::default() };
        let findings = vec![finding("s/7", "src/a.ts", 2, "ev", "claim", None)];
        let out = render_github_review(&findings, &[], DIFF, &scope, None);
        let review = out.review.unwrap();
        assert!(review.body.contains("3 refused"));
    }

    #[test]
    fn the_scope_line_states_what_was_not_attempted() {
        let scope = DeliverScope {
            rules_run: vec!["r1".into(), "r2".into()],
            hunks_covered: 2,
            hunks_total: 5,
            not_attempted: vec!["architectural review".into()],
            ..Default::default()
        };
        let line = scope_line(&scope, 4);
        assert!(line.contains("2 rule(s)"));
        assert!(line.contains("2/5 hunks"));
        assert!(line.contains("4 finding(s)"));
        assert!(line.contains("Not attempted: architectural review."));
    }

    #[test]
    fn nothing_to_say_is_a_noop_not_an_empty_review() {
        let out = render_github_review(&[], &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.mode, "noop");
        assert!(out.review.is_none());
    }

    #[test]
    fn line_touched_mutation_kill_a_context_line_counts_same_as_an_added_line() {
        // (#2310 P4b self-QA) The suggestion-vs-patch branch hinges on
        // `line_touched` treating context lines the same as added lines —
        // line 1 (` function f() {`) is context-only in `DIFF` above.
        let touched = diff_touched_lines(DIFF);
        assert!(line_touched(&touched, "src/a.ts", Some(1)), "context line must count as touched");
        assert!(!line_touched(&touched, "src/a.ts", Some(99)), "a line outside the hunk must not");
        assert!(!line_touched(&touched, "src/other.ts", Some(1)), "a file never in the diff must not");
    }

    #[test]
    fn a_line_one_past_the_hunks_end_is_outside_the_diff_through_the_shared_parser() {
        // (#2310 P4b, coordinator-requested red-prove) `DIFF`'s
        // `@@ -1,2 +1,3 @@` hunk spans new-side lines 1-3 exactly
        // (` function f() {`, `+  const x = 1;`, ` }`) — resolved through
        // `crate::diff::parse_diff`, the SAME parser `darkmux-lab`'s
        // bundler uses (moved down in #2310 P4b so this crate never
        // hand-rolls a second one). Line 3 (the hunk's own last line) is
        // touched; line 4 (one past it) is not.
        let touched = diff_touched_lines(DIFF);
        assert!(line_touched(&touched, "src/a.ts", Some(3)), "the hunk's own last line must count as touched");
        assert!(!line_touched(&touched, "src/a.ts", Some(4)), "one line past the hunk's end must not");
    }

    #[test]
    fn the_kind_registers_via_its_own_dedicated_function() {
        let registry = StepKindRegistry::new();
        register_deliver_kind(&registry).unwrap();
        assert!(registry.ids().iter().any(|id| id == DELIVER_GITHUB_REVIEW_KIND));
    }

    #[test]
    fn the_step_kind_run_reads_config_and_emits_to_the_named_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let out_path = dir.path().join("out.json");
        let step = Step {
            id: "deliver-step".into(),
            task_id: "deliver-task".into(),
            kind: DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: crate::types::NodeStatus::Planned,
            config: json!({
                "findings": [],
                "mods": [],
                "diff": DIFF,
                "scope": {},
                "emit": out_path.to_string_lossy(),
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = Task {
            run_on: crate::types::default_run_on(),
            id: "deliver-task".into(),
            phase_id: "p".into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["deliver-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let outcome = DeliverGithubReviewStepKind.run(&step, &task, &BTreeMap::new()).unwrap();
        assert_eq!(outcome.output, out_path.to_string_lossy());
        let written = std::fs::read_to_string(&out_path).unwrap();
        let parsed: DeliverOutcome = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed.mode, "noop", "zero findings, zero mods: nothing to say");
    }

    /// (#2310 P4b) The golden the brief asks for: one fixture set of
    /// findings + mods covering EVERY delivery form named in DESIGN.md
    /// (mod-in-diff/suggestion, mod-outside-diff/patch, no-mod/double-
    /// check, gate-failed/double-check, search, question) plus the scope
    /// line's refused/not-attempted counts, rendered and compared
    /// byte-for-byte against a committed golden file. The finding/mod
    /// VALUES are hand-specified in this test via the same `finding()`/
    /// `gated_mod()` helpers every other test in this module uses
    /// (synthetic, sanitized — no real repo content) — the golden is the
    /// RENDERED PAYLOAD, which is what a drift in this module's logic
    /// would actually change.
    ///
    /// To regenerate after a deliberate behavior change:
    /// `DARKMUX_DELIVER_GOLDEN_UPDATE=1 cargo test -p darkmux-crew --lib \
    ///  step_kinds::deliver_github_review::tests::golden_rendered_payload_covers_every_delivery_form`
    /// then review the diff before committing.
    #[test]
    fn golden_rendered_payload_covers_every_delivery_form() {
        const CLAMP_KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        let findings = vec![
            finding("sess-a/1", "src/a.ts", 2, "const x = 1;", "reimplements clamp()", None),
            finding("sess-a/2", "src/other.ts", 5, "irrelevant", "unrelated change", None),
            finding("sess-a/3", "src/a.ts", 2, "the cited line", "might duplicate an existing helper", None),
            finding("sess-a/4", "src/a.ts", 2, "ev", "a gate-failed claim", None),
            finding("sess-a/5", "src/mw.ts", 10, "14 endpoints use this middleware", "shared auth changed", Some("search")),
            finding(
                "sess-a/6",
                "src/x.ts",
                1,
                "Status enum, Kind enum",
                "did you check whether the repo already has this",
                Some("question"),
            ),
            // (#2310 P4b review, M-A) A never-gated mod — `gate_passed:
            // None` — planted so this golden isn't vacuous against the
            // `== Some(true)` -> `!= Some(false)` mutation (see the
            // packet report's mutation self-check).
            finding("sess-a/7", "src/a.ts", 2, "ev", "a mod exists but nothing ever gated it", None),
        ];
        let mods = vec![
            // (#2310 P4b review, M-B) A REAL unified-diff kit, declared
            // via `kit_kind`, whose hunk sits inside the PR diff's own
            // touched lines — the one case that renders an inline
            // suggestion.
            gated_mod_kind("sess-a/1", CLAMP_KIT, Some("unified-diff"), Some(true)),
            gated_mod("sess-a/2", "the patch text", Some(true)),
            gated_mod("sess-a/4", "kit text nobody sees", Some(false)),
            gated_mod("sess-a/7", "never-gated kit text nobody sees either", None),
        ];
        let scope = DeliverScope {
            rules_run: vec!["unnamed-predicate".into(), "existing-solution".into()],
            hunks_covered: 3,
            hunks_total: 4,
            refused: 2,
            not_attempted: vec!["architectural review".into()],
            errored: Vec::new(),
        };

        let outcome = render_github_review(&findings, &mods, DIFF, &scope, Some("Advisory, not a merge gate."));
        let actual = serde_json::to_string_pretty(&outcome).unwrap();

        let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/deliver-github-review/every-delivery-form.json");
        if std::env::var("DARKMUX_DELIVER_GOLDEN_UPDATE").is_ok() {
            std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
            std::fs::write(&golden_path, format!("{actual}\n")).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|_| {
            panic!("read {} — run with DARKMUX_DELIVER_GOLDEN_UPDATE=1 to generate it", golden_path.display())
        });
        assert_eq!(
            actual.trim_end(),
            expected.trim_end(),
            "the rendered payload drifted from the committed golden at {}.\n\
             If this drift is an intended behavior change, regenerate with:\n\
             DARKMUX_DELIVER_GOLDEN_UPDATE=1 cargo test -p darkmux-crew --lib \
             step_kinds::deliver_github_review::tests::golden_rendered_payload_covers_every_delivery_form\n\
             then review the diff before committing.",
            golden_path.display()
        );

        // Sanity: every delivery form actually fired, or a broken fixture
        // could pass this golden vacuously.
        assert_eq!(outcome.mode, "review");
        let review = outcome.review.unwrap();
        assert_eq!(review.comments.len(), 1, "exactly one in-diff suggestion");
        assert_eq!(review.comments[0].side.as_deref(), Some("RIGHT"));
        assert!(review.body.contains("the patch text"), "mod-outside-diff");
        assert!(review.body.contains("might duplicate"), "no-mod double-check");
        assert!(review.body.contains("a gate-failed claim"), "gate-failed double-check");
        assert!(review.body.contains("a mod exists but nothing ever gated it"), "never-gated double-check");
        assert!(!review.body.contains("kit text nobody sees"), "a gate-failed mod's kit never renders");
        assert!(!review.body.contains("never-gated kit text"), "a never-gated mod's kit never renders");
        assert!(review.body.contains("14 endpoints"), "search form");
        assert!(review.body.contains("Status enum, Kind enum"), "question form");
        assert!(review.body.contains("2 refused"));
        assert!(review.body.contains("Not attempted: architectural review."));
    }

    /// (#2310 P4b review, CONSIDER — renamed to what it actually asserts)
    /// `emit: "-"` resolves to the SAME `Path::new("-")` convention
    /// `review_render::emit_rendered` uses (reimplemented independently —
    /// see this module's doc for why it cannot depend on that one) and the
    /// step reports `"-"` as its own output marker rather than a file
    /// path. This does NOT capture actual stdout bytes (`println!` isn't
    /// trivially interceptable from a plain `#[test]`) — it proves the
    /// DESTINATION decision, not the byte-for-byte "one JSON line and
    /// nothing else" purity claim the old name implied.
    #[test]
    fn emit_dash_step_output_names_stdout_not_a_path() {
        let step = Step {
            id: "deliver-step".into(),
            task_id: "deliver-task".into(),
            kind: DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: crate::types::NodeStatus::Planned,
            config: json!({
                "findings": [finding("sess-a/1", "src/a.ts", 2, "ev", "claim", None)],
                "mods": [],
                "diff": DIFF,
                "scope": {},
                "emit": "-",
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = Task {
            run_on: crate::types::default_run_on(),
            id: "deliver-task".into(),
            phase_id: "p".into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["deliver-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let outcome = DeliverGithubReviewStepKind.run(&step, &task, &BTreeMap::new()).unwrap();
        assert_eq!(outcome.output, "-", "the step's own output names stdout, not a path");
    }
}
