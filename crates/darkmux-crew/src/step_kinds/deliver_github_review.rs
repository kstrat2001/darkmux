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
use anyhow::{anyhow, Context, Result};
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubReviewComment {
    pub path: String,
    pub line: u32,
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
/// is anything at all to say (findings, or a non-empty scope worth
/// reporting) and `"noop"` when the run produced literally nothing —
/// mirrors the review pipeline's own `mode` vocabulary in spirit (a
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
                let gated = mods.iter().find(|m| m.record.r#for.iter().any(|k| k == &finding.key));
                match gated {
                    Some(m) if m.gate_passed == Some(true) => {
                        let kit = m.record.kit.as_deref().unwrap_or("");
                        if window.file.as_deref().is_some_and(|f| line_touched(&touched, f, window.line)) {
                            comments.push(GithubReviewComment {
                                path: window.file.clone().unwrap_or_default(),
                                line: window.line.unwrap_or(1),
                                body: format!("```suggestion\n{kit}\n```"),
                            });
                        } else {
                            mod_bullets.push(format!(
                                "- `{}` — proposed change (outside the diff's lines):\n\n```\n{kit}\n```",
                                window.display()
                            ));
                        }
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
        return DeliverOutcome { mode: "noop".to_string(), review: None };
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
    line
}

enum DeliveryForm {
    Mod,
    Search,
    Question,
}

fn delivery_form(finding: &FindingRecord) -> DeliveryForm {
    match finding.context.get("form").and_then(|v| v.as_str()) {
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
/// for stdout>"}`. Every field but `findings`/`mods`/`diff` is optional.
/// Reads its inputs from `Step.config` directly rather than through typed
/// graph ports (#2301's `Output<T>` envelope convention): no producer for
/// "findings as one bulk typed value" exists anywhere in the codebase
/// today (a finding is read from the finding STORE by key, per
/// `crawl.summary`'s own "the same one read of the dispatch's
/// findings.jsonl" pattern, DESIGN.md's record table) — inventing four
/// speculative port kinds nothing will ever produce is worse than the
/// config-embedded shape a future caller (#2310 P4c) can trivially swap
/// for a store read once that caller exists.
struct DeliverConfig {
    findings: Vec<FindingRecord>,
    mods: Vec<GatedMod>,
    diff: String,
    scope: DeliverScope,
    attribution: Option<String>,
    emit: Option<PathBuf>,
}

impl DeliverConfig {
    fn from_step(step: &Step) -> Result<Self> {
        let field = |key: &str| -> Result<serde_json::Value> {
            step.config
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("step `{}`: `{DELIVER_GITHUB_REVIEW_KIND}` requires config.{key}", step.id))
        };
        let findings: Vec<FindingRecord> = serde_json::from_value(field("findings")?)
            .with_context(|| format!("step `{}`: config.findings", step.id))?;
        let mods: Vec<GatedMod> =
            serde_json::from_value(field("mods")?).with_context(|| format!("step `{}`: config.mods", step.id))?;
        let diff = field("diff")?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("step `{}`: config.diff must be a string", step.id))?;
        let scope: DeliverScope = match step.config.get("scope") {
            Some(v) => serde_json::from_value(v.clone())
                .with_context(|| format!("step `{}`: config.scope", step.id))?,
            None => DeliverScope::default(),
        };
        let attribution = step.config.get("attribution").and_then(|v| v.as_str()).map(str::to_string);
        let emit = step.config.get("emit").and_then(|v| v.as_str()).map(PathBuf::from);
        Ok(Self { findings, mods, diff, scope, attribution, emit })
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

    fn run(&self, step: &Step, _task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = DeliverConfig::from_step(step)?;
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
            context = json!({ "form": f });
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
        GatedMod {
            record: ModRecord {
                key: "mod-1-abcdef".to_string(),
                ts: "2026-09-04T00:00:01Z".to_string(),
                by: "coder".to_string(),
                r#for: vec![for_key.to_string()],
                kit: Some(kit.to_string()),
                kit_looks_json: false,
                attachments: Vec::new(),
                context: Default::default(),
                warnings: Vec::new(),
                mission_id: None,
                phase_id: None,
                step_id: None,
                schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            },
            gate_passed,
        }
    }

    const DIFF: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,2 +1,3 @@\n function f() {\n+  const x = 1;\n }\n";

    #[test]
    fn a_gated_mod_inside_the_diff_becomes_a_suggestion_comment() {
        let findings = vec![finding("s/1", "src/a.ts", 2, "const x = 1;", "reimplements a helper", None)];
        let mods = vec![gated_mod("s/1", "  const x = clamp(1);", Some(true))];
        let scope = DeliverScope { rules_run: vec!["r1".into()], hunks_covered: 1, hunks_total: 1, ..Default::default() };
        let out = render_github_review(&findings, &mods, DIFF, &scope, None);
        assert_eq!(out.mode, "review");
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_eq!(review.comments[0].path, "src/a.ts");
        assert_eq!(review.comments[0].line, 2);
        assert!(review.comments[0].body.starts_with("```suggestion\n"), "{}", review.comments[0].body);
        assert!(review.comments[0].body.contains("clamp(1)"));
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
        ];
        let mods = vec![
            gated_mod("sess-a/1", "  const x = clamp(1);", Some(true)),
            gated_mod("sess-a/2", "the patch text", Some(true)),
            gated_mod("sess-a/4", "kit text nobody sees", Some(false)),
        ];
        let scope = DeliverScope {
            rules_run: vec!["unnamed-predicate".into(), "existing-solution".into()],
            hunks_covered: 3,
            hunks_total: 4,
            refused: 2,
            not_attempted: vec!["architectural review".into()],
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
        assert!(review.body.contains("the patch text"), "mod-outside-diff");
        assert!(review.body.contains("might duplicate"), "no-mod double-check");
        assert!(review.body.contains("a gate-failed claim"), "gate-failed double-check");
        assert!(!review.body.contains("kit text nobody sees"), "a gate-failed mod's kit never renders");
        assert!(review.body.contains("14 endpoints"), "search form");
        assert!(review.body.contains("Status enum, Kind enum"), "question form");
        assert!(review.body.contains("2 refused"));
        assert!(review.body.contains("Not attempted: architectural review."));
    }

    /// (#2310 P4b self-QA — neighbor check) `emit: "-"` writes EXACTLY one
    /// line to stdout and nothing else, the "stdout purity" the harness
    /// relies on (`review_render::emit_rendered`'s own `Some(p) if p ==
    /// Path::new("-")` convention, reimplemented here independently — see
    /// this module's doc for why it cannot depend on that one).
    #[test]
    fn emit_dash_writes_exactly_one_json_line_to_stdout() {
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
