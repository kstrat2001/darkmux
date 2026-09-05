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
use crate::step_kinds::types::{Port, SeatClaim, StepKind, StepOutcome, StepRunCtx};
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
    /// (#2310 fix-loop E2) How many rules this run's config DECLARED —
    /// the denominator the scope line's "N of M rules reviewed" needs.
    /// `rules_run` alone can only say how many ran, and "2 rule(s)" reads
    /// as complete coverage of a 2-rule review when the config had 7.
    ///
    /// Additive (`#[serde(default)]`), so a caller that does not set it
    /// reads back `0` and [`Self::rules_declared`] falls back to what the
    /// other two lists can prove.
    #[serde(default)]
    pub rules_total: usize,
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

/// (#2310 fix-loop E2, S1-6) The standing narrowness of EVERY run this kind
/// delivers, stated once in the scope line whatever the per-run numbers
/// say. `not_attempted` names what THIS run left out; this names what the
/// mechanism itself does not do, which no run-shaped number can ever
/// reveal. DESIGN.md's "The honest limit": a narrow review must never read
/// as complete, and a reader who sees "7 of 7 rules reviewed, 12/12 hunks
/// covered" has been told nothing about the whole class of problems a
/// rule-shaped review cannot see.
const STANDING_NARROWNESS: &str =
    "This is a rule-shaped review; architectural breadth stays with the frontier gate.";

impl DeliverScope {
    /// The denominator for "N of M rules reviewed". `rules_total` when the
    /// producer set it (never below the number that actually ran — a
    /// smaller declared count than run count is a bookkeeping error, and
    /// "3 of 2" would be a worse lie than a slightly generous M); otherwise
    /// what the two lists can prove between them.
    pub fn rules_declared(&self) -> usize {
        if self.rules_total > 0 {
            self.rules_total.max(self.rules_run.len())
        } else {
            self.rules_run.len() + self.not_attempted.len()
        }
    }
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
/// genuinely CLEAN run — nothing found, nothing went wrong
/// (`scope.errored` empty), AND the whole diff was covered
/// (`hunks_covered >= hunks_total`, or nothing to cover at all);
/// `"degraded"` (#2310 P4c-2b PR #2357 review MUST FIX D, widened onto
/// the coverage axis by #2310 fix loop A / S3-2) when nothing was found
/// to say but the run was not clean — an errored/abandoned unit or plan
/// step, or hunks the run never covered, means this run is NOT a clean
/// pass even with zero findings, and must never read as one.
/// Mirrors the review pipeline's own `mode` vocabulary in spirit (a
/// distinct outcome, never silently folded into `"review"`) without
/// depending on its type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliverOutcome {
    pub mode: String,
    pub review: Option<GithubReviewPayload>,
}

// ─── Model text is untrusted markdown (#2310 fix loop A, S5-4) ──────────
//
// Everything this module interpolates into the review body — a finding's
// `why`/`evidence`/`file`, a mod's opaque `kit`, a kit hunk's new-side
// lines and its path — is MODEL-authored, and on a review of a public PR
// the model's own input (the diff) is attacker-influenced. Rendered
// naively it is not just sloppy output: a payload that closes its
// container forges a `### darkmux review` header, a fabricated scope line
// and verdict, and a second one-click `suggestion` block, all in
// darkmux's voice under darkmux's byline.
//
// Four rules, applied at every sink below, so the containment is
// structural rather than per-call-site care:
//
// 1. **A fenced block picks a fence LONGER than anything in its payload**
//    ([`fence_for`]) — the CommonMark rule that a closing fence must be at
//    least as long as its opener. GitHub honors ````suggestion the same as
//    ```suggestion, so this covers suggestion blocks too.
// 2. **Text interpolated into a one-line bullet is folded onto that line**
//    ([`inline_text`]) — every RUN of newlines becomes ONE space, so
//    model text can never reach column 0 and therefore can never open a
//    fence or start a `#` heading, a `---`/`***` break, or a `>` quote.
//    Folding (rather than prefixing every line) is what keeps a bullet a
//    bullet.
// 3. **`<` in that same text becomes `&lt;`** ([`inline_text`], #2310 fix
//    loop E1-1) — the one rule here that is a security property rather
//    than a rendering one. GitHub renders raw inline HTML in comment
//    bodies and permits `<img src=…>`, so an unescaped `<` would let a
//    review posted under darkmux's byline fetch an attacker-chosen URL
//    from every reader of it. `&` and `>` are deliberately NOT escaped —
//    [`inline_text`]'s own doc says why, and `inline_text_probe_table`
//    pins both decisions.
// 4. **Text shown as code picks a code-span delimiter longer than any
//    backtick run inside it** ([`code_span`]) — the inline analogue of
//    rule 1, which preserves the text EXACTLY (a path is evidence; a
//    look-alike substitution there would be a lie). Prose has no such
//    delimiter to lengthen, so [`inline_text`] substitutes a look-alike
//    for a backtick instead: an unterminated span in prose would otherwise
//    swallow the bullets that follow it.
//
// Convention note: `darkmux-lab`'s `review_render::public_safe_note` is
// the same discipline at the same boundary (untrusted text about to be
// posted to a public PR, collapsed to one line and bounded) — this crate
// cannot depend on that one (see this module's own doc), so the shape is
// matched deliberately, not shared.

/// The backtick run a fence around `payload` must use: one longer than the
/// longest run inside it, never shorter than three.
fn fence_for(payload: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in payload.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// U+02CB MODIFIER LETTER GRAVE ACCENT — a backtick's look-alike that
/// carries no markdown meaning. Used by [`inline_text`] only (running
/// prose has no delimiter to lengthen); [`code_span`] preserves real
/// backticks exactly.
const BACKTICK_LOOKALIKE: char = '\u{02cb}';

/// Model prose on its way into a one-line bullet: every RUN of line breaks
/// becomes ONE space, every backtick becomes [`BACKTICK_LOOKALIKE`], and
/// every `<` becomes `&lt;`. The text stays readable and complete —
/// nothing is dropped or truncated — it just loses the characters that let
/// it act as markdown structure or as raw HTML.
///
/// Three deliberate non-substitutions, each load-bearing enough to have
/// its own row in `inline_text_probe_table` (#2310 fix loop E1):
///
/// - **`>` and `&` are left alone.** `&` decodes to a literal CHARACTER in
///   CommonMark's text stream, never to markup — a model writing `&lt;`
///   gets a visible `<` on the page, not a tag — so escaping it would only
///   mangle prose. `>` opens a block quote at column 0 only, and this
///   function's whole job is that model text never reaches column 0.
/// - **`#`, `-`, `>`, `1.` are left alone** for the same reason: every
///   call site interpolates the result mid-line.
/// - **U+2028 / U+2029 are left alone.** CommonMark's "line endings" are
///   LF, CR and CRLF; cmark-gfm does not break a line on the Unicode
///   separators, so they cannot carry text to column 0 either.
///
/// (#2310 fix loop E1-1) The `<` escape is the one that is a security
/// property rather than a rendering one: GitHub renders raw inline HTML in
/// comment bodies and permits `<img src=…>`, so an unescaped `<` in model
/// prose would let a review posted under darkmux's byline fetch an
/// attacker-chosen URL from every reader.
fn inline_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // (#2310 fix loop E1-4) A CRLF is ONE line break and a blank line is
    // still one break; mapping each break character to its own space left
    // `a\r\nb` reading `a  b`.
    let mut in_break = false;
    for c in s.chars() {
        if c == '\n' || c == '\r' {
            if !in_break {
                out.push(' ');
                in_break = true;
            }
            continue;
        }
        in_break = false;
        match c {
            '`' => out.push(BACKTICK_LOOKALIKE),
            '<' => out.push_str("&lt;"),
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Model text shown as code: folded onto one line, then wrapped in a
/// delimiter longer than any backtick run it contains (CommonMark strips
/// one padding space on each side when the content itself starts or ends
/// with a backtick). The content is preserved byte-for-byte inside the
/// span — a file path is evidence a reader may need to paste.
///
/// Two rules that differ from [`inline_text`]'s on purpose: a line break
/// maps to exactly one space PER CHARACTER (a CRLF stays two spaces), so a
/// pathological path stays visibly pathological rather than being tidied
/// away; and `<` is left alone, because inside a code span it is inert and
/// escaping it would corrupt the evidence.
fn code_span(content: &str) -> String {
    let flat: String = content.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    // (#2310 fix loop E1-3) Empty content would otherwise render as two
    // adjacent backticks — an UNCLOSED backtick string, which CommonMark
    // prints literally. Not a breakout, but a visible artifact in a
    // posted review, and reachable: `FindingWindow::display` returns a
    // bare `key`, so a finding with an empty key lands here.
    if flat.trim().is_empty() {
        return "`(empty)`".to_string();
    }
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in flat.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let delim = "`".repeat(longest + 1);
    let pad = if flat.starts_with('`') || flat.ends_with('`') { " " } else { "" };
    format!("{delim}{pad}{flat}{pad}{delim}")
}

/// Render findings + mods + a diff + a scope summary into a
/// [`DeliverOutcome`]. Pure — no I/O, no model dispatch — so this is what
/// this packet's golden tests call directly. `rule_titles` maps a rule id
/// to its author-facing title (`Rule::title`); [`rule_titles`] is how the
/// step kind builds one, and a caller with none may pass an empty map.
///
/// (#2310 delivery rewrite) **Every entry is headed by the RULE it came
/// from, never by how it was found.** The author of a pull request does
/// not care which internal procedure surfaced a problem — but the rule
/// itself is useful to them the way a lint rule name is: it names what was
/// violated and is the handle for re-running the same check after a fix.
/// So the body groups entries under the rule's own title, tagged with the
/// rule id, and an entry reads: the location (`path:line`), the finding's
/// own claim (`why`), then whichever applies —
///
/// - **a gate-passed mod whose hunk sits inside the diff** — the change
///   rides as an inline `comments[]` suggestion on that line, and the body
///   entry says so. This is true WHATEVER the rule's confirmation form:
///   a passed change is a passed change, and dropping one because the rule
///   happened to be confirmed by searching is what shipped a review with a
///   ready patch nobody saw.
/// - **a gate-passed mod outside the diff (or one that is not an
///   applicable patch)** — the change quoted as a fenced block, with a
///   plain-words reason.
/// - **a question the finding answers** — the claim as a question, with
///   any candidates the finding recorded.
///
/// Everything else is a lead, not a headline: a finding with no passed
/// change lands in the single cross-rule tail section, **Worth a double
/// check**, still headed by its rule inside it. That deliberately includes
/// a search-shaped finding with no change proposed. The reason is
/// evidential: a finding whose search came back "nothing to change here"
/// (`shared-symbol-callers`' "all callers already match") is
/// indistinguishable, MECHANICALLY, from one that found a real problem —
/// `create_finding`'s tool arguments are fixed at `{file, line, pattern,
/// evidence, why}` (`runtime/src/tools/mod.rs`) with no severity/holds/
/// action field to read, and prose-matching a model's `why` for
/// all-clear-ness is exactly the kind of guess this module refuses to
/// make. Widening the FINDING contract is a separate change; until then
/// the honest placement for "somebody looked, nothing was patched" is the
/// thread-to-pull section, never a headline that reads as a defect.
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
    rule_titles: &BTreeMap<String, String>,
) -> DeliverOutcome {
    let touched = diff_touched_lines(diff);

    let mut comments: Vec<GithubReviewComment> = Vec::new();
    let mut headline: Vec<RuleGroup> = Vec::new();
    let mut double_check: Vec<RuleGroup> = Vec::new();

    for finding in findings {
        let window = FindingWindow::from(finding);
        let rule = rule_id_of(finding);
        // (#2310 P4b review, CONSIDER) Prefer a GATE-PASSED mod over an
        // earlier gate-failed one naming the same finding: without this, a
        // coder's second (successful) attempt at a finding could lose to
        // its own first failed one just because it landed later in `mods`.
        let gated = mods
            .iter()
            .filter(|m| m.record.r#for.iter().any(|k| k == &finding.key))
            .find(|m| m.gate_passed == Some(true));
        if let Some(m) = gated {
            // (#2310 delivery rewrite, rule 2) The FORM does not gate this
            // branch — a passed change renders as a change no matter which
            // way its rule is confirmed. The form decides only what to
            // render when there is no passed change at all.
            let bullets = group_for(&mut headline, rule.as_deref(), rule_titles);
            render_gated_mod(m, &window, &touched, &mut comments, bullets);
            continue;
        }
        match delivery_form(finding) {
            DeliveryForm::Question => {
                let bullets = group_for(&mut headline, rule.as_deref(), rule_titles);
                bullets.push(question_bullet(&window));
            }
            // (#2310 delivery rewrite, rule 3) A search-shaped finding with
            // no passed change is a lead, not a headline — see this
            // function's own doc for why the all-clear case cannot be
            // detected mechanically today.
            DeliveryForm::Search | DeliveryForm::Mod => {
                let bullets = group_for(&mut double_check, rule.as_deref(), rule_titles);
                bullets.push(double_check_bullet(&window));
            }
        }
    }

    let unresolved = unresolved_rule_titles(findings, rule_titles);

    // Captured before `body` takes ownership below — whether there was
    // anything to say at all decides `mode`.
    let nothing_to_say = comments.is_empty() && headline.is_empty() && double_check.is_empty();
    if nothing_to_say {
        // (#2310 fix loop A, S3-2 — PROVEN) "Nothing was found" only means
        // a clean run when the run actually LOOKED. A scope of
        // `hunks_covered: 0, hunks_total: 5` rendered `noop` with
        // `review: null` before this fix, discarding the one fact that
        // mattered — the same "an incomplete run renders nothing" defect
        // as MUST FIX D one line below, on the coverage axis instead of
        // the error axis. `hunks_total == 0` is the honest zero (nothing
        // to cover, e.g. an empty diff), not an uncovered run.
        let covered = scope.hunks_total == 0 || scope.hunks_covered >= scope.hunks_total;
        if scope.errored.is_empty() && covered {
            // A genuinely CLEAN run — nothing to say because nothing went
            // wrong, nothing was found, and the whole diff was looked at.
            // The only `mode` this applies to.
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
        let mut body =
            vec!["### darkmux review".to_string(), String::new(), scope_line(scope, findings.len(), &unresolved)];
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
    body.push(scope_line(scope, findings.len(), &unresolved));
    for group in &headline {
        body.push(String::new());
        body.push(group.heading("**"));
        body.extend(group.bullets.iter().cloned());
    }
    if !double_check.is_empty() {
        body.push(String::new());
        body.push("**Worth a double check** (not merge-blocking):".to_string());
        for group in &double_check {
            body.push(String::new());
            body.push(group.heading("_"));
            body.extend(group.bullets.iter().cloned());
        }
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

/// One rule's entries, in the order their findings arrived. The rule — not
/// the confirmation procedure — is what heads a group: see
/// [`render_github_review`]'s own doc.
struct RuleGroup {
    rule_id: Option<String>,
    /// The rule's author-facing title, when one resolved. `None` falls the
    /// heading back to the rule id itself (and the scope line names the
    /// rule whose title was missing).
    title: Option<String>,
    bullets: Vec<String>,
}

impl RuleGroup {
    /// `emphasis` is the markdown wrapper for the rule's NAME — `**` for a
    /// headline group, `_` for a group nested inside the double-check
    /// tail. The rule id rides as a code span beside it: the author's
    /// handle for re-running this exact check after a fix.
    fn heading(&self, emphasis: &str) -> String {
        match (&self.rule_id, &self.title) {
            (Some(id), Some(title)) => format!("{emphasis}{title}{emphasis} {}", code_span(id)),
            (Some(id), None) => format!("{emphasis}{}{emphasis}", inline_text(id)),
            (None, _) => format!("{emphasis}Other findings{emphasis}"),
        }
    }
}

/// The rule a finding names — `context.rule` (host-stamped by the crawl
/// unit's own dispatch: `crawl::unit_step::run`'s `record_context`), or
/// the first entry of `context.rules` when a run stamped only the list.
fn rule_id_of(finding: &FindingRecord) -> Option<String> {
    if let Some(id) = finding.context.get("rule").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    finding
        .context
        .get("rules")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The rules these findings name that `rule_titles` could not title —
/// surfaced in the scope line so a heading that fell back to a bare rule
/// id is explained rather than just odd.
fn unresolved_rule_titles(findings: &[FindingRecord], rule_titles: &BTreeMap<String, String>) -> BTreeSet<String> {
    findings.iter().filter_map(rule_id_of).filter(|id| !rule_titles.contains_key(id)).collect()
}

/// The group for `rule`, appended if this is its first entry — so groups
/// render in the order their first finding arrived, not in id order.
fn group_for<'g>(
    groups: &'g mut Vec<RuleGroup>,
    rule: Option<&str>,
    rule_titles: &BTreeMap<String, String>,
) -> &'g mut Vec<String> {
    let key = rule.map(str::to_string);
    if let Some(i) = groups.iter().position(|g| g.rule_id == key) {
        return &mut groups[i].bullets;
    }
    let title = key.as_ref().and_then(|id| rule_titles.get(id)).map(|t| inline_text(t));
    groups.push(RuleGroup { rule_id: key, title, bullets: Vec::new() });
    let last = groups.len() - 1;
    &mut groups[last].bullets
}

/// Author-facing rule titles, read from the SAME registry
/// `crawl::unit_step::run` resolves a rule from (`crate::rules`, embedded
/// built-ins plus the `<darkmux root>/rules` user tier). Lenient by
/// construction: [`crate::rules::load_all`] never fails, and a rule that
/// declares no `title` simply has no entry here, which
/// [`render_github_review`] renders as the bare rule id.
pub fn rule_titles() -> BTreeMap<String, String> {
    let user_dir = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root.join("rules");
    let (rules, _warnings) = crate::rules::load_all(Some(&user_dir));
    rules.into_iter().filter_map(|(id, rule)| rule.title.map(|t| (id, t))).collect()
}

/// The finding's own claim, folded onto one line.
fn claim(window: &FindingWindow) -> String {
    inline_text(window.why.as_deref().unwrap_or("(no claim recorded)"))
}

/// The claim with sentence punctuation, for the entries that follow it
/// with a sentence of darkmux's own. A model writes `why` as a fragment as
/// often as not, and "reimplements clamp() The change touches lines…" runs
/// the two voices together.
fn claim_sentence(window: &FindingWindow) -> String {
    let claim = claim(window);
    if claim.ends_with(['.', '!', '?', ':', ';', ',']) {
        claim
    } else {
        format!("{claim}.")
    }
}

/// The body entry for a change that rode out as inline suggestion(s): the
/// location and the claim, plus where the change itself went.
fn suggestion_bullet(window: &FindingWindow, suggested: usize) -> String {
    let tail = if suggested == 1 {
        "A suggested change is attached to that line."
    } else {
        "Suggested changes are attached to those lines."
    };
    format!("- {} — {} {tail}", window.span(), claim_sentence(window))
}

/// A finding whose rule asks a question: the claim IS the answer plus its
/// reasoning (the crawl unit instructs the model to put both at the start
/// of `why`), so it renders as the question, with any candidates the
/// finding recorded.
fn question_bullet(window: &FindingWindow) -> String {
    let claim = claim(window);
    let question = if claim.ends_with('?') { claim } else { format!("{claim}?") };
    match window.evidence.as_deref().map(inline_text).filter(|e| !e.is_empty()) {
        Some(candidates) => format!("- {} — {question} Candidates: {candidates}", window.span()),
        None => format!("- {} — {question}", window.span()),
    }
}

/// A lead, not a headline — the tail section's entry shape.
fn double_check_bullet(window: &FindingWindow) -> String {
    format!(
        "- {} — {} _Worth checking: {}_",
        window.span(),
        claim_sentence(window),
        inline_text(window.evidence.as_deref().unwrap_or("the cited line"))
    )
}

/// DESIGN.md "rules run, hunks covered / total, findings by delivery form,
/// refused count, and what the review did not attempt. Never reads as
/// complete."
fn scope_line(scope: &DeliverScope, findings_considered: usize, unresolved_rules: &BTreeSet<String>) -> String {
    // (#2310 fix-loop E2) "N of M rules reviewed" — loop D made the VALUES
    // honest (a rule with no completed unit no longer counts as run); the
    // wording still said "2 rule(s)", which reads as the whole review
    // rather than as two of seven.
    let mut line = format!(
        "review ran: {} of {} rules reviewed, {}/{} hunks covered, {} finding(s) considered, {} refused.",
        scope.rules_run.len(),
        scope.rules_declared(),
        scope.hunks_covered,
        scope.hunks_total,
        findings_considered,
        scope.refused,
    );
    // (#2310 fix loop E1-2) Both lists go through `inline_text` per entry.
    // They are host-authored today, but this is COLUMN-0 text on the
    // degraded path — where the scope line is the entire payload — so the
    // module's "nothing interpolated here can reach column 0" claim has to
    // hold as WRITTEN, not because of who currently happens to call it.
    let joined = |entries: &[String]| entries.iter().map(|e| inline_text(e)).collect::<Vec<_>>().join(", ");
    if !scope.not_attempted.is_empty() {
        line.push_str(&format!(" Not attempted: {}.", joined(&scope.not_attempted)));
    }
    // (#2310 P4c-2b PR #2357 review MUST FIX D) Named so a `"degraded"`
    // run's scope line (its whole payload, when there is nothing else to
    // say) actually names what broke, not just that something did.
    if !scope.errored.is_empty() {
        line.push_str(&format!(" Errored: {}.", joined(&scope.errored)));
    }
    // (#2310 delivery rewrite, rule 1) A heading that fell back to a bare
    // rule id says so here, rather than leaving a reader to wonder why one
    // section is named differently from the rest.
    if !unresolved_rules.is_empty() {
        let names = unresolved_rules.iter().map(|e| inline_text(e)).collect::<Vec<_>>().join(", ");
        line.push_str(&format!(" Titles unavailable for these rules: {names}."));
    }
    // (#2310 fix-loop E2, S1-6) Appended ONCE, unconditionally, last — see
    // `STANDING_NARROWNESS`. Unconditional because it is a property of the
    // mechanism, not of the run: the cleanest possible run is exactly when
    // a reader is most likely to mistake it for a complete review.
    line.push(' ');
    line.push_str(STANDING_NARROWNESS);
    line
}

/// (#2310 P4b review, M-B) A gate-passed mod's change becomes either
/// inline GitHub suggestion(s) or a fenced block in the body — NEVER a
/// suggestion for opaque text. DESIGN.md: "darkmux never opens a kit".
/// Pasting opaque model text verbatim into a ```suggestion block was the
/// bug this function fixes: the common shape is itself a unified diff, so
/// "Commit suggestion" would have replaced the anchored line with raw
/// `+++`/`@@` text, and a multi-line change collapsed to a single-line
/// suggestion (no `start_line`) that duplicated the lines below it.
///
/// Only a mod whose proposer explicitly declared `kit_kind:
/// "unified-diff"` gets parsed — through the SAME shared
/// `crate::diff::parse_diff` this crate and `darkmux-lab`'s bundler both
/// use, never a second parser (see that module's own doc) — and only a
/// HUNK whose OLD range sits ENTIRELY inside the PR diff's own touched
/// lines becomes a suggestion (the OLD side names the lines it replaces in
/// the file's CURRENT state, which is the same coordinate space the PR
/// diff's NEW side already occupies). Every other case — `kit_kind` unset
/// or not `"unified-diff"`, unparseable text, a pure-insertion hunk with
/// no OLD range to anchor a replacement against, or a hunk outside the PR
/// diff — falls back to the fenced body block this branch has always
/// rendered.
fn render_gated_mod(
    m: &GatedMod,
    window: &FindingWindow,
    touched: &BTreeMap<String, BTreeSet<u32>>,
    comments: &mut Vec<GithubReviewComment>,
    bullets: &mut Vec<String>,
) {
    // Map the change out of container coordinates by the source the gate
    // recorded (a no-op for one already in repo coordinates, and for a
    // record with no source). Without this a gate-PASSED change written as
    // `a/<source>/src/x.ts` parses to a path the diff never touched and files
    // as "outside the diff" — how the first passing coder change rendered live.
    let mapped_kit;
    let kit = match (m.record.source.as_deref(), m.record.kit.as_deref()) {
        (Some(source), Some(raw)) if !source.trim().is_empty() => {
            mapped_kit = crate::mods::strip_kit_source_prefix(source, raw);
            mapped_kit.as_str()
        }
        (_, raw) => raw.unwrap_or(""),
    };
    if m.record.kit_kind.as_deref() != Some("unified-diff") {
        bullets.push(fenced_patch_bullet(window, kit, "The change is written out rather than as a patch, so it is quoted here"));
        return;
    }
    let hunks = crate::diff::parse_diff(kit);
    if hunks.is_empty() {
        // Declared a unified diff but nothing parsed. Never guess at
        // intent; quote it as written, same as any other shape.
        bullets.push(fenced_patch_bullet(window, kit, "The change did not read as a patch, so it is quoted here"));
        return;
    }
    let mut suggested = 0usize;
    for (path, file_hunks) in &hunks {
        for h in file_hunks {
            if h.old_block.is_empty() {
                // A pure-insertion hunk has no OLD range to anchor a
                // REPLACEMENT suggestion against — GitHub suggestions
                // replace an existing line range; they cannot insert
                // between two lines with no line of their own.
                bullets.push(fenced_hunk_bullet(
                    window,
                    path,
                    h,
                    "The change adds lines rather than replacing them, so it cannot be attached to a line",
                ));
                continue;
            }
            let old_start = h.old_start;
            let old_end = old_start + h.old_block.len() as u32 - 1;
            let inside = (old_start..=old_end).all(|l| line_touched(touched, path, Some(l)));
            if inside {
                // (#2310 fix loop A, S5-4) The suggestion's payload is
                // model-authored and its block ends on the line after it,
                // so the fence is sized to the payload — GitHub honors a
                // longer ````suggestion fence identically.
                let suggestion = h.new_block.join("\n");
                let fence = fence_for(&suggestion);
                // (#2310 delivery rewrite, rule 1) The claim rides in the
                // comment too: an inline comment is read at the line, with
                // none of the body's context around it, so a bare
                // suggestion block asks the author to guess why.
                let claim = claim(window);
                let body = if claim.is_empty() {
                    format!("{fence}suggestion\n{suggestion}\n{fence}")
                } else {
                    format!("{claim}\n\n{fence}suggestion\n{suggestion}\n{fence}")
                };
                comments.push(GithubReviewComment {
                    path: path.clone(),
                    line: old_end,
                    start_line: if old_start != old_end { Some(old_start) } else { None },
                    side: Some("RIGHT".to_string()),
                    body,
                });
                suggested += 1;
            } else {
                bullets.push(fenced_hunk_bullet(window, path, h, "The change touches lines outside this diff"));
            }
        }
    }
    if suggested > 0 {
        // Last, so a change that produced BOTH a suggestion and a quoted
        // hunk reads in the order the hunks did, with the pointer to the
        // attached suggestion closing the entry.
        bullets.push(suggestion_bullet(window, suggested));
    }
}

/// (#2310 fix loop A, S5-4) The change is opaque model text — the fence is
/// sized to it ([`fence_for`]) so it cannot close its own block and
/// continue in darkmux's voice.
fn fenced_patch_bullet(window: &FindingWindow, kit: &str, reason: &str) -> String {
    let fence = fence_for(kit);
    format!("- {} — {} {reason}:\n\n{fence}\n{kit}\n{fence}", window.span(), claim_sentence(window))
}

/// (#2310 fix loop A, S5-4) Both model-authored strings here are
/// contained: `path` goes through [`code_span`] (the change names it, so
/// it is no more trusted) and the hunk body through a [`fence_for`]-sized
/// fence.
fn fenced_hunk_bullet(window: &FindingWindow, path: &str, h: &crate::diff::Hunk, reason: &str) -> String {
    let old_len = h.old_block.len() as u32;
    let path_span = code_span(path);
    let span = if old_len == 0 {
        format!("after line {} of {path_span}", h.old_start)
    } else {
        format!("at lines {}–{} of {path_span}", h.old_start, h.old_start + old_len - 1)
    };
    let body = h.new_block.join("\n");
    let fence = fence_for(&body);
    format!("- {} — {} {reason}, {span}:\n\n{fence}\n{body}\n{fence}", window.span(), claim_sentence(window))
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

    /// The window's human-readable content — the LOCATION, `file:line`.
    /// `file` is MODEL-authored (it comes out of the finding's own
    /// emission), so every caller renders this through [`Self::span`]
    /// rather than pasting it between two literal backticks (#2310 fix
    /// loop A, S5-4).
    ///
    /// (#2310 delivery rewrite) The finding KEY used to lead this string.
    /// It is darkmux's own record id, meaningless to the author reading
    /// the review, so it renders only when there is no location at all —
    /// where it is the one identifying thing left.
    fn display(&self) -> String {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{f}:{l}"),
            (Some(f), None) => f.clone(),
            _ => self.key.clone(),
        }
    }

    /// [`Self::display`] as a code span whose delimiter is longer than any
    /// backtick run inside it — the form every bullet in this module uses.
    fn span(&self) -> String {
        code_span(&self.display())
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
    /// (#2394) [`SeatClaim::NoModel`] — this kind renders and posts a GitHub review from records already gathered; it
    /// dispatches nothing. Bounded by `runtime.dispatch_free_concurrency`
    /// and, per command, by `runtime.step_command_timeout_seconds` — never
    /// by the hosted-endpoint cap.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

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
        // (#2310 delivery rewrite) Titles resolve HERE, at the one impure
        // edge, so `render_github_review` stays a pure function over its
        // inputs and every golden test states the titles it renders under.
        let titles = rule_titles();
        let outcome = render_github_review(
            &cfg.findings,
            &cfg.mods,
            &cfg.diff,
            &cfg.scope,
            cfg.attribution.as_deref(),
            &titles,
        );
        let payload = serde_json::to_string(&outcome).context("serializing the deliver outcome")?;
        match cfg.emit.as_deref() {
            Some(p) if p == std::path::Path::new("-") => println!("{payload}"),
            Some(p) => std::fs::write(p, &payload).with_context(|| format!("writing {}", p.display()))?,
            None => println!("{payload}"),
        }
        let dest = cfg.emit.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".to_string());
        // (#2310 fix-loop E2, from the C2 post-merge review) The step's
        // output is a JSON OBJECT, not the emit path.
        //
        // `review-v2.json` declares `outcome_from: "deliver"`, which
        // promotes this step's output into the run's `mission close`
        // payload — and a bare path is not a JSON object, so
        // `promoted_step_body` promoted NOTHING. A run whose whole point is
        // to deliver a verdict closed with a null payload; the one fact
        // every downstream reader wants (did this review say anything, and
        // what) was reachable only by opening the emit file. `mode` is the
        // verdict (`review`/`degraded`/`noop`), `summary` the same scope
        // line the payload's own body leads with, and `emit` the
        // destination the previous contract carried — nothing is lost, the
        // shape just became promotable.
        //
        // Deliberately NOT the whole `DeliverOutcome`: the review body is
        // model-authored text that would then be copied verbatim into a
        // flow record, and the close payload is a summary surface, not a
        // second copy of the artifact.
        let summary = scope_line(&cfg.scope, cfg.findings.len(), &unresolved_rule_titles(&cfg.findings, &titles));
        let output = serde_json::to_string(&serde_json::json!({
            "mode": outcome.mode,
            "summary": summary,
            "emit": dest,
        }))
        .context("serializing the deliver step output")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
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

    /// Rule titles for the fixtures below — the real strings from
    /// `templates/builtin/rules/*.json`, so a golden reads exactly the way
    /// a live review does.
    fn test_titles() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "unnamed-predicate".to_string(),
                "A compound condition encodes a domain rule that has no name and cannot be tested on its own".to_string(),
            ),
            (
                "union-vs-enum".to_string(),
                "A new string-literal union or enum-like set may duplicate an existing one".to_string(),
            ),
            ("existing-solution".to_string(), "A new routine looks re-implemented rather than reused".to_string()),
            (
                "swallowed-error".to_string(),
                "A failure is caught or discarded and nothing records that it happened".to_string(),
            ),
            (
                "shared-symbol-callers".to_string(),
                "A shared function or type's signature or behavior changed".to_string(),
            ),
        ])
    }

    /// [`render_github_review`] against [`test_titles`] — every test below
    /// that does not care which titles resolved goes through this.
    fn render(
        findings: &[FindingRecord],
        mods: &[GatedMod],
        diff: &str,
        scope: &DeliverScope,
        attribution: Option<&str>,
    ) -> DeliverOutcome {
        render_github_review(findings, mods, diff, scope, attribution, &test_titles())
    }

    fn finding(key: &str, file: &str, line: u32, evidence: &str, why: &str, form: Option<&str>) -> FindingRecord {
        finding_of_rule(key, None, file, line, evidence, why, form)
    }

    /// A finding that names its rule — `context.rule`, the key
    /// `crawl::unit_step::run` host-stamps on every real finding.
    #[allow(clippy::too_many_arguments)]
    fn finding_of_rule(
        key: &str,
        rule: Option<&str>,
        file: &str,
        line: u32,
        evidence: &str,
        why: &str,
        form: Option<&str>,
    ) -> FindingRecord {
        let mut context = json!({});
        if let Some(f) = form {
            // (#2310 P4c-2b fix) `confirm`, not `form` — see `delivery_form`'s own doc.
            context = json!({ "confirm": f });
        }
        if let Some(r) = rule {
            context["rule"] = json!(r);
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
            source: None,
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
                source: None,
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
        let out = render(&findings, &mods, DIFF, &scope, None);
        assert_eq!(out.mode, "review");
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_eq!(review.comments[0].path, "src/a.ts");
        assert_eq!(review.comments[0].line, 2, "the hunk's old-range END line");
        assert_eq!(review.comments[0].start_line, None, "a single-line range carries no start_line");
        assert_eq!(review.comments[0].side.as_deref(), Some("RIGHT"));
        // (#2310 delivery rewrite) The claim leads the comment — an inline
        // comment is read at the line, with none of the body around it.
        assert!(
            review.comments[0].body.starts_with("reimplements a helper\n\n```suggestion\n"),
            "{}",
            review.comments[0].body
        );
        assert!(review.comments[0].body.contains("clamp(1)"));
    }

    /// (#2310 fix-loop B2, S3-3) `render_gated_mod`'s anchor math is
    /// `old_end = old_start + old_block.len() - 1` and its body is
    /// `new_block.join("\n")` — BOTH are wrong by a whole line if the
    /// parser silently drops a bare blank line inside the kit's hunk.
    /// Concretely: the suggestion would anchor lines 2–3 while carrying
    /// the replacement for 2–4 and would have LOST the blank line, so
    /// "Commit suggestion" deletes a real line of the operator's file.
    /// This pins the whole span and the body.
    /// (live proof pass 4, 2026-09-05) A create-mod dispatch writes its kit in
    /// container coordinates (`a/app/src/a.ts`); the gate maps that to apply
    /// it and records the source id it resolved. The deliverer must map the
    /// same way before parsing, or a gate-PASSED kit files as "outside the
    /// diff" under a path the diff never touched — which is exactly how the
    /// first passing coder kit rendered live.
    #[test]
    fn a_gated_mod_in_container_coordinates_with_a_recorded_source_still_becomes_a_suggestion() {
        const KIT: &str =
            "--- a/app/src/a.ts\n+++ b/app/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        let findings = vec![finding("s/1", "src/a.ts", 2, "const x = 1;", "reimplements a helper", None)];
        let mut m = gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true));
        m.record.source = Some("app".to_string());
        let scope = DeliverScope { rules_run: vec!["r1".into()], hunks_covered: 1, hunks_total: 1, ..Default::default() };
        let review = render(&findings, &[m], DIFF, &scope, None).review.unwrap();
        assert_eq!(review.comments.len(), 1, "the mapped kit sits inside the diff: {review:?}");
        assert_eq!(review.comments[0].path, "src/a.ts", "repo coordinates, not the mount's");
        assert!(review.comments[0].body.contains("clamp(1)"));
    }

    #[test]
    fn a_kit_with_a_bare_blank_line_anchors_and_replaces_the_full_span() {
        // The PR diff touches lines 1–4; its own blank context line is
        // space-prefixed, the shape git actually emits.
        const PR_DIFF: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,4 +1,4 @@\n function f() {\n-  const x = 1;\n+  const y = 1;\n \n }\n";
        // The KIT's blank line is BARE — the shape that survives a
        // trailing-whitespace stripper anywhere on the path to here.
        const KIT: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,3 +2,3 @@\n-  const y = 1;\n+  const y = clamp(1);\n\n }\n";
        let findings = vec![finding("s/1", "src/a.ts", 2, "const y = 1;", "reimplements a helper", None)];
        let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
        let scope = DeliverScope { rules_run: vec!["r1".into()], hunks_covered: 1, hunks_total: 1, ..Default::default() };
        let review = render(&findings, &mods, PR_DIFF, &scope, None).review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_eq!(review.comments[0].start_line, Some(2), "the hunk's old range STARTS at 2");
        assert_eq!(review.comments[0].line, 4, "…and ENDS at 4 — the blank line is one of the three replaced lines");
        assert_eq!(
            review.comments[0].body, "reimplements a helper\n\n```suggestion\n  const y = clamp(1);\n\n}\n```",
            "the replacement keeps the blank line it replaces"
        );
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
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
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
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
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
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
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
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "never-gated must not become a suggestion: {review:?}");
        assert!(review.body.contains("Worth a double check"));
        // (#2310 delivery rewrite) The entry is headed by the LOCATION —
        // the finding key is darkmux's own record id, meaningless to an
        // author reading the review.
        assert!(review.body.contains("`src/a.ts:2`"), "{}", review.body);
        assert!(!review.body.contains("s/9"), "the record key never renders: {}", review.body);
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
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "the gate-passed mod renders, not a double-check: {review:?}");
        assert!(!review.body.contains("an earlier failed attempt"));
    }

    #[test]
    fn a_gated_mod_outside_the_diff_becomes_a_body_patch_not_a_suggestion() {
        let findings = vec![finding("s/2", "src/other.ts", 5, "irrelevant", "unrelated", None)];
        let mods = vec![gated_mod("s/2", "the patch text", Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "not an inline suggestion: {review:?}");
        assert!(review.body.contains("the patch text"));
        assert!(!review.body.contains("```suggestion"));
    }

    #[test]
    fn a_finding_with_no_mod_becomes_a_worth_a_double_check_thread() {
        let findings = vec![finding("s/3", "src/a.ts", 2, "the cited line", "might duplicate an existing helper", None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty());
        assert!(review.body.contains("Worth a double check"));
        assert!(review.body.contains("`src/a.ts:2`"), "{}", review.body);
        assert!(!review.body.contains("s/3"), "the record key never renders: {}", review.body);
        assert!(review.body.contains("might duplicate an existing helper"));
    }

    #[test]
    fn a_gate_failed_mod_becomes_a_worth_a_double_check_thread_not_a_suggestion() {
        let findings = vec![finding("s/4", "src/a.ts", 2, "ev", "claim", None)];
        let mods = vec![gated_mod("s/4", "kit text", Some(false))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty());
        assert!(review.body.contains("Worth a double check"));
        assert!(!review.body.contains("kit text"));
    }

    #[test]
    fn a_searched_rule_with_no_change_is_a_lead_never_a_headline() {
        // (#2310 delivery rewrite, rule 3) A rule confirmed by searching,
        // with nobody proposing a change, cannot be told apart —
        // mechanically — from one whose search came back all-clear
        // (`create_finding` has no severity/holds field to read). So it is
        // a thread to pull, under the tail section, never a headline that
        // reads as a defect. Its own rule still heads it inside there.
        let findings = vec![finding_of_rule(
            "s/5",
            Some("shared-symbol-callers"),
            "src/mw.ts",
            10,
            "14 endpoints use this middleware",
            "shared auth changed",
            Some("search"),
        )];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        let body = review.body;
        let tail = body.find("Worth a double check").unwrap_or_else(|| panic!("no tail section:\n{body}"));
        let entry = body.find("`src/mw.ts:10`").unwrap_or_else(|| panic!("no entry:\n{body}"));
        assert!(entry > tail, "a searched rule with no change must sit UNDER the tail section:\n{body}");
        assert!(
            body[tail..].contains("_A shared function or type's signature or behavior changed_ `shared-symbol-callers`"),
            "the rule still heads it inside the tail:\n{body}"
        );
        assert!(body.contains("14 endpoints use this middleware"), "the evidence is never dropped:\n{body}");
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
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.body.contains("Status enum, Kind enum"));
        assert!(review.body.contains("did you check whether the repo already has this"));
    }

    #[test]
    fn refused_findings_never_render_only_the_scope_count_shows() {
        let scope = DeliverScope { refused: 3, rules_run: vec!["r1".into()], ..Default::default() };
        let findings = vec![finding("s/7", "src/a.ts", 2, "ev", "claim", None)];
        let out = render(&findings, &[], DIFF, &scope, None);
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
        let line = scope_line(&scope, 4, &BTreeSet::new());
        // (#2310 fix-loop E2) The denominator here comes from the fallback:
        // no `rules_total`, so 2 run + 1 not-attempted = 3 declared.
        assert!(line.contains("2 of 3 rules reviewed"), "{line}");
        assert!(line.contains("2/5 hunks"));
        assert!(line.contains("4 finding(s)"));
        assert!(line.contains("Not attempted: architectural review."));
    }

    #[test]
    fn nothing_to_say_is_a_noop_not_an_empty_review() {
        let out = render(&[], &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.mode, "noop");
        assert!(out.review.is_none());
    }

    /// (#2310 P4c-2b PR #2357 round-2 review item 4) A UNIT-level pin on
    /// `render_github_review` itself — before this test, only the two
    /// real-launch CLI tests exercised the `noop`/`degraded` branch,
    /// neither of which calls this function directly. Zero findings/mods
    /// but a non-empty `scope.errored` must render `mode: "degraded"`
    /// with a review body carrying the scope line (never `"noop"`, never
    /// `review: None`).
    #[test]
    fn an_errored_scope_with_nothing_to_say_is_degraded_not_noop() {
        let scope = DeliverScope { errored: vec!["unit `x` (Error)".to_string()], ..Default::default() };
        let out = render(&[], &[], DIFF, &scope, None);
        assert_eq!(out.mode, "degraded", "{out:?}");
        let review = out.review.expect("a degraded outcome still carries a review payload (the scope line)");
        assert!(review.comments.is_empty(), "{review:?}");
        assert!(review.body.contains("review ran:"), "the scope line must be present: {}", review.body);
        // (#2310 fix loop E1-2) The entry now goes through `inline_text`,
        // so its backticks become the look-alike. That is a deliberate
        // cost: `DeliverScope::errored` has no production producer yet
        // (grep — it is filled only by this module's own tests), so no
        // shipped text loses its code styling, while the containment
        // claim becomes true as written for whoever fills it first.
        assert!(review.body.contains("Errored: unit \u{02cb}x\u{02cb} (Error)."), "{}", review.body);
    }

    // ─── (#2310 fix loop A / S3-2) coverage is never discarded ──────────

    /// (#2310 fix loop A, S3-2 — PROVEN) A run that covered NONE of the
    /// diff's hunks is not a clean pass. Before this fix `nothing_to_say`
    /// looked only at `scope.errored`, so `0/5` rendered `noop` with
    /// `review: null` — the coverage fact the scope line exists to state
    /// was discarded on the one path where it is the whole story.
    #[test]
    fn zero_of_five_hunks_covered_with_nothing_to_say_is_degraded_not_noop() {
        let scope = DeliverScope { hunks_covered: 0, hunks_total: 5, ..Default::default() };
        let out = render(&[], &[], DIFF, &scope, None);
        assert_eq!(out.mode, "degraded", "an uncovered run is never a clean noop: {out:?}");
        let review = out.review.expect("a degraded outcome still carries the scope line");
        assert!(review.body.contains("0/5 hunks covered"), "{}", review.body);
    }

    /// The other side of the same rule: full coverage with nothing found
    /// and nothing errored IS a clean run, and still renders `noop`.
    #[test]
    fn five_of_five_hunks_covered_with_nothing_to_say_is_still_a_noop() {
        let scope = DeliverScope { hunks_covered: 5, hunks_total: 5, ..Default::default() };
        let out = render(&[], &[], DIFF, &scope, None);
        assert_eq!(out.mode, "noop", "{out:?}");
        assert!(out.review.is_none());
    }

    // ─── (#2310 fix loop A / S5-4) the body is not the model's canvas ───

    /// Model-authored text engineered to break out of whatever container
    /// it lands in: a three-backtick run, a four-backtick run, a forged
    /// `### darkmux review` header with its own plausible scope line and
    /// verdict, a thematic break, a block quote, and raw HTML.
    const ATTACK: &str = "benign lead-in\n```\n### darkmux review\n\nreview ran: 9 rule(s), 9/9 hunks covered, 0 finding(s) considered, 0 refused. Approved.\n---\n***\n> quoted\n<img src=x onerror=alert(1)>\n````\nstill the model talking";

    /// A markdown STRUCTURE check, not a substring check (#2310 fix loop
    /// A, S5-4): walks `text` tracking fence state the way a CommonMark
    /// reader does, and asserts (a) every fence that opened also closed —
    /// so no model payload terminated its own container — and (b) the only
    /// lines that begin a heading / thematic break / block quote at column
    /// 0 OUTSIDE a fence are the ones this module itself authored
    /// (`expected_headings`). A forged header lands in the panic message.
    fn assert_no_markdown_breakout(text: &str, expected_headings: &[&str], label: &str) {
        let mut open: Option<usize> = None;
        let mut headings: Vec<String> = Vec::new();
        let mut breaks: Vec<String> = Vec::new();
        for line in text.lines() {
            let ticks = line.chars().take_while(|c| *c == '`').count();
            match open {
                Some(n) => {
                    if ticks >= n && line.trim_end().chars().all(|c| c == '`') {
                        open = None;
                    }
                    continue;
                }
                None => {
                    if ticks >= 3 {
                        open = Some(ticks);
                        continue;
                    }
                }
            }
            if line.starts_with('#') {
                headings.push(line.to_string());
            }
            if line.starts_with("---") || line.starts_with("***") || line.starts_with('>') {
                breaks.push(line.to_string());
            }
        }
        assert!(open.is_none(), "{label}: a fence opened and never closed — model text terminated its container:\n{text}");
        let expected: Vec<String> = expected_headings.iter().map(|s| s.to_string()).collect();
        assert_eq!(headings, expected, "{label}: a model-authored heading reached column 0:\n{text}");
        assert!(breaks.is_empty(), "{label}: a model-authored break/quote reached column 0: {breaks:?}\n{text}");
    }

    /// The invariant for every ONE-LINE bullet a model string lands in
    /// (#2310 fix loop A, S5-4 (c)): model text never introduces a line
    /// break, so it can never reach column 0 and can never open a fence —
    /// EVERY line carrying the payload's start must also carry its end.
    /// A structural fence walk alone is fooled here: a payload that opens
    /// AND closes its own fence looks balanced while still having taken
    /// over the body.
    fn assert_folded_onto_one_line(text: &str, label: &str) {
        let carrying: Vec<&str> = text.lines().filter(|l| l.contains("benign lead-in")).collect();
        assert!(!carrying.is_empty(), "{label}: the model text vanished entirely:\n{text}");
        for line in carrying {
            assert!(
                line.contains("still the model talking"),
                "{label}: model text spans multiple lines — it reaches column 0 and can start a heading, \
                 a thematic break, or a fence:\n{text}"
            );
        }
    }

    #[test]
    fn an_attack_payload_in_why_cannot_forge_a_header_in_the_double_check_bullet() {
        let findings = vec![finding("s/1", "src/a.ts", 2, "ev", ATTACK, None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_folded_onto_one_line(&review.body, "double-check bullet `why`");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "double-check bullet `why`");
        assert!(review.body.contains("Approved."), "the claim is folded into the bullet, never dropped: {}", review.body);
    }

    #[test]
    fn an_attack_payload_in_evidence_cannot_forge_a_header_in_the_search_bullet() {
        let findings = vec![finding("s/1", "src/a.ts", 2, ATTACK, "why", Some("search"))];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_folded_onto_one_line(&review.body, "search bullet `evidence`");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "search bullet `evidence`");
    }

    #[test]
    fn an_attack_payload_in_a_question_bullet_cannot_forge_a_header() {
        let findings = vec![finding("s/1", "src/a.ts", 2, ATTACK, ATTACK, Some("question"))];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_folded_onto_one_line(&review.body, "question bullet");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "question bullet");
    }

    /// Reads the FIRST code span off a bullet line the way a CommonMark
    /// reader does — opening delimiter = the leading backtick run, closing
    /// = the next run of exactly that length — and returns what it
    /// actually encloses. Independent of [`code_span`]'s own arithmetic,
    /// so a test asserting on it is not tautological: given a delimiter
    /// too short for its content, the span closes early and this returns
    /// a truncated string.
    fn first_code_span(line: &str) -> String {
        let rest = line.strip_prefix("- ").unwrap_or_else(|| panic!("not a bullet: {line}"));
        let delim_len = rest.chars().take_while(|c| *c == '`').count();
        assert!(delim_len >= 1, "the bullet does not open with a code span: {line}");
        let body: Vec<char> = rest.chars().skip(delim_len).collect();
        let mut i = 0usize;
        while i < body.len() {
            if body[i] == '`' {
                let run = body[i..].iter().take_while(|c| **c == '`').count();
                if run == delim_len {
                    return body[..i].iter().collect::<String>().trim().to_string();
                }
                i += run;
            } else {
                i += 1;
            }
        }
        panic!("the code span opened and never closed: {line}");
    }

    #[test]
    fn a_model_supplied_file_path_cannot_break_out_of_its_code_span() {
        // The `file` field is model-authored and lands inside a backtick
        // span via `FindingWindow::display` — both its backticks (which
        // would close a one-backtick delimiter early) and its line breaks
        // (which would reach column 0) must be contained.
        const FILE: &str = "src/`a`.ts\n### darkmux review\n---";
        let findings = vec![finding("s/1", FILE, 2, "ev", "why", None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "FindingWindow::display `file`");
        let bullet = review
            .body
            .lines()
            .find(|l| l.starts_with("- "))
            .unwrap_or_else(|| panic!("no bullet rendered:\n{}", review.body));
        let span = first_code_span(bullet);
        assert!(
            span.contains("src/`a`.ts") && span.contains("### darkmux review") && span.ends_with(":2"),
            "the whole window must sit INSIDE the code span — a delimiter no longer than the content's own \
             backtick run closes it early and hands the rest of the path to markdown.\nspan: {span}\nbullet: {bullet}"
        );
    }

    #[test]
    fn an_attack_payload_in_an_opaque_kit_cannot_terminate_its_fence() {
        let findings = vec![finding("s/1", "src/a.ts", 2, "ev", "why", None)];
        let mods = vec![gated_mod("s/1", ATTACK, Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "fenced_patch_bullet kit");
    }

    #[test]
    fn an_attack_payload_in_an_out_of_diff_hunk_cannot_terminate_its_fence() {
        // A parseable unified-diff kit whose hunk sits outside the PR
        // diff — its new-side lines reach `fenced_hunk_bullet`, and its
        // `path` reaches a backtick span in the same bullet.
        let kit = format!(
            "diff --git a/src/`a`.ts b/src/`a`.ts\n--- a/src/`a`.ts\n+++ b/src/`a`.ts\n@@ -99,1 +99,1 @@\n-old\n{}\n",
            ATTACK.lines().map(|l| format!("+{l}")).collect::<Vec<_>>().join("\n")
        );
        let findings = vec![finding("s/1", "src/a.ts", 99, "ev", "why", None)];
        let mods = vec![gated_mod_kind("s/1", &kit, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "{review:?}");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "fenced_hunk_bullet");
    }

    #[test]
    fn an_attack_payload_in_a_suggestion_body_cannot_terminate_its_block() {
        // An in-diff hunk whose new-side lines carry the attack payload —
        // the ```suggestion block is the one container that ends on a
        // model-adjacent line, and a second one-click suggestion forged
        // inside it would be indistinguishable from darkmux's own.
        let kit = format!(
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n{}\n",
            ATTACK.lines().map(|l| format!("+{l}")).collect::<Vec<_>>().join("\n")
        );
        let findings = vec![finding("s/1", "src/a.ts", 2, "ev", "why", None)];
        let mods = vec![gated_mod_kind("s/1", &kit, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_no_markdown_breakout(&review.comments[0].body, &[], "suggestion block");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "body alongside the suggestion");
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
        // (#2310 fix-loop E2, from the C2 post-merge review) The step's own
        // output is a promotable JSON object — `{mode, summary, emit}` —
        // NOT the bare emit path it used to be. `review-v2.json` declares
        // `outcome_from: "deliver"`, and a bare path promoted nothing, so
        // the run closed with a null payload. The emit destination is still
        // carried, as a field.
        let step_output: serde_json::Value = serde_json::from_str(&outcome.output)
            .expect("the deliver step's output must be a JSON object so `outcome_from` can promote it");
        assert_eq!(step_output["emit"], json!(out_path.to_string_lossy()));
        assert_eq!(step_output["mode"], json!("noop"), "the verdict rides the output: {step_output}");
        assert!(
            step_output["summary"].as_str().unwrap().contains("rules reviewed"),
            "the scope line rides it too: {step_output}"
        );
        let written = std::fs::read_to_string(&out_path).unwrap();
        let parsed: DeliverOutcome = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed.mode, "noop", "zero findings, zero mods: nothing to say");
    }

    /// (#2310 fix-loop E2, from the C2 post-merge review) The verdict the
    /// step output carries is the RUN's, not a constant: a degraded run
    /// promotes `"degraded"`. Without this, hardcoding `"noop"` in `run`
    /// would leave the test above green.
    #[test]
    fn the_step_outputs_verdict_is_the_runs_own_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let out_path = dir.path().join("out.json");
        let mut step = Step {
            id: "deliver-step".into(),
            task_id: "deliver-task".into(),
            kind: DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: crate::types::NodeStatus::Planned,
            config: json!({}),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        step.config = json!({
            "findings": [],
            "mods": [],
            "diff": DIFF,
            "scope": { "errored": ["unit `u-1` (Error)"] },
            "emit": out_path.to_string_lossy(),
        });
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
        let step_output: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(step_output["mode"], json!("degraded"), "{step_output}");
        assert!(step_output["summary"].as_str().unwrap().contains("Errored:"), "{step_output}");
    }

    /// (#2310 fix-loop E2) The scope line names the DENOMINATOR: "2 of 7
    /// rules reviewed", never a bare "2 rule(s)" that reads as the whole
    /// review. Loop D made the numerator honest; this is the sentence that
    /// makes the number mean something.
    #[test]
    fn the_scope_line_states_how_many_rules_of_how_many_were_reviewed() {
        let scope = DeliverScope {
            rules_run: vec!["a".into(), "b".into()],
            rules_total: 7,
            hunks_covered: 3,
            hunks_total: 12,
            ..Default::default()
        };
        let line = scope_line(&scope, 4, &BTreeSet::new());
        assert!(line.contains("2 of 7 rules reviewed"), "{line}");
        assert!(line.contains("3/12 hunks covered"), "{line}");
    }

    /// The denominator falls back to what the lists can PROVE when a
    /// producer sets no `rules_total` — and never drops below the number
    /// that ran, which would render the nonsense "3 of 2".
    #[test]
    fn the_rules_denominator_falls_back_and_never_undercounts() {
        let no_total = DeliverScope {
            rules_run: vec!["a".into(), "b".into()],
            not_attempted: vec!["c".into()],
            ..Default::default()
        };
        assert_eq!(no_total.rules_declared(), 3, "run + not-attempted is what the lists prove");

        let understated =
            DeliverScope { rules_run: vec!["a".into(), "b".into(), "c".into()], rules_total: 2, ..Default::default() };
        assert_eq!(understated.rules_declared(), 3, "M is never below N");
    }

    /// (#2310 fix-loop E2, S1-6) Every scope line states the standing
    /// narrowness of the mechanism — ONCE, and on the cleanest run too,
    /// which is exactly when a reader is most likely to mistake a
    /// rule-shaped review for a complete one.
    #[test]
    fn the_scope_line_states_the_standing_narrowness_exactly_once() {
        for scope in [
            DeliverScope { rules_run: vec!["a".into()], rules_total: 1, hunks_covered: 1, hunks_total: 1, ..Default::default() },
            DeliverScope { errored: vec!["unit `u-1` (Error)".into()], ..Default::default() },
            DeliverScope::default(),
        ] {
            let line = scope_line(&scope, 0, &BTreeSet::new());
            assert_eq!(
                line.matches("rule-shaped review").count(),
                1,
                "stated once, never twice and never omitted: {line}"
            );
            assert!(line.contains("architectural breadth stays with the frontier gate"), "{line}");
        }
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
    /// The fixture every delivery shape is read off — shared by the
    /// golden and by the vocabulary conformance test below so the two can
    /// never drift apart.
    fn every_form_fixture() -> (Vec<FindingRecord>, Vec<GatedMod>, DeliverScope) {
        const CLAMP_KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        // (#2310 delivery rewrite, rule 2) A gate-passed change on a rule
        // that is confirmed by SEARCHING — the shape that was dropped live.
        const UNION_KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,1 @@\n-function f() {\n+function f(): Status {\n";
        let findings = vec![
            finding_of_rule("sess-a/1", Some("swallowed-error"), "src/a.ts", 2, "const x = 1;", "reimplements clamp()", None),
            finding_of_rule("sess-a/2", Some("unnamed-predicate"), "src/other.ts", 5, "irrelevant", "unrelated change", None),
            finding_of_rule(
                "sess-a/3",
                Some("unnamed-predicate"),
                "src/a.ts",
                2,
                "the cited line",
                "might duplicate an existing helper",
                None,
            ),
            finding_of_rule("sess-a/4", Some("union-vs-enum"), "src/a.ts", 2, "ev", "a gate-failed claim", Some("search")),
            finding_of_rule(
                "sess-a/5",
                Some("shared-symbol-callers"),
                "src/mw.ts",
                10,
                "14 endpoints use this middleware",
                "shared auth changed",
                Some("search"),
            ),
            finding_of_rule(
                "sess-a/6",
                Some("existing-solution"),
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
            finding_of_rule(
                "sess-a/7",
                Some("swallowed-error"),
                "src/a.ts",
                2,
                "ev",
                "a mod exists but nothing ever gated it",
                None,
            ),
            finding_of_rule(
                "sess-a/8",
                Some("union-vs-enum"),
                "src/a.ts",
                1,
                "function f() {",
                "this union duplicates the Status enum",
                Some("search"),
            ),
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
            gated_mod_kind("sess-a/8", UNION_KIT, Some("unified-diff"), Some(true)),
        ];
        let scope = DeliverScope {
            rules_run: vec!["unnamed-predicate".into(), "existing-solution".into()],
            rules_total: 3,
            hunks_covered: 3,
            hunks_total: 4,
            refused: 2,
            not_attempted: vec!["architectural review".into()],
            errored: Vec::new(),
        };
        (findings, mods, scope)
    }

    /// (#2310 delivery rewrite, rule 1) Entries are grouped and headed by
    /// the RULE — its title, tagged with its id — never by how the finding
    /// was confirmed. The three procedure-shaped section headings this
    /// module used to print are gone as organizers.
    #[test]
    fn entries_are_headed_by_the_rules_title_and_tagged_with_its_id() {
        let findings = vec![
            finding_of_rule(
                "s/1",
                Some("existing-solution"),
                "src/x.ts",
                1,
                "Status enum",
                "did you check whether the repo already has this",
                Some("question"),
            ),
            finding_of_rule("s/2", Some("unnamed-predicate"), "src/a.ts", 2, "ev", "the condition has no name", None),
        ];
        let body = render(&findings, &[], DIFF, &DeliverScope::default(), None).review.unwrap().body;
        assert!(
            body.contains("**A new routine looks re-implemented rather than reused** `existing-solution`"),
            "the rule's title heads its group, tagged with the id:\n{body}"
        );
        assert!(
            body.contains("cannot be tested on its own_ `unnamed-predicate`"),
            "a rule inside the tail section is headed the same way:\n{body}"
        );
        for gone in ["Worth enumerating", "**Questions:**", "Proposed changes outside the diff"] {
            assert!(!body.contains(gone), "{gone:?} must not organize the review any more:\n{body}");
        }
    }

    /// (#2310 delivery rewrite, rule 2) A gate-passed change renders as a
    /// change whatever the rule's confirmation shape — a passed patch on a
    /// SEARCH-confirmed rule was dropped entirely by the old form-first
    /// branch, live.
    #[test]
    fn a_gate_passed_change_renders_whatever_shape_its_rule_is_confirmed_by() {
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x: Status = 1;\n";
        let findings = vec![finding_of_rule(
            "s/1",
            Some("union-vs-enum"),
            "src/a.ts",
            2,
            "const x = 1;",
            "this union duplicates the Status enum",
            Some("search"),
        )];
        let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "the passed change must ride out as a suggestion: {review:?}");
        assert!(review.comments[0].body.contains("const x: Status = 1;"), "{}", review.comments[0].body);
        let body = review.body;
        let head = body
            .find("**A new string-literal union or enum-like set may duplicate an existing one** `union-vs-enum`")
            .unwrap_or_else(|| panic!("the rule heads a HEADLINE group:\n{body}"));
        let entry = body.find("`src/a.ts:2`").unwrap_or_else(|| panic!("no entry:\n{body}"));
        assert!(entry > head, "the entry sits under its rule's headline group:\n{body}");
        assert!(!body.contains("Worth a double check"), "a passed change is never demoted to a lead:\n{body}");
    }

    /// (#2310 delivery rewrite, rule 1) A rule whose title cannot be
    /// resolved falls back to its own id, and the scope line says so
    /// rather than leaving one oddly-named section unexplained.
    #[test]
    fn an_unresolvable_rule_heads_its_group_by_id_and_the_scope_line_says_so() {
        let findings =
            vec![finding_of_rule("s/1", Some("a-rule-nobody-shipped"), "src/a.ts", 2, "ev", "a claim", None)];
        let body = render(&findings, &[], DIFF, &DeliverScope::default(), None).review.unwrap().body;
        assert!(body.contains("_a-rule-nobody-shipped_"), "the id is the fallback heading:\n{body}");
        assert!(
            body.contains("Titles unavailable for these rules: a-rule-nobody-shipped."),
            "the scope line names it:\n{body}"
        );
    }

    /// (#2310 delivery rewrite, rule 4) The rendered review — body AND
    /// inline comments — speaks the AUTHOR's language. darkmux's own
    /// procedure words never appear; the rule ID does, because it is the
    /// author's handle for re-running the same check after a fix.
    #[test]
    fn the_rendered_review_carries_no_internal_procedure_vocabulary() {
        let (findings, mods, scope) = every_form_fixture();
        let review = render(&findings, &mods, DIFF, &scope, Some("Advisory, not a merge gate.")).review.unwrap();
        let mut text = review.body.clone();
        for c in &review.comments {
            text.push('\n');
            text.push_str(&c.body);
        }
        let text = text.to_lowercase();
        for word in ["search-confirmed", "confirm", "kit", "mod-form", "question-form", "dispatch", "unit"] {
            assert!(!text.contains(word), "the review speaks darkmux's own procedure word {word:?}:\n{text}");
        }
        assert!(review.body.contains("`union-vs-enum`"), "the rule id IS author-facing — it names what was violated");
        assert!(review.body.contains("rules reviewed"), "the scope line's own author-meaningful counts stay");
        assert!(review.body.contains("hunks covered"));
    }

    #[test]
    fn golden_rendered_payload_covers_every_delivery_form() {
        let (findings, mods, scope) = every_form_fixture();

        let outcome = render(&findings, &mods, DIFF, &scope, Some("Advisory, not a merge gate."));
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
        assert_eq!(review.comments.len(), 2, "two in-diff suggestions — one on a mod-confirmed rule, one on a searched rule");
        assert!(review.comments.iter().all(|c| c.side.as_deref() == Some("RIGHT")));
        // (#2310 delivery rewrite, rule 2) The searched rule's passed
        // change rides out as a suggestion, and heads its own group in the
        // body — the shape the old form-first branch dropped entirely.
        assert!(review.comments.iter().any(|c| c.body.contains("function f(): Status {")), "{review:?}");
        assert!(
            review.body.contains("**A new string-literal union or enum-like set may duplicate an existing one** `union-vs-enum`"),
            "{}",
            review.body
        );
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
        // (#2310 fix-loop E2) The destination is now a FIELD on the step's
        // promotable output object, not the whole output.
        let step_output: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(step_output["emit"], json!("-"), "the step's own output names stdout, not a path");
    }

    // ---------------------------------------------------------------
    // (#2310 fix loop E1) The containment primitives' own probe tables.
    // The #2365 review ran these by hand against a scratch binary; the
    // operator's rule is that a reviewer probe becomes a NAMED COMMITTED
    // test, so every row below is one of that review's probes with its
    // expectation recomputed here rather than transcribed.
    // ---------------------------------------------------------------

    /// (#2310 fix loop E1-1) A `<` in model prose must not be able to open
    /// an HTML tag. GitHub's markdown renders raw inline HTML, and it
    /// allows `<img src=…>` — so an unescaped `<` in a `why` lets a model
    /// (or a repo whose content a model quotes) fetch an attacker-chosen
    /// URL from every reader of a review posted under darkmux's byline,
    /// and render an attacker-chosen image/link there. The double-check
    /// path this exercises emits NO fenced block, so the assertion can be
    /// the strongest available: not one `<` survives anywhere in the body.
    #[test]
    fn a_model_supplied_angle_bracket_cannot_open_an_html_tag() {
        let findings = vec![finding(
            "s/1",
            "src/a.ts",
            2,
            "<a href=\"https://evil.example/track\">click</a>",
            "<img src=x onerror=1>",
            None,
        )];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "no mod, so nothing is inline: {review:?}");
        assert!(
            !review.body.contains('<'),
            "a raw `<` reached the posted body — an <img src> would load from every reader:\n{}",
            review.body
        );
        // `>` is deliberately NOT escaped (see `inline_text`'s doc): it
        // only means anything at column 0, which this function's folding
        // already makes unreachable. Asserting the exact surviving `>`
        // pins that decision rather than leaving it implied.
        assert!(review.body.contains("&lt;img src=x onerror=1>"), "{}", review.body);
        assert!(review.body.contains("&lt;a href=\"https://evil.example/track\">click&lt;/a>"), "{}", review.body);
    }

    /// (#2310 fix loop E1-2) `scope_line`'s `not_attempted` / `errored`
    /// entries are column-0 text on the DEGRADED path, where the scope
    /// line is the entire payload. They are host-authored today, but the
    /// module's containment claim ("model text can never reach column 0")
    /// has to be true AS WRITTEN, not true because of who happens to call
    /// it — a future caller filling `errored` from an agent's own error
    /// string is the obvious next step.
    #[test]
    fn a_forged_heading_in_a_scope_entry_folds_onto_the_scope_line() {
        let scope = DeliverScope {
            errored: vec!["unit `x` (Error)\n### forged heading\nand a second line".to_string()],
            not_attempted: vec!["architectural review\n### also forged".to_string()],
            ..Default::default()
        };
        let out = render(&[], &[], DIFF, &scope, None);
        assert_eq!(out.mode, "degraded");
        let body = out.review.unwrap().body;
        assert_eq!(
            body.lines().filter(|l| l.trim_start().starts_with('#')).count(),
            1,
            "only darkmux's own `### darkmux review` may sit at column 0:\n{body}"
        );
        assert_eq!(body.lines().count(), 3, "heading, blank, one scope line:\n{body}");
        assert!(body.contains("### forged heading and a second line"), "{body}");
        assert!(body.contains("### also forged"), "{body}");
    }

    /// (#2310 fix loop E1-3) `code_span("")` used to render ` `` ` — two
    /// adjacent backticks, which CommonMark reads as an UNCLOSED backtick
    /// string and prints literally. Not a breakout (the bullet after it
    /// survives), but it is a visible artifact in a posted review, and
    /// the input is reachable: `FindingWindow::display` returns a bare
    /// `key`, and a finding with an empty key renders exactly this.
    #[test]
    fn an_empty_code_span_names_itself_instead_of_rendering_two_backticks() {
        assert_eq!(code_span(""), "`(empty)`");
        assert_eq!(code_span("   "), "`(empty)`", "an all-whitespace span is invisible, so it names itself too");
        let findings = vec![finding("/1", "", 0, "ev", "claim", None)];
        // `finding()` splits the key on `/`, so this is the empty-key
        // shape reaching `display()` through the real path.
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let body = out.review.unwrap().body;
        assert!(!body.contains("``"), "no bare double-backtick artifact in a posted body:\n{body}");
    }

    /// (#2310 fix loop E1-4) A CRLF is ONE line break, and a blank line is
    /// still one break. Mapping each break character to its own space
    /// left `a\r\nb` reading `a  b` and a paragraph gap reading `a   b`.
    #[test]
    fn a_run_of_line_breaks_folds_to_exactly_one_space() {
        assert_eq!(inline_text("a\r\nb"), "a b");
        assert_eq!(inline_text("a\n\nb"), "a b");
        assert_eq!(inline_text("a\r\n\r\n\r\nb"), "a b");
        assert_eq!(inline_text("a\n\r\n\rb"), "a b");
    }

    /// (#2310 fix loop E1-5) `fence_for`'s probe table. The rule under
    /// test is CommonMark's: a closing fence must be at least as long as
    /// its opener and must be made of the SAME character, so the fence
    /// only ever has to out-run backticks.
    #[test]
    fn fence_for_probe_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("", "```", "an empty payload still gets the three-backtick minimum"),
            ("plain text", "```", "no backticks at all"),
            ("a ` b `` c", "```", "longest run is 2, so 3 still clears it"),
            ("```", "````", "a three-backtick fence inside the payload"),
            ("````", "`````", "a four-backtick fence inside the payload"),
            ("```````", "````````", "a seven-backtick fence inside the payload"),
            (
                "   ```\nnested\n   ```",
                "````",
                "an INDENTED closing fence is still a valid closer in CommonMark \
                 (up to three spaces), so indentation must not exempt a run from the scan",
            ),
            (
                "~~~\nnested\n~~~",
                "```",
                "a tilde fence cannot close a backtick fence, so it must NOT lengthen ours",
            ),
            ("`", "```", "a lone backtick is under the minimum"),
            ("``ends here``", "```", "runs of 2 at both ends"),
        ];
        for (payload, expected, why) in cases {
            assert_eq!(&fence_for(payload).as_str(), expected, "fence_for({payload:?}): {why}");
        }
    }

    /// (#2310 fix loop E1-5) `inline_text`'s probe table — what it
    /// changes, and (just as load-bearing) what it deliberately leaves
    /// alone.
    #[test]
    fn inline_text_probe_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("a\nb", "a b", "LF"),
            ("a\rb", "a b", "lone CR"),
            ("a\r\nb", "a b", "CRLF is ONE break"),
            (
                "a\u{2028}b",
                "a\u{2028}b",
                "U+2028 LINE SEPARATOR is NOT a line ending to cmark-gfm \
                 (CommonMark 'Line endings' names only LF, CR and CRLF), so it \
                 cannot reach column 0 and is left alone",
            ),
            ("a\u{2029}b", "a\u{2029}b", "U+2029 PARAGRAPH SEPARATOR, same reasoning"),
            ("# not a heading", "# not a heading", "left alone: every call site interpolates mid-line"),
            ("- not a list item", "- not a list item", "same"),
            ("> not a block quote", "> not a block quote", "same"),
            ("1. not an ordered item", "1. not an ordered item", "same"),
            ("--- not a thematic break", "--- not a thematic break", "same"),
            ("`code`", "\u{02cb}code\u{02cb}", "prose has no delimiter to lengthen, so a backtick becomes its look-alike"),
            ("<img src=x onerror=1>", "&lt;img src=x onerror=1>", "E1-1: `<` can open raw HTML on GitHub"),
            (
                "&lt;img src=x&gt;",
                "&lt;img src=x&gt;",
                "`&` is NOT escaped: CommonMark decodes an entity reference to a literal \
                 CHARACTER in the text stream, never to markup — `&lt;` renders as a visible \
                 `<`, and re-escaping the output of a decode is not a thing GitHub does",
            ),
            ("  padded  ", "padded", "the result is trimmed"),
            ("\n\nleading and trailing breaks\n\n", "leading and trailing breaks", "folded then trimmed"),
        ];
        for (input, expected, why) in cases {
            assert_eq!(&inline_text(input).as_str(), expected, "inline_text({input:?}): {why}");
        }
    }

    /// (#2310 fix loop E1-5) `code_span`'s probe table. Its contract is
    /// the opposite of `inline_text`'s: the content is EVIDENCE (a path a
    /// reader may need to paste), so nothing inside is substituted — the
    /// delimiter grows instead.
    #[test]
    fn code_span_probe_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("plain", "`plain`", "no backticks, no padding"),
            (
                "`x`",
                "`` `x` ``",
                "backticks at BOTH ends: the delimiter grows to 2 and one padding space \
                 goes on each side, which CommonMark strips back off",
            ),
            ("`leading", "`` `leading ``", "a backtick at the start alone still needs both pads"),
            ("trailing`", "`` trailing` ``", "and at the end"),
            ("``", "``` `` ```", "content that is ONLY backticks: delimiter 3, padded"),
            ("a`b", "``a`b``", "an interior backtick needs no padding"),
            ("a``b", "```a``b```", "interior run of 2"),
            ("a\nb", "`a b`", "an embedded newline is flattened"),
            (
                "a\r\nb",
                "`a  b`",
                "a CRLF becomes TWO spaces here on purpose: unlike prose, a span's content is \
                 evidence, and one output character per input character keeps the mangling \
                 of a pathological path visible rather than tidied away",
            ),
            ("<img src=x>", "`<img src=x>`", "a `<` inside a code span is inert, and altering it would corrupt the evidence"),
            ("", "`(empty)`", "E1-3: never a bare backtick pair"),
        ];
        for (input, expected, why) in cases {
            assert_eq!(&code_span(input).as_str(), expected, "code_span({input:?}): {why}");
        }
    }

    /// (#2310 fix loop E1-6) The hardening above is a NO-OP on benign
    /// text. `golden_rendered_payload_covers_every_delivery_form` already
    /// enforces byte-identity against the committed golden — this test
    /// says WHY that golden did not have to be regenerated, by proving
    /// the committed bytes contain nothing any of E1-1..E1-4 would have
    /// touched. If a future golden legitimately needs a `<`, this test is
    /// the place that decision gets made rather than silently absorbed
    /// into a regenerated file.
    #[test]
    fn the_committed_golden_contains_nothing_the_containment_hardening_would_change() {
        let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/deliver-github-review");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&golden_path).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(!text.contains('<'), "{} carries a raw `<`, which E1-1 now escapes", path.display());
            assert!(!text.contains("\\r"), "{} carries a CR, which E1-4 now folds", path.display());
            // An empty code span is a MAXIMAL run of exactly two
            // backticks. A substring test can't tell one from the tail of
            // a ``` fence, so scan maximal runs instead. Today's golden
            // has runs of 1 (spans) and 3 (fences) only. A future golden
            // whose content legitimately needs a padded two-backtick
            // delimiter (a span whose own content starts or ends with a
            // backtick) trips this deliberately — that is the decision
            // point, not a silent regeneration.
            let mut run = 0usize;
            for c in text.chars().chain(std::iter::once('\n')) {
                if c == '`' {
                    run += 1;
                    continue;
                }
                assert_ne!(
                    run,
                    2,
                    "{} carries a two-backtick run — either an empty code span (which E1-3 now \
                     names `(empty)`) or a padded delimiter that needs a deliberate decision",
                    path.display()
                );
                run = 0;
            }
            checked += 1;
        }
        assert!(checked > 0, "no golden files found under {} — this test would pass vacuously", golden_path.display());
    }
}
