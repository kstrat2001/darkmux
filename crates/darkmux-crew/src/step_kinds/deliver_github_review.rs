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
use crate::step_kinds::types::{CwdPolicy, Port, SeatClaim, StepKind, StepOutcome, StepRunCtx};
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
/// (PR #2398 review, item 4 follow-through) Every sentence of darkmux's
/// OWN voice in the rendered review, named so the vocabulary conformance
/// test can check them EXHAUSTIVELY. Checking only a fixture's rendered
/// output tests the paths that fixture happens to take: a mutation putting
/// "the kit did not parse as a unified diff" back into the unparseable-
/// patch reason survived the fixture check, because no fixture has an
/// unparseable patch. Anything model-authored is still checked through the
/// rendered output; this array is for the words darkmux itself chooses.
#[cfg(test)]
const AUTHORED_PROSE: [&str; 5] = [
    STANDING_NARROWNESS,
    REASON_NOT_A_PATCH,
    REASON_DID_NOT_PARSE,
    REASON_INSERTION,
    REASON_OUTSIDE_DIFF,
];

const REASON_NOT_A_PATCH: &str = "The change is written out rather than as a patch, so it is quoted here";
const REASON_DID_NOT_PARSE: &str = "The change did not read as a patch, so it is quoted here";
const REASON_INSERTION: &str = "The change adds lines rather than replacing them, so it cannot be attached to a line";
const REASON_OUTSIDE_DIFF: &str = "The change touches lines outside this diff";

const STANDING_NARROWNESS: &str =
    "This review checks a fixed set of rules; it is not a full design review.";

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
    /// (#2429 part 4) The `head_sha` this run's plan/units read the diff
    /// at, echoed at the TOP LEVEL of the emitted payload (never inside
    /// `review` — the posted body/comments are author-facing text, this is
    /// provenance for the poster). `None` when the caller supplied no
    /// `head_sha` (every non-CI caller today — see `DeliverConfig`'s own
    /// doc). The workflow's post step reads this back and compares it
    /// against a fresh `gh pr view --json headRefOid` at POST time: a
    /// review can run for tens of minutes, and a branch that moved during
    /// that window means the anchored comments may point at lines that no
    /// longer exist — this field is what makes that check possible without
    /// threading a second out-of-band value through the workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at_sha: Option<String>,
    /// (#2431 round 2, MF-C) A summary safe to post as a PLAIN PR comment
    /// when the formal review cannot be — a moved branch, a GitHub 422 on
    /// the inline anchors, or `mode` never producing a `review` payload at
    /// all (`degraded`/`noop`). `darkmux-review.yml`'s own fallback path
    /// used to read a `.comment` field this type never had (silently
    /// posting the literal text "null"); this is that field, for real.
    /// The scope line (same text `scope_line` renders), then one
    /// `path:line — claim` bullet per KEPT finding — whether it rendered
    /// inline, as a body count, or not at all yet (a withheld finding
    /// never gets a bullet here either; see [`should_withhold`]) — so an
    /// author reading the fallback comment sees the same findings the
    /// formal review would have raised, just without the one-click
    /// suggestions. Always present, even when empty of bullets (a clean
    /// `noop` run's fallback is just its scope line).
    pub fallback_comment: String,
    /// (PR #2398 review, item 5) What was delivered, one row per rendered
    /// entry — the operator's index back from a posted review to the
    /// records behind it, now that the finding KEY no longer renders in
    /// the author-facing body (and must not: it is darkmux's own record
    /// id, meaningless to the author).
    ///
    /// `#[serde(skip)]` on purpose: this is provenance for the OPERATOR,
    /// carried in the step's promoted output beside `mode`/`summary`/
    /// `emit` (`DeliverGithubReviewStepKind::run`), never in the GitHub
    /// payload written to `emit` — which stays exactly the bytes that get
    /// posted, and nothing else.
    #[serde(skip)]
    pub entries: Vec<DeliveredEntry>,
    /// (#2429 part 5) How many findings this render DROPPED because their
    /// own claim answered a yes/no/can't-tell rule negatively ("No, ..." /
    /// "Can't tell ..." — see [`render_github_review`]'s own doc). Belt and
    /// braces on top of the unit brief's own instruction not to call
    /// `create_finding` for such an answer; counted here, never posted,
    /// same `#[serde(skip)]` provenance-for-the-operator treatment as
    /// [`Self::entries`].
    #[serde(skip)]
    pub dropped_non_findings: usize,
}

/// One rendered entry's provenance — the row `DeliverOutcome::entries`
/// carries. `rendered_as` is the SHAPE the entry took — `"suggestion"` (a
/// gate-passed mod's hunk, anchored inside the diff), `"comment"` (any
/// other finding anchored INSIDE the diff, a plain inline comment), or
/// `"body"` (a gate-passed mod's hunk that could not become a suggestion,
/// a finding anchored OUTSIDE the diff, or a finding with no anchor at
/// all — #2429 and #2431 round 2 MF-A folded all three into the one
/// body-rendered shape) — one row per rendered artifact: a change whose
/// patch spanned two hunks, one inside the diff and one outside, is two
/// rows, because that is two things a reader sees. A finding
/// [`should_withhold`] withholds gets NO row at all — see
/// [`DeliverOutcome::dropped_non_findings`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub key: String,
    pub rendered_as: String,
}

impl DeliveredEntry {
    fn of(window: &FindingWindow, rule: Option<&str>, rendered_as: &str) -> Self {
        Self {
            rule: rule.map(str::to_string),
            path: window.file.clone(),
            line: window.line,
            key: window.key.clone(),
            rendered_as: rendered_as.to_string(),
        }
    }
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
/// (#2429 "every finding is an inline conversation") **Every finding
/// anchored to a `path`+`line` becomes an inline review comment — never a
/// body bullet.** A gate-passed mod anchored inside the diff still rides
/// as a one-click `suggestion` block, exactly as before this packet
/// (`render_gated_mod`, unchanged); every OTHER anchored finding —
/// whatever its rule's confirm form (`mod`/`search`/`question`) — becomes
/// a plain inline comment carrying the claim ([`plain_finding_comment_body`]),
/// so the author resolves it as a conversation on the line it names rather
/// than hunting a body bullet for the context a line number already gives
/// for free. Kain's own framing (#2429): "I can set rules to make sure
/// merges are gated by unresolved conversations so that an author has to
/// address each one, even if it is a false positive" — a claim sitting in
/// the body is invisible to that gate; a claim on a `comments[]` entry is
/// not.
///
/// Only a finding with no anchor INSIDE the diff (its own emission
/// carries no `file`/`line`, or names a real line the diff never touches —
/// #2431 round 2 MF-A) falls back to the body — and even then it renders
/// as a COUNT under its rule's heading, never its claim. The body is a summary
/// surface now: the coverage line, a per-rule heading for whatever a rule
/// still has to say once its findings are all accounted for elsewhere (a
/// gate-passed mod's fenced fallback when it can't become a suggestion, or
/// an unanchored count), the errored-unit note, and the fixed-rule
/// disclaimer — never a second copy of a claim the reader already saw
/// inline.
///
/// (#2429 part 5, redesigned #2431 round 2 MF-B) A finding whose
/// STRUCTURED `answer` is `no`/`cannot_tell` answered a yes/no/can't-tell
/// rule NEGATIVELY and should never have called `create_finding` at all —
/// the unit brief says so now (`crawl::unit_step::pattern_block`'s
/// `ConfirmForm::Question` arm, and `runtime/src/tools/mod.rs`'s
/// `Tool::CreateFinding` schema, which documents the `answer` field) — but
/// a model that already didn't comply cannot be trusted to comply this
/// run either, so [`should_withhold`] drops it unconditionally, before it
/// ever reaches a mod lookup or a comment. This is keyed ONLY on the
/// structured field, never on the claim's own wording — an earlier
/// word-heuristic version of this guard (judging whether `why` itself
/// opened with "No"/"Can't tell") shipped and then had to be removed: it
/// silently dropped real findings whose claim legitimately opened with
/// those words ("No test in planning.spec.ts exercises this path") and,
/// being case-sensitive for "No" but not "can't tell", also mis-dropped
/// unrelated claims ("no-op wrapper…", "NO_COLOR is read…"). Counted in
/// [`DeliverOutcome::dropped_non_findings`], never posted.
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
    absence_backstop: &BTreeMap<String, crate::absence_backstop::AbsenceBackstopNote>,
) -> DeliverOutcome {
    let touched = diff_touched_lines(diff);

    let mut comments: Vec<GithubReviewComment> = Vec::new();
    let mut rule_groups: Vec<RuleGroup> = Vec::new();
    let mut entries: Vec<DeliveredEntry> = Vec::new();
    let mut dropped_non_findings = 0usize;
    // (#2431 round 2, MF-C) One `- path:line — claim` line per KEPT
    // finding, gathered alongside the main loop — this is what
    // `fallback_comment` posts verbatim when the formal review cannot be
    // (a moved branch, a 422, or a mode that never produces a `review`
    // payload at all). A withheld finding gets no line here either.
    let mut fallback_bullets: Vec<String> = Vec::new();
    // (post-#2431 fix loop) One `(key, reason)` per finding that has a mod
    // NOBODY ever verified — a mod exists (`create_mod`/`mod create` ran)
    // but no member of it reached `gate_passed == Some(true)`, whether
    // because nothing gated it at all (`review.json`'s `test_command` is
    // unset, so `mods_gate.rs` skips every mod with `gate_skipped_reason`)
    // or because it ran and failed. Before this, such a finding rendered
    // exactly like one nobody ever proposed anything for — the scope line's
    // own "N finding(s) considered, N refused" never mentioned it, and the
    // author had no way to learn a change was sitting in the mod store.
    let mut unverified_mods: Vec<(String, String)> = Vec::new();

    for finding in findings {
        let mut window = FindingWindow::from(finding);
        // (#1748) Attach the mechanical backstop's note, when there is
        // one, BEFORE anything reads `window`'s claim text — every
        // renderer below (`render_gated_mod`, `plain_finding_comment_body`,
        // the fallback bullet) reads the claim through `claim()`/
        // `claim_sentence()`, both of which fold this in when present, so
        // the caveat shows up wherever the claim itself shows up rather
        // than needing a second, separately-maintained render path.
        window.absence_backstop = absence_backstop.get(&finding.key).cloned();
        // (#2431 round 2, MF-B) Withheld before anything else runs: a
        // finding whose STRUCTURED `answer` is `no`/`cannot_tell` gets no
        // mod lookup, no comment, no body bullet, and no `DeliveredEntry`
        // row — it is treated as though the unit never called
        // `create_finding` for it at all. A finding with no `answer` field
        // (every non-question finding, and a question-form one from a
        // unit that omitted it) is NEVER withheld here.
        if should_withhold(window.answer.as_deref()) {
            dropped_non_findings += 1;
            continue;
        }
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
            // (#2429 part 1) The FORM does not gate this branch — a passed
            // change renders as a change no matter which way its rule is
            // confirmed, and no matter whether it lands as a suggestion or
            // (outside the diff / not a patch at all) a fenced body block
            // — both unchanged from before this packet.
            let group = group_for(&mut rule_groups, rule.as_deref(), rule_titles);
            let rendered = render_gated_mod(m, &window, &touched, &mut comments, group);
            for _ in 0..rendered.suggestions {
                entries.push(DeliveredEntry::of(&window, rule.as_deref(), "suggestion"));
            }
            for _ in 0..rendered.patches {
                entries.push(DeliveredEntry::of(&window, rule.as_deref(), "body"));
            }
            fallback_bullets.push(format!("- {} — {}", window.span(), claim_sentence(&window)));
            continue;
        }
        // (#2431 round 2, MF-A) A `file`+`line` alone is not enough: GitHub
        // rejects the WHOLE review if even one comment anchors to a line
        // outside the PR diff (darkmux-review.yml's own 422 fallback
        // comment names this), the same `line_touched` predicate
        // `render_gated_mod` already uses to decide whether a suggestion's
        // hunk sits inside the diff. A finding at a real but off-diff line
        // is exactly as unanchorable, for this purpose, as one with no
        // line at all.
        let has_anchor = window.file.is_some() && line_touched(&touched, window.file.as_deref().unwrap_or(""), window.line);
        // (post-#2431 fix loop) `gated` is `None` here, so ANY mod naming
        // this finding is, by construction, one nothing ever verified —
        // never conflated with `gated` itself (a passed mod took the
        // early-return branch above and never reaches this line).
        let unverified: Vec<&GatedMod> =
            mods.iter().filter(|m| m.record.r#for.iter().any(|k| k == &finding.key)).collect();
        let unverified_note = unverified_mods_note(&unverified);
        // (post-#2431 fix loop, round 2 CONSIDER 2) Every match, not just
        // the first — a mod naming two findings would otherwise silently
        // drop its second appearance from the scope-level tally.
        // `dedup_unverified` (below, after the loop) collapses a mod
        // pushed more than once (naming several findings this run) back
        // down to one scope-line entry.
        for m in &unverified {
            unverified_mods.push((m.record.key.clone(), unverified_reason(m)));
        }
        if has_anchor {
            // (#2429 part 1) No gated mod, but the finding names a real,
            // in-diff line: an inline comment, never a body bullet — the
            // shape every in-diff finding takes now, regardless of its
            // rule's confirm form.
            let mut body = plain_finding_comment_body(&window, rule.as_deref());
            if let Some(note) = &unverified_note {
                body.push_str("\n\n");
                body.push_str(note);
            }
            comments.push(GithubReviewComment {
                path: window.file.clone().expect("has_anchor checked file.is_some()"),
                line: window.line.expect("has_anchor checked line via line_touched, which requires Some"),
                start_line: None,
                side: Some("RIGHT".to_string()),
                body,
            });
            entries.push(DeliveredEntry::of(&window, rule.as_deref(), "comment"));
        } else {
            // (#2429 part 1) No anchor at all — the one case that still
            // reaches the body, and even then as a COUNT under its rule's
            // heading, never the claim.
            let group = group_for(&mut rule_groups, rule.as_deref(), rule_titles);
            group.unanchored += 1;
            if let Some(note) = &unverified_note {
                group.bullets.push(format!("- {note}"));
            }
            entries.push(DeliveredEntry::of(&window, rule.as_deref(), "body"));
        }
        fallback_bullets.push(format!("- {} — {}", window.span(), claim_sentence(&window)));
    }
    // (post-#2431 fix loop, round 2 CONSIDER 2) One mod naming several
    // findings was pushed once per finding above; collapse back to one
    // scope-line entry per unique mod key.
    let unverified_mods = dedup_unverified(unverified_mods);

    let unresolved = unresolved_rule_titles(findings, rule_titles);
    // (#2431 round 2, MF-C) Built ONCE, reused across every `mode` this
    // function can return — the scope line, then every kept finding's own
    // `path:line — claim` bullet, in the order they arrived.
    let fallback_comment = build_fallback_comment(
        scope,
        findings.len(),
        &unresolved,
        dropped_non_findings,
        &unverified_mods,
        &fallback_bullets,
    );

    // Captured before `body` takes ownership below — whether there was
    // anything to say at all decides `mode`. A gate-passed mod whose every
    // hunk became a suggestion still populates `comments`, even though it
    // left its own `RuleGroup` empty (nothing further to say in the body)
    // — so this check reads `comments` first, the same as before this
    // packet.
    let nothing_to_say = comments.is_empty() && rule_groups.is_empty();
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
            return DeliverOutcome {
                mode: "noop".to_string(),
                review: None,
                reviewed_at_sha: None,
                fallback_comment,
                entries,
                dropped_non_findings,
            };
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
            vec![
                "### darkmux review".to_string(),
                String::new(),
                scope_line_with_withheld(scope, findings.len(), &unresolved, dropped_non_findings, &unverified_mods),
            ];
        if let Some(a) = attribution.filter(|a| !a.trim().is_empty()) {
            body.push(String::new());
            body.push(format!("_{a}_"));
        }
        return DeliverOutcome {
            mode: "degraded".to_string(),
            review: Some(GithubReviewPayload { event: "COMMENT".to_string(), body: body.join("\n"), comments: Vec::new() }),
            reviewed_at_sha: None,
            fallback_comment,
            entries,
            dropped_non_findings,
        };
    }

    let mut body = vec!["### darkmux review".to_string(), String::new()];
    body.push(scope_line_with_withheld(scope, findings.len(), &unresolved, dropped_non_findings, &unverified_mods));
    // (#2429 part 2) The body is a summary surface now: only a group that
    // still has something to say (a gated mod's fenced fallback, or an
    // unanchored count) gets a heading at all — a rule whose every finding
    // rendered cleanly as a suggestion or a plain inline comment leaves no
    // trace here, because the reader already saw it at the line.
    for group in &rule_groups {
        if group.bullets.is_empty() && group.unanchored == 0 {
            continue;
        }
        body.push(String::new());
        body.push(group.heading("**"));
        body.extend(group.bullets.iter().cloned());
        if group.unanchored > 0 {
            let word = if group.unanchored == 1 { "finding" } else { "findings" };
            body.push(format!("- {} {word} could not be anchored to a line.", group.unanchored));
        }
    }
    if let Some(a) = attribution.filter(|a| !a.trim().is_empty()) {
        body.push(String::new());
        body.push(format!("_{a}_"));
    }

    DeliverOutcome {
        mode: "review".to_string(),
        review: Some(GithubReviewPayload { event: "COMMENT".to_string(), body: body.join("\n"), comments }),
        reviewed_at_sha: None,
        fallback_comment,
        entries,
        dropped_non_findings,
    }
}

/// (#2431 round 2, MF-C) [`DeliverOutcome::fallback_comment`]'s builder —
/// the scope line, then one `- path:line — claim` bullet per KEPT finding
/// (`fallback_bullets`, gathered in [`render_github_review`]'s own main
/// loop), so a poster can fall back to a plain `gh pr comment` and still
/// show the author every finding a formal review would have raised.
fn build_fallback_comment(
    scope: &DeliverScope,
    findings_considered: usize,
    unresolved_rules: &BTreeSet<String>,
    withheld: usize,
    unverified: &[(String, String)],
    bullets: &[String],
) -> String {
    let mut lines =
        vec![scope_line_with_withheld(scope, findings_considered, unresolved_rules, withheld, unverified)];
    if !bullets.is_empty() {
        lines.push(String::new());
        lines.extend(bullets.iter().cloned());
    }
    lines.join("\n")
}

/// One rule's LEFTOVER body content, in the order its findings arrived —
/// the rule (not the confirmation procedure) is what heads a group, same
/// as before this packet, but the group's job narrowed (#2429): it no
/// longer holds every finding's bullet, only what a gated mod could not
/// turn into a suggestion (`bullets`) and how many of its findings had no
/// anchor at all (`unanchored`). A rule whose every finding rendered
/// inline never gets a `RuleGroup` with anything in it, and
/// [`render_github_review`]'s own body loop skips an empty one.
struct RuleGroup {
    rule_id: Option<String>,
    /// The rule's author-facing title, when one resolved. `None` falls the
    /// heading back to the rule id itself (and the scope line names the
    /// rule whose title was missing).
    title: Option<String>,
    bullets: Vec<String>,
    /// (#2429 part 1) Findings for this rule with no `path`+`line` at all
    /// — the one case that still reaches the body, and even then as a
    /// count, never a claim.
    unanchored: usize,
}

impl RuleGroup {
    /// `emphasis` is the markdown wrapper for the rule's NAME — always
    /// `**` now (#2429 removed the separate "Worth a double check" tail
    /// section and its `_`-emphasis groups, since a leftover group is no
    /// longer a lead sitting beneath a headline, it is the whole body).
    /// The rule id rides as a code span beside it: the author's handle for
    /// re-running this exact check after a fix.
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
///
/// `pub(crate)` (#1748) so `crate::absence_backstop::run_backstop` can
/// resolve the same rule id this module already uses, rather than a
/// second copy of the same lookup drifting from this one.
pub(crate) fn rule_id_of(finding: &FindingRecord) -> Option<String> {
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
    findings.iter().filter_map(rule_id_of).filter(|id| resolved_title(rule_titles, id).is_none()).collect()
}

/// A rule's title, iff one is actually there. (PR #2398 review, item 3) A
/// rule declaring `"title": ""` is a rule with NO title — rendering it
/// produced a heading of two bare emphasis markers (`****`), which is both
/// meaningless to a reader and invisible to the scope line's own
/// "titles unavailable" notice. The heading falls back to the rule id and
/// the notice names it, exactly as an absent title does. Both the group
/// heading and that notice read titles through here, so the two can never
/// disagree about what counts as resolved.
fn resolved_title<'t>(rule_titles: &'t BTreeMap<String, String>, id: &str) -> Option<&'t str> {
    rule_titles.get(id).map(String::as_str).filter(|t| !t.trim().is_empty())
}

/// The group for `rule`, appended if this is its first entry — so groups
/// render in the order their first finding arrived, not in id order.
fn group_for<'g>(
    groups: &'g mut Vec<RuleGroup>,
    rule: Option<&str>,
    rule_titles: &BTreeMap<String, String>,
) -> &'g mut RuleGroup {
    let key = rule.map(str::to_string);
    if let Some(i) = groups.iter().position(|g| g.rule_id == key) {
        return &mut groups[i];
    }
    let title = key.as_ref().and_then(|id| resolved_title(rule_titles, id)).map(inline_text);
    groups.push(RuleGroup { rule_id: key, title, bullets: Vec::new(), unanchored: 0 });
    let last = groups.len() - 1;
    &mut groups[last]
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
    titles_of(rules)
}

/// The titled subset of a rule registry. (PR #2398 review, item 3) A
/// blank `title` is dropped HERE, at the source, as well as being treated
/// as absent by [`resolved_title`] at the sink — a user rule tier is
/// hand-edited, and `"title": ""` (or `"   "`) is a rule someone started
/// naming and did not finish, not a name.
fn titles_of(rules: BTreeMap<String, crate::rules::Rule>) -> BTreeMap<String, String> {
    rules
        .into_iter()
        .filter_map(|(id, rule)| rule.title.filter(|t| !t.trim().is_empty()).map(|t| (id, t)))
        .collect()
}

/// The finding's own claim, folded onto one line — with the mechanical
/// absence-backstop's caveat appended when [`FindingWindow::absence_backstop`]
/// is set (#1748). This is the ONE place the caveat is added: every
/// renderer that calls `claim`/`claim_sentence` (a gated-mod's suggestion
/// body, a plain inline comment, the fallback bullet) picks it up for
/// free, so a contradicted claim reads with its caveat wherever it is
/// shown, not just in one render path.
fn claim(window: &FindingWindow) -> String {
    let base = inline_text(window.why.as_deref().unwrap_or("(no claim recorded)"));
    match &window.absence_backstop {
        Some(note) => format!("{base} {}", absence_backstop_caveat(note)),
        None => base,
    }
}

/// (#1748) The caveat sentence a contradicted absence claim carries. Named
/// as its own function (rather than inlined into [`claim`]) so a test can
/// assert its wording directly without re-deriving it from a fixture.
fn absence_backstop_caveat(note: &crate::absence_backstop::AbsenceBackstopNote) -> String {
    // Ends with `.` deliberately (never a trailing `)` or similar) — this
    // string is itself fed back through `claim_sentence`, whose own
    // sentence-punctuation check only recognizes `. ! ? : ; ,`; anything
    // else gets a SECOND period appended.
    match note.line {
        Some(line) => format!(
            "A mechanical check found `{}` elsewhere in this file, at {}:{} — this claim may not hold; verify before relying on it.",
            note.token, note.file, line
        ),
        None => format!(
            "A mechanical check found `{}` elsewhere in this file, in {} — this claim may not hold; verify before relying on it.",
            note.token, note.file
        ),
    }
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

/// (#2431 round 2, MF-B) Whether a `confirm: "question"` finding should be
/// WITHHELD — never rendered, never a comment, never a body bullet — read
/// from the STRUCTURED `answer` field the unit brief now asks a model to
/// set (`crawl::unit_step::pattern_block`'s `ConfirmForm::Question` arm;
/// `runtime/src/tools/mod.rs`'s `Tool::CreateFinding` schema documents the
/// four values). This REPLACES an earlier word-heuristic version of this
/// function that judged the claim's own FIRST WORD ("No"/"Can't tell") —
/// proven, live, to drop real findings whose claim legitimately opened
/// with those words ("No test in planning.spec.ts exercises this path",
/// "No caller updates the cache…") and, because the "No" check was
/// case-sensitive while "can't tell" was not, to ALSO drop unrelated
/// claims like "no-op wrapper…" and "NO_COLOR is read…". A structured
/// field the model sets DELIBERATELY (or leaves absent) cannot misfire on
/// a claim's incidental wording — a finding with no `answer` at all
/// (every `confirm: "mod"`/`"search"` finding, and a `"question"` finding
/// from a unit that never sets the field) is never withheld by this
/// function; it renders exactly as before.
fn should_withhold(answer: Option<&str>) -> bool {
    matches!(answer.map(str::trim).map(str::to_ascii_lowercase).as_deref(), Some("no") | Some("cannot_tell"))
}

/// A finding with no gate-passed mod, anchored to a `path`+`line`: a plain
/// inline comment carrying the claim, in the same lead-in style a
/// suggestion comment uses (`- <claim>`, [`render_gated_mod`]'s own
/// `body`), with the rule that flagged it named on its own line the same
/// way a group heading names it ([`RuleGroup::heading`]'s `code_span(id)`)
/// — the comment has no group heading of its own to lean on, since it
/// lives at the line rather than in the body.
fn plain_finding_comment_body(window: &FindingWindow, rule: Option<&str>) -> String {
    let claim = claim(window);
    match rule {
        Some(id) => format!("- {claim}\n\n{}", code_span(id)),
        None => format!("- {claim}"),
    }
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

/// (#2431 round 2, MF-B) `scope_line`, with a trailing clause naming how
/// many findings this run WITHHELD — a `confirm: "question"` finding whose
/// structured `answer` was `no`/`cannot_tell` (`should_withhold`). A
/// separate helper rather than a new `scope_line` parameter: most of
/// `scope_line`'s own direct callers (its unit tests) have no withheld
/// count to report, and this keeps that signature — and every one of
/// those tests — untouched.
fn scope_line_with_withheld(
    scope: &DeliverScope,
    findings_considered: usize,
    unresolved_rules: &BTreeSet<String>,
    withheld: usize,
    unverified: &[(String, String)],
) -> String {
    let mut line = scope_line(scope, findings_considered, unresolved_rules);
    if withheld > 0 {
        let word = if withheld == 1 { "finding" } else { "findings" };
        line.push_str(&format!(" {withheld} {word} withheld: the unit answered no or could not tell."));
    }
    if let Some(suffix) = unverified_scope_suffix(unverified) {
        line.push(' ');
        line.push_str(&suffix);
    }
    line
}

/// (post-#2431 fix loop) A mod that exists for a finding but has not
/// passed a gate — [`render_github_review`]'s own `unverified` list — WHY
/// this exists: `mod-1788657840-b8bcdc` (a real review run, 2026-09-06)
/// proposed a real change, `mods_gate.rs` skipped it (`review.json`'s
/// `test_command` is unset by default, so nothing ever ran it), and the
/// delivered review's scope line read "5 finding(s) considered, 0
/// refused" — no mention that a proposed change existed at all. This
/// names it, in the same voice as the rest of the scope line.
fn unverified_reason(gated: &GatedMod) -> String {
    if let Some(reason) = gated.record.gate_skipped_reason.as_deref() {
        return reason.to_string();
    }
    match gated.gate_passed {
        // (test-fixture note) Real records keep `gate_passed` and
        // `record.gate` in sync (`records_gather.rs` derives the former
        // FROM the latter) — reading `gate_passed` first, rather than
        // `record.gate` directly, is what keeps this function honest even
        // if a caller ever constructs a `GatedMod` the two ways out of
        // step (as this module's own test helpers deliberately do, to
        // isolate "which mod wins" from "what did the gate record").
        Some(false) => gated
            .record
            .gate
            .as_ref()
            .and_then(|g| g.reason.clone())
            .unwrap_or_else(|| "the gate ran and did not pass".to_string()),
        _ => "not yet gated".to_string(),
    }
}

/// The per-finding note ([`render_github_review`]'s comment/body path) —
/// `None` when the finding has no unverified mod at all. `matches` holds
/// every mod naming this finding that reached this point (i.e. none of
/// them passed a gate, since a passed one would have taken the earlier
/// `gated` branch and never reach here). (post-#2431 fix loop, round 2
/// CONSIDER 3) Lists EVERY match, not just the first — the count already
/// said N; a finding with two unverified mods now also names both keys,
/// not just one, so `darkmux mod show <key>` actually resolves to
/// something for each one the count promised.
fn unverified_mods_note(matches: &[&GatedMod]) -> Option<String> {
    if matches.is_empty() {
        return None;
    }
    let word = if matches.len() == 1 { "change" } else { "changes" };
    let details = matches
        .iter()
        .map(|m| format!("{}; `darkmux mod show {}`", unverified_reason(m), m.record.key))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(format!("{} proposed {word} not verified — {details}", matches.len()))
}

/// (post-#2431 fix loop, round 2 CONSIDER 2) Dedup a `(key, reason)` list
/// by key, keeping each key's FIRST occurrence — a single mod can name
/// more than one finding (`ModRecord::r#for` is a list), so collecting one
/// entry per FINDING that mod addresses would otherwise list the same mod
/// twice in the scope line. The scope line only needs to name a given
/// unverified mod once, however many findings it touches.
fn dedup_unverified(list: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    list.into_iter().filter(|(key, _)| seen.insert(key.clone())).collect()
}

/// [`render_github_review`]'s own `unverified_mods` list, rebuilt from
/// scratch for [`DeliverGithubReviewStepKind::run`]'s SEPARATE `summary`
/// recomputation (the step's promoted `summary` field is computed directly
/// from `cfg.scope`/`cfg.findings`/`cfg.mods`, never read back off the
/// `DeliverOutcome` `render_github_review` already built) — so the two
/// never drift on what counts as "not verified" for the same run. A
/// withheld finding is skipped, matching the main loop's own
/// `should_withhold` short-circuit. Collects EVERY unverified mod per
/// finding, then [`dedup_unverified`]s the whole list — the same two-step
/// shape the main loop itself now follows, so a mod naming several
/// findings is still named only once here too.
fn unverified_mods_for(findings: &[FindingRecord], mods: &[GatedMod]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for f in findings.iter().filter(|f| !should_withhold(FindingWindow::from(f).answer.as_deref())) {
        let has_passed_mod =
            mods.iter().any(|m| m.record.r#for.iter().any(|k| k == &f.key) && m.gate_passed == Some(true));
        if has_passed_mod {
            continue;
        }
        for m in mods.iter().filter(|m| m.record.r#for.iter().any(|k| k == &f.key)) {
            out.push((m.record.key.clone(), unverified_reason(m)));
        }
    }
    dedup_unverified(out)
}

/// The scope-line-level suffix, aggregating one `(key, reason)` per
/// UNIQUE unverified mod (the caller is expected to have already run the
/// list through [`dedup_unverified`]). `None` when there were none, so
/// the caller never appends a stray space.
fn unverified_scope_suffix(unverified: &[(String, String)]) -> Option<String> {
    if unverified.is_empty() {
        return None;
    }
    let word = if unverified.len() == 1 { "change" } else { "changes" };
    let details = unverified
        .iter()
        .map(|(key, reason)| format!("{reason}; `darkmux mod show {key}`"))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(format!("{} proposed {word} not verified — {}.", unverified.len(), details))
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
/// What one gate-passed change actually rendered as — the counts
/// `render_github_review` turns into [`DeliveredEntry`] rows.
struct ModRender {
    suggestions: usize,
    patches: usize,
}

fn render_gated_mod(
    m: &GatedMod,
    window: &FindingWindow,
    touched: &BTreeMap<String, BTreeSet<u32>>,
    comments: &mut Vec<GithubReviewComment>,
    group: &mut RuleGroup,
) -> ModRender {
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
        group.bullets.push(fenced_patch_bullet(window, kit, REASON_NOT_A_PATCH));
        return ModRender { suggestions: 0, patches: 1 };
    }
    let hunks = crate::diff::parse_diff(kit);
    if hunks.is_empty() {
        // Declared a unified diff but nothing parsed. Never guess at
        // intent; quote it as written, same as any other shape.
        group.bullets.push(fenced_patch_bullet(window, kit, REASON_DID_NOT_PARSE));
        return ModRender { suggestions: 0, patches: 1 };
    }
    let mut suggested = 0usize;
    let mut patched = 0usize;
    for (path, file_hunks) in &hunks {
        for h in file_hunks {
            if h.old_block.is_empty() {
                // A pure-insertion hunk has no OLD range to anchor a
                // REPLACEMENT suggestion against — GitHub suggestions
                // replace an existing line range; they cannot insert
                // between two lines with no line of their own.
                group.bullets.push(fenced_hunk_bullet(window, path, h, REASON_INSERTION));
                patched += 1;
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
                //
                // (PR #2398 review, MUST FIX — PROVEN) It rides behind a
                // `- ` OF THIS MODULE'S OWN, never at column 0. This
                // module's containment rests on ONE precondition —
                // `inline_text` folds line breaks and neutralizes
                // backticks and `<`, and deliberately leaves `#`, `>`,
                // `-`, `***`, `1.` alone BECAUSE every call site
                // interpolates it mid-line (see `inline_text`'s own doc).
                // Leading a comment with it broke that precondition: a
                // `why` of "### darkmux review - approved, merge this"
                // forged a heading inside a comment posted under darkmux's
                // byline. The bullet restores the invariant structurally
                // rather than by adding a second escape table.
                let claim = claim(window);
                let body = if claim.is_empty() {
                    format!("{fence}suggestion\n{suggestion}\n{fence}")
                } else {
                    format!("- {claim}\n\n{fence}suggestion\n{suggestion}\n{fence}")
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
                group.bullets.push(fenced_hunk_bullet(window, path, h, REASON_OUTSIDE_DIFF));
                patched += 1;
            }
        }
    }
    // (#2429 part 2) No trailing "a suggested change is attached to that
    // line" pointer bullet any more: the suggestion comment itself already
    // carries the claim (`body` above), so a body-side pointer would be a
    // second copy of the same sentence, which is exactly the "claims in
    // the body" this packet removes. The body still gets a bullet for
    // every hunk that COULD NOT become a suggestion (`fenced_hunk_bullet`
    // / `fenced_patch_bullet` above) — those are the change itself, not a
    // claim about it, so they stay.
    ModRender { suggestions: suggested, patches: patched }
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
    /// (#2429) No longer read by this module — the "Candidates" suffix
    /// that used to interpolate it into a question bullet is gone (part
    /// 3), and no other renderer here needs the raw `evidence` string.
    /// Kept off `FindingWindow` entirely rather than carried unused: a
    /// dead field here would be exactly the kind of drift `darkmux
    /// doctor`-style hygiene exists to catch, and this module is the
    /// pure-function surface a golden test reads directly.
    why: Option<String>,
    /// (#2431 round 2, MF-B) The unit's own structured yes/no/partly/
    /// cannot_tell answer, for a `confirm: "question"` finding — optional
    /// on the wire (`runtime/src/tools/mod.rs`'s `Tool::CreateFinding`
    /// schema), and simply `None` on any finding whose unit never set it
    /// (every `mod`/`search`-confirmed finding, and a `question`-confirmed
    /// one from a unit that omitted it). [`should_withhold`] is the only
    /// reader.
    answer: Option<String>,
    /// (#1748) The mechanical absence-claim backstop's note for THIS
    /// finding, when it has one — `None` on every `FindingWindow` built by
    /// [`Self::from`] alone (which has no visibility into the run-level
    /// backstop map); [`render_github_review`]'s own loop is what sets
    /// this, per-finding, right after constructing the window. `claim`
    /// folds it into the rendered text when present.
    absence_backstop: Option<crate::absence_backstop::AbsenceBackstopNote>,
}

impl FindingWindow {
    fn from(finding: &FindingRecord) -> Self {
        let get = |k: &str| finding.emitted.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Self {
            key: finding.key.clone(),
            file: get("file"),
            line: finding.emitted.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
            why: get("why"),
            answer: get("answer"),
            absence_backstop: None,
        }
    }

    /// The window's human-readable content — the LOCATION, `file:line`.
    /// `file` is MODEL-authored (it comes out of the finding's own
    /// emission), so every caller renders this through [`Self::span`]
    /// rather than pasting it between two literal backticks (#2310 fix
    /// loop A, S5-4).
    ///
    /// (#2310 delivery rewrite) The finding KEY used to lead this string
    /// when there was no location at all — darkmux's own record id,
    /// meaningless to the author reading the review. (#2431 round 3) It
    /// still is, but the KEY itself no longer renders even here:
    /// `fallback_comment` posts this text verbatim to the PR, and #2398
    /// removed record keys from posted text everywhere else — a bare
    /// `sess-a/3` surviving in the one code path nobody thought to check
    /// would be exactly the leak that fix was for. `"(no anchor)"` names
    /// the same fact (nothing to point the reader at) without exposing
    /// the id.
    fn display(&self) -> String {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{f}:{l}"),
            (Some(f), None) => f.clone(),
            _ => "(no anchor)".to_string(),
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
    /// (#2429 part 4) The sha this run's plan/units read the diff at —
    /// launch-time data, same as `attribution`/`emit`: it names WHEN this
    /// run looked, never WHAT it found, so it always comes from
    /// `step.config` and never from a `records.gather` envelope.
    /// `review.json`'s `deliver-step` passes it through as
    /// `"head_sha": "{{head_sha}}"`, the same mission-launch param
    /// `plan.sites` already reads. `None` for every caller that doesn't
    /// set it (every test fixture below, and any embedder that never had a
    /// head sha to begin with).
    head_sha: Option<String>,
    /// (#1748) The mechanical absence-claim backstop's findings, keyed by
    /// finding key — see `records_gather::GatherOutput::absence_backstop`'s
    /// doc. Empty for every caller that never set it: every embedded-config
    /// test fixture in this module (the check needs the whole file on
    /// disk, which a literal `Step.config` cannot carry, so this key is
    /// never read out of `step.config`) and any `GatherOutput` produced
    /// before #1748. An empty map renders every finding exactly as before
    /// this packet.
    absence_backstop: BTreeMap<String, crate::absence_backstop::AbsenceBackstopNote>,
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
        // (#2429 part 4) A blank `{{head_sha}}` (the param unset at launch)
        // reads the same as absent — never echo an empty string into the
        // payload as though it were a real sha.
        let head_sha = step.config.get("head_sha").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(str::to_string);

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
            return Ok(Self { findings, mods, diff, scope, attribution, emit, head_sha, absence_backstop: BTreeMap::new() });
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
        Ok(Self {
            findings: body.findings,
            mods: body.mods,
            diff: body.diff,
            scope: body.scope,
            attribution,
            emit,
            head_sha,
            absence_backstop: body.absence_backstop,
        })
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

    /// (#1511) `None` — the documented no-dispatch opt-out, matching this
    /// kind's [`SeatClaim::NoModel`] above: it renders and posts a GitHub review
    /// and speaks to no model, so there is no role for the
    /// licensed-adjacent consent gate to check.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
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

    /// (#2577 audit) `CwdPolicy::NoAmbientDependency` (the trait default,
    /// stated explicitly here) — this kind spawns no subprocess at all: it
    /// renders review text in-process and posts it over the network via
    /// the GitHub API, never a `Command`. Was previously covered only by
    /// the trait default (silently, with no row naming this a checked
    /// audit) — a #2577-review finding. Not a `StepKindRegistry::
    /// with_builtins()` member, so the registry conformance test cannot
    /// see this kind — audited by hand.
    fn cwd_policy(&self) -> CwdPolicy {
        CwdPolicy::NoAmbientDependency
    }

    fn run(&self, step: &Step, _task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = DeliverConfig::from_step(step, input)?;
        // (#2310 delivery rewrite) Titles resolve HERE, at the one impure
        // edge, so `render_github_review` stays a pure function over its
        // inputs and every golden test states the titles it renders under.
        let titles = rule_titles();
        let mut outcome = render_github_review(
            &cfg.findings,
            &cfg.mods,
            &cfg.diff,
            &cfg.scope,
            cfg.attribution.as_deref(),
            &titles,
            &cfg.absence_backstop,
        );
        // (#2429 part 4) Echoed onto the outcome AFTER rendering — the sha
        // names WHEN this run looked, not what it found, so it plays no
        // part in `render_github_review`'s own pure logic (mode/body/
        // comments never branch on it) and rides in unconditionally,
        // including on the `noop`/`degraded` paths, so a poster always has
        // it to compare against.
        outcome.reviewed_at_sha = cfg.head_sha.clone();
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
        // `review.json` declares `outcome_from: "deliver"`, which
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
        let summary = scope_line_with_withheld(
            &cfg.scope,
            cfg.findings.len(),
            &unresolved_rule_titles(&cfg.findings, &titles),
            outcome.dropped_non_findings,
            &unverified_mods_for(&cfg.findings, &cfg.mods),
        );
        let output = serde_json::to_string(&serde_json::json!({
            "mode": outcome.mode,
            "summary": summary,
            "emit": dest,
            // (PR #2398 review, item 5) The operator's index back from the
            // posted review to the records behind it — the finding key is
            // deliberately absent from the author-facing body, so this is
            // where it lives. Never the review text itself: the promoted
            // payload stays a summary surface (see the note above).
            "entries": outcome.entries,
            // (#2429 part 5) A count, never the dropped claims themselves
            // — same operator-provenance treatment as `entries` above.
            "dropped_non_findings": outcome.dropped_non_findings,
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
        render_github_review(findings, mods, diff, scope, attribution, &test_titles(), &BTreeMap::new())
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

    /// (post-#2431 fix loop, round 2 CONSIDER 1) Same as [`gated_mod`], but
    /// with the caller's OWN key rather than the shared fixture default
    /// `"mod-1-abcdef"` — needed wherever a fixture plants more than one
    /// unverified mod side by side (`every_form_fixture`'s gate-failed and
    /// never-gated mods), so a golden diff and a `darkmux mod show <key>`
    /// note actually pin WHICH mod a given reason belongs to instead of
    /// two mods sharing one indistinguishable key.
    fn gated_mod_with_key(key: &str, for_key: &str, kit: &str, gate_passed: Option<bool>) -> GatedMod {
        let mut m = gated_mod_kind(for_key, kit, None, gate_passed);
        m.record.key = key.to_string();
        m
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
            review.comments[0].body.starts_with("- reimplements a helper\n\n```suggestion\n"),
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
            review.comments[0].body, "- reimplements a helper\n\n```suggestion\n  const y = clamp(1);\n\n}\n```",
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
    fn a_never_gated_mod_gate_passed_none_becomes_a_plain_inline_comment() {
        // (#2310 P4b review, M-A — proven-vacuous MUST FIX) No prior
        // fixture ever planted `gate_passed: None`, so a mutation from
        // `== Some(true)` to `!= Some(false)` slipped past every existing
        // test (`None != Some(false)` is ALSO true). This test plants
        // exactly that shape and asserts the never-gated outcome; see
        // the mutation self-check in this packet's own report for the
        // red-prove against the `!= Some(false)` mutation.
        //
        // (#2429) Since it is anchored (`path`+`line`), it now renders as
        // an ordinary inline comment — never a suggestion, and never a
        // body bullet either.
        let findings = vec![finding("s/9", "src/a.ts", 2, "ev", "a mod exists but nothing ever gated it", None)];
        let mods = vec![gated_mod("s/9", "some proposed kit text", None)];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "never-gated must become a plain comment, not a suggestion: {review:?}");
        assert!(!review.comments[0].body.contains("```suggestion"));
        assert!(review.comments[0].body.contains("a mod exists but nothing ever gated it"));
        assert!(
            !review.body.contains("might duplicate")
                && !review.body.contains("a mod exists but nothing ever gated it"),
            "the claim rides the comment, never the body: {}",
            review.body
        );
        // (#2310 delivery rewrite) The finding key is darkmux's own record
        // id, meaningless to an author reading the review.
        assert!(!review.body.contains("s/9"), "the record key never renders: {}", review.body);
        assert!(
            !review.body.contains("some proposed kit text"),
            "a never-gated mod's kit must not render at all, only the finding's own claim"
        );
    }

    /// (post-#2431 fix loop, real run 2026-09-06) `mod-1788657840-b8bcdc`
    /// proposed a real change against `existing-solution`'s finding, and
    /// `mods_gate.rs` skipped gating it (`review.json`'s default
    /// `test_command` is unset) — `gate_skipped_reason: Some("no
    /// test_command configured")`. Before this fix the delivered review's
    /// scope line read "5 finding(s) considered, 0 refused" with no
    /// mention that a proposed change existed at all, and the finding's
    /// own comment carried nothing but the claim — an author reading
    /// either had no way to learn a mod was sitting in the store. Both
    /// surfaces must name it now.
    #[test]
    fn an_ungated_mod_with_a_skip_reason_is_named_in_the_comment_and_the_scope_line() {
        let findings = vec![finding("s/9", "src/a.ts", 2, "ev", "reimplements the existing helper", None)];
        let mods = vec![GatedMod {
            record: ModRecord {
                key: "mod-1788657840-b8bcdc".to_string(),
                ts: "2026-09-06T01:24:00Z".to_string(),
                by: "reviewer (qwen3.6-35b-a3b-turboquant-mlx)".to_string(),
                r#for: vec!["s/9".to_string()],
                kit: Some("Replace isLinkActive with a call to the existing isActive helper.".to_string()),
                kit_looks_json: false,
                kit_kind: None,
                attachments: Vec::new(),
                context: Default::default(),
                warnings: Vec::new(),
                mission_id: None,
                phase_id: None,
                step_id: None,
                source: None,
                gate: None,
                gate_skipped_reason: Some("no test_command configured".to_string()),
                schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            },
            gate_passed: None,
        }];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();

        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert!(
            review.comments[0].body.contains("1 proposed change not verified — no test_command configured"),
            "the finding's own comment must name the unverified mod: {:?}",
            review.comments[0]
        );
        assert!(
            review.comments[0].body.contains("darkmux mod show mod-1788657840-b8bcdc"),
            "and point at it by key: {:?}",
            review.comments[0]
        );
        assert!(
            review.body.contains("1 proposed change not verified — no test_command configured"),
            "the scope line must name it too, so a reader who never opens a single comment still \
             learns a change exists: {}",
            review.body
        );
        assert!(
            review.body.contains("darkmux mod show mod-1788657840-b8bcdc"),
            "{}",
            review.body
        );
        assert!(
            out.fallback_comment.contains("1 proposed change not verified — no test_command configured"),
            "the plain-comment fallback carries the same note: {}",
            out.fallback_comment
        );
    }

    /// (post-#2431 fix loop, round 2 CONSIDER 2 + 3) One mod names TWO
    /// findings (`s/20` and `s/21`), and `s/20` also has a SECOND
    /// unverified mod of its own. This proves both fixes at once: `s/20`'s
    /// comment must name BOTH of its mods by key (not just the first),
    /// and the scope line must list the SHARED mod's key only ONCE even
    /// though it addresses two findings, not twice.
    #[test]
    fn a_mod_naming_two_findings_is_deduped_and_a_finding_with_two_mods_names_both() {
        let findings = vec![
            finding("s/20", "src/a.ts", 2, "ev", "shared claim one", None),
            finding("s/21", "src/other.ts", 5, "ev", "shared claim two", None),
        ];
        let shared = GatedMod {
            record: ModRecord {
                key: "mod-shared".to_string(),
                ts: "2026-09-06T00:00:00Z".to_string(),
                by: "coder".to_string(),
                r#for: vec!["s/20".to_string(), "s/21".to_string()],
                kit: Some("a shared proposed change".to_string()),
                kit_looks_json: false,
                kit_kind: None,
                attachments: Vec::new(),
                context: Default::default(),
                warnings: Vec::new(),
                mission_id: None,
                phase_id: None,
                step_id: None,
                source: None,
                gate: None,
                gate_skipped_reason: Some("no test_command configured".to_string()),
                schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            },
            gate_passed: None,
        };
        let second_on_20 = GatedMod {
            record: ModRecord {
                key: "mod-second-on-20".to_string(),
                ts: "2026-09-06T00:00:01Z".to_string(),
                by: "coder".to_string(),
                r#for: vec!["s/20".to_string()],
                kit: Some("a second proposed change on the same finding".to_string()),
                kit_looks_json: false,
                kit_kind: None,
                attachments: Vec::new(),
                context: Default::default(),
                warnings: Vec::new(),
                mission_id: None,
                phase_id: None,
                step_id: None,
                source: None,
                gate: None,
                gate_skipped_reason: Some("no test_command configured".to_string()),
                schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            },
            gate_passed: None,
        };
        let mods = vec![shared, second_on_20];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();

        let s20 = review.comments.iter().find(|c| c.body.contains("shared claim one")).expect("s/20 rendered");
        assert!(s20.body.contains("2 proposed changes not verified"), "{}", s20.body);
        assert!(s20.body.contains("darkmux mod show mod-shared"), "names the shared mod: {}", s20.body);
        assert!(s20.body.contains("darkmux mod show mod-second-on-20"), "AND the second one: {}", s20.body);

        // The SCOPE LINE (the body's first paragraph, before either
        // per-finding bullet) names `mod-shared` exactly ONCE, even though
        // it addresses two findings — `s/21` (off-diff, so it ALSO gets
        // its own per-finding bullet naming `mod-shared` again further
        // down) is not double-counted in the aggregate tally itself.
        let scope_line = review.body.lines().nth(2).expect("the scope line is the body's third line");
        assert!(scope_line.contains("2 proposed changes not verified"), "{scope_line}");
        let shared_mentions_in_scope_line = scope_line.matches("mod-shared").count();
        assert_eq!(shared_mentions_in_scope_line, 1, "the shared mod is named once in the scope line: {scope_line}");
        assert!(scope_line.contains("darkmux mod show mod-second-on-20"), "{scope_line}");
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
    fn a_finding_with_no_mod_becomes_a_plain_inline_comment() {
        // (#2429) Anchored, no gated mod: a plain inline comment, never a
        // body bullet — the shape every anchored finding takes now,
        // whatever its rule's confirm form.
        let findings = vec![finding("s/3", "src/a.ts", 2, "the cited line", "might duplicate an existing helper", None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let comment = &review.comments[0];
        assert_eq!(comment.path, "src/a.ts");
        assert_eq!(comment.line, 2);
        assert!(comment.body.contains("might duplicate an existing helper"));
        assert!(!comment.body.contains("```suggestion"));
        assert!(!review.body.contains("might duplicate an existing helper"), "the claim never repeats in the body: {}", review.body);
        assert!(!review.body.contains("s/3"), "the record key never renders: {}", review.body);
    }

    #[test]
    fn a_finding_anchored_outside_the_diff_falls_back_to_a_body_count_not_a_comment() {
        // (#2431 round 2, MF-A) `has_anchor` used to mean only "the
        // finding's own emission carries a `file`+`line`" — it never
        // checked whether that line is actually IN the PR diff. GitHub
        // rejects an ENTIRE review if even one comment anchors to a line
        // outside the diff (a single bad anchor 422s the whole POST, per
        // darkmux-review.yml's own fallback comment), so a finding at a
        // real but off-diff line must take the SAME "could not be
        // anchored" body-count path an unanchored finding does.
        let findings = vec![finding("s/1", "src/z.ts", 1, "ev", "a claim about a line outside the diff", None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.dropped_non_findings, 0, "{out:?}");
        assert_eq!(out.entries.len(), 1, "{out:?}");
        assert_eq!(out.entries[0].rendered_as, "body", "{out:?}");
        let review = out.review.unwrap();
        assert!(review.comments.is_empty(), "an off-diff anchor must never become a comment: {review:?}");
        assert!(
            review.body.contains("1 finding could not be anchored to a line."),
            "an off-diff line is exactly as unanchorable as no line at all: {}",
            review.body
        );
        assert!(
            !review.body.contains("a claim about a line outside the diff"),
            "the claim never renders, only the count: {}",
            review.body
        );
    }

    #[test]
    fn a_gate_failed_mod_becomes_a_plain_inline_comment_not_a_suggestion() {
        let findings = vec![finding("s/4", "src/a.ts", 2, "ev", "claim", None)];
        let mods = vec![gated_mod("s/4", "kit text", Some(false))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert!(!review.comments[0].body.contains("```suggestion"));
        assert!(review.comments[0].body.contains("claim"));
        assert!(!review.body.contains("kit text"));
        assert!(!review.body.contains("claim"), "the claim rides the comment, not the body: {}", review.body);
    }

    #[test]
    fn a_searched_rule_with_no_change_is_still_an_inline_comment() {
        // (#2310 delivery rewrite, rule 3 — #2429 superseded the headline/
        // tail split entirely) A rule confirmed by searching, with nobody
        // proposing a change, used to be undistinguishable from an
        // all-clear search and so was demoted to a tail-section lead.
        // #2429 makes the confirm form irrelevant to WHERE a finding
        // renders: any anchored finding is an inline comment now, so this
        // rule's own "cannot tell all-clear from a real hit" limitation no
        // longer needs a body-side demotion to express.
        // (#2431 round 2, MF-A) Anchored to a line INSIDE the diff
        // (`src/a.ts:2`, one of `DIFF`'s own touched lines) — an anchor
        // outside the diff falls to the body-count path instead (see
        // `a_finding_anchored_outside_the_diff_falls_back_to_a_body_count_not_a_comment`),
        // which is not what this test is about.
        let findings = vec![finding_of_rule(
            "s/5",
            Some("shared-symbol-callers"),
            "src/a.ts",
            2,
            "14 endpoints use this middleware",
            "shared auth changed",
            Some("search"),
        )];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_eq!(review.comments[0].path, "src/a.ts");
        assert_eq!(review.comments[0].line, 2);
        assert!(review.comments[0].body.contains("shared auth changed"), "the claim is never dropped: {:?}", review.comments[0]);
        assert!(!review.body.contains("Worth a double check"), "the tail section is gone: {}", review.body);
    }

    #[test]
    fn a_question_form_finding_renders_as_a_plain_inline_comment_with_no_candidates_suffix() {
        // (#2429 part 3) The old renderer appended "? Candidates: <evidence>"
        // to a question's claim. That suffix is gone — the comment carries
        // only the claim, and the finding's own anchor already shows the
        // line the candidates search was about.
        // (#2431 round 2, MF-A) Anchored inside the diff (`src/a.ts:1`) —
        // this test is about the Candidates suffix, not the diff-anchor
        // check.
        let findings = vec![finding(
            "s/6",
            "src/a.ts",
            1,
            "Status enum, Kind enum",
            "did you check whether the repo already has this",
            Some("question"),
        )];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let comment = &review.comments[0];
        assert!(comment.body.contains("did you check whether the repo already has this"));
        assert!(!comment.body.contains("Candidates"), "{comment:?}");
        assert!(!comment.body.contains("Status enum, Kind enum"), "the evidence is not appended any more: {comment:?}");
        assert!(!review.body.contains("Candidates"), "{}", review.body);
    }

    #[test]
    fn refused_findings_never_render_only_the_scope_count_shows() {
        let scope = DeliverScope { refused: 3, rules_run: vec!["r1".into()], ..Default::default() };
        let findings = vec![finding("s/7", "src/a.ts", 2, "ev", "claim", None)];
        let out = render(&findings, &[], DIFF, &scope, None);
        let review = out.review.unwrap();
        assert!(review.body.contains("3 refused"));
    }

    /// (#2429, the issue's own fixture) A minimal unanchored finding —
    /// `finding()`/`finding_of_rule()` both always set `file`/`line`, so an
    /// un-anchored finding needs its own builder that leaves them out of
    /// `emitted` entirely, the way a rule with no location to report ever
    /// would.
    fn unanchored_finding(key: &str, rule: &str, why: &str) -> FindingRecord {
        FindingRecord {
            key: key.to_string(),
            dispatch: key.split('/').next().unwrap().to_string(),
            seq: key.split('/').nth(1).unwrap().parse().unwrap(),
            ts: "2026-09-06T00:00:00Z".to_string(),
            tool_name: "create_finding".to_string(),
            proposer: crate::findings::Proposer { handle: "reviewer".to_string(), model: "test".to_string(), machine_id: None },
            mission_id: None,
            phase_id: None,
            step_id: None,
            context: json!({ "rule": rule, "confirm": "question" }),
            emitted: json!({ "pattern": "test", "evidence": "n/a", "why": why }),
            source: None,
            schema_version: crate::findings::FINDING_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    /// (#2429) The issue's own acceptance fixture: 1 gate-passed mod with
    /// TWO hunks (both inside the diff, so both become suggestions) + 2
    /// question-form findings (each anchored, so each becomes a plain
    /// inline comment) + 1 un-anchored finding (falls back to a body
    /// bullet, as a COUNT never a claim). Expected: 4 inline comments (2
    /// suggestion + 2 plain), exactly 1 body bullet naming the un-anchored
    /// count, no claim text anywhere in the body, and no `Candidates`
    /// substring anywhere in the rendered payload.
    #[test]
    fn every_finding_becomes_an_inline_conversation_or_a_counted_fallback() {
        const TWO_HUNK_KIT: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,1 @@\n-function f() {\n+function f(): void {\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        // (#2431 round 2, MF-A) All three anchored findings sit on lines
        // `DIFF` actually touches (`src/a.ts` 1-3) — an anchor OUTSIDE the
        // diff renders as a body count instead of a comment (see
        // `a_finding_anchored_outside_the_diff_falls_back_to_a_body_count_not_a_comment`),
        // which would silently turn this fixture's "2 plain comments"
        // into 0.
        let findings = vec![
            finding_of_rule("s/1", Some("swallowed-error"), "src/a.ts", 1, "ev", "the failure is discarded", None),
            finding_of_rule(
                "s/2",
                Some("existing-solution"),
                "src/a.ts",
                2,
                "ev",
                "did you check for an existing helper",
                Some("question"),
            ),
            finding_of_rule(
                "s/3",
                Some("test-gap"),
                "src/a.ts",
                3,
                "ev",
                "does a test cover this new branch",
                Some("question"),
            ),
            unanchored_finding("s/4", "unnamed-predicate", "the compound condition has no name"),
        ];
        let mods = vec![gated_mod_kind("s/1", TWO_HUNK_KIT, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.clone().unwrap();

        assert_eq!(review.comments.len(), 4, "2 suggestion hunks + 2 plain comments: {review:?}");
        let suggestions: Vec<_> = review.comments.iter().filter(|c| c.body.contains("```suggestion")).collect();
        let plain: Vec<_> = review.comments.iter().filter(|c| !c.body.contains("```suggestion")).collect();
        assert_eq!(suggestions.len(), 2, "one comment per mod hunk: {review:?}");
        assert_eq!(plain.len(), 2, "{review:?}");
        assert!(plain.iter().any(|c| c.body.contains("did you check for an existing helper")), "{review:?}");
        assert!(plain.iter().any(|c| c.body.contains("does a test cover this new branch")), "{review:?}");

        // Exactly one body bullet — the un-anchored count — and it is a
        // COUNT, never the dropped finding's own claim.
        assert!(
            review.body.contains("1 finding could not be anchored to a line."),
            "the body states the count:\n{}",
            review.body
        );
        assert!(
            !review.body.contains("the compound condition has no name"),
            "the un-anchored finding's claim never renders, only its count:\n{}",
            review.body
        );
        // (#2429 part 2) No OTHER claim text leaks into the body either —
        // every anchored finding's claim rides its own inline comment.
        for claim in ["the failure is discarded", "did you check for an existing helper", "does a test cover this new branch"] {
            assert!(!review.body.contains(claim), "{claim:?} must not repeat in the body:\n{}", review.body);
        }
        let payload = serde_json::to_string(&out).unwrap();
        assert!(!payload.contains("Candidates"), "the `Candidates` suffix is gone entirely: {payload}");
    }

    /// (#2429 part 4) `reviewed_at_sha` rides the TOP LEVEL of the emitted
    /// payload — set by the step (`DeliverGithubReviewStepKind::run`), not
    /// by `render_github_review` itself (the sha names WHEN the run
    /// looked, never what it found).
    #[test]
    fn reviewed_at_sha_rides_the_emitted_payload() {
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
                "head_sha": "abc123def456",
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
        DeliverGithubReviewStepKind.run(&step, &task, &BTreeMap::new()).unwrap();
        let emitted: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        assert_eq!(emitted["reviewed_at_sha"], json!("abc123def456"), "{emitted}");
    }

    /// (#2429 part 4) A blank `head_sha` (the mission-launch param unset,
    /// so `{{head_sha}}` resolves to an empty string) reads as absent, not
    /// as a real sha a poster would compare against.
    #[test]
    fn a_blank_head_sha_reads_as_absent() {
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
                "head_sha": "",
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
        DeliverGithubReviewStepKind.run(&step, &task, &BTreeMap::new()).unwrap();
        let emitted: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        assert!(emitted.get("reviewed_at_sha").is_none(), "{emitted}");
    }

    /// (#2431 round 2, MF-C) `fallback_comment` — the field
    /// `darkmux-review.yml`'s post step reads for every mode's plain
    /// `gh pr comment` fallback, instead of the `.comment` key
    /// `DeliverOutcome` never had (which used to post the literal text
    /// "null"). Present and populated on the `review`, `degraded`, AND
    /// `noop` paths.
    #[test]
    fn fallback_comment_is_present_and_names_every_kept_finding() {
        let findings = vec![
            finding_of_rule("s/1", Some("swallowed-error"), "src/a.ts", 1, "ev", "the failure is discarded", None),
            finding_of_rule("s/2", Some("test-gap"), "src/a.ts", 2, "ev", "does a test cover this new branch", Some("question")),
            unanchored_finding("s/3", "unnamed-predicate", "the compound condition has no name"),
            finding_with_answer("s/4", Some("existing-solution"), "src/a.ts", 3, "ev", "No, nothing exists that does this.", "no"),
        ];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.mode, "review", "{out:?}");
        // The scope line, first.
        assert!(out.fallback_comment.contains("review ran:"), "{}", out.fallback_comment);
        // One bullet per KEPT finding — inline (`s/1`), a body count
        // (`s/2` is anchored so it becomes a plain comment too, `s/3` is
        // unanchored) — every one of them still gets a `path:line — claim`
        // line here regardless of how it rendered in the formal review.
        assert!(out.fallback_comment.contains("`src/a.ts:1` — the failure is discarded."), "{}", out.fallback_comment);
        assert!(out.fallback_comment.contains("`src/a.ts:2` — does a test cover this new branch."), "{}", out.fallback_comment);
        // `s/3` has no anchor, so `window.span()` renders `(no anchor)`
        // rather than the record key (#2431 round 3 — `fallback_comment`
        // is posted verbatim to the PR, and a record key is meaningless,
        // and #2398-forbidden, in author-facing text).
        assert!(out.fallback_comment.contains("`(no anchor)` — the compound condition has no name."), "{}", out.fallback_comment);
        assert!(!out.fallback_comment.contains("s/3"), "the record key must never render: {}", out.fallback_comment);
        // The withheld finding (`s/4`, `answer: "no"`) gets NO bullet —
        // withheld means withheld everywhere, including the fallback.
        assert!(!out.fallback_comment.contains("nothing exists"), "{}", out.fallback_comment);
        assert!(!out.fallback_comment.contains("Candidates"), "{}", out.fallback_comment);
    }

    /// (#2431 round 2, MF-C) `fallback_comment` on the `degraded`/`noop`
    /// paths, where `review`'s own body is either the SAME scope line
    /// (`degraded`) or absent entirely (`noop`, `review: None`) — the
    /// workflow's plain-comment fallback needs `fallback_comment` on
    /// EVERY mode, not just `review`.
    #[test]
    fn fallback_comment_is_present_on_degraded_and_noop() {
        let errored_scope = DeliverScope { errored: vec!["unit `x` (Error)".to_string()], ..Default::default() };
        let degraded = render(&[], &[], DIFF, &errored_scope, None);
        assert_eq!(degraded.mode, "degraded", "{degraded:?}");
        assert!(!degraded.fallback_comment.is_empty(), "{degraded:?}");
        assert!(degraded.fallback_comment.contains("Errored:"), "{}", degraded.fallback_comment);

        let clean = render(&[], &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(clean.mode, "noop", "{clean:?}");
        assert!(!clean.fallback_comment.is_empty(), "even a clean noop's fallback states the scope line: {clean:?}");
    }

    /// A structured helper: `finding_of_rule` plus the unit's own
    /// `answer` field on `emitted` — the wire shape
    /// `runtime/src/tools/mod.rs`'s `Tool::CreateFinding` schema declares.
    #[allow(clippy::too_many_arguments)]
    fn finding_with_answer(
        key: &str,
        rule: Option<&str>,
        file: &str,
        line: u32,
        evidence: &str,
        why: &str,
        answer: &str,
    ) -> FindingRecord {
        let mut f = finding_of_rule(key, rule, file, line, evidence, why, Some("question"));
        f.emitted["answer"] = json!(answer);
        f
    }

    /// (#2431 round 2, MF-B) A finding whose STRUCTURED `answer` is `no`
    /// is withheld unconditionally — no comment, no body bullet, no
    /// `DeliveredEntry` row — and counted; the scope line names the count.
    /// A sibling finding with no `answer` field renders normally.
    #[test]
    fn a_finding_with_answer_no_is_withheld_and_counted() {
        let findings = vec![
            finding_with_answer("s/1", Some("existing-solution"), "src/a.ts", 2, "ev", "No, nothing exists that does this.", "no"),
            finding_of_rule("s/2", Some("test-gap"), "src/a.ts", 1, "ev", "a real finding", None),
        ];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.dropped_non_findings, 1, "{out:?}");
        assert_eq!(out.entries.len(), 1, "the withheld finding gets no entry row: {out:?}");
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "only the real finding becomes a comment: {review:?}");
        assert!(!review.comments[0].body.contains("nothing exists"), "{review:?}");
        assert!(!review.body.contains("nothing exists"), "{}", review.body);
        assert!(
            review.body.contains("1 finding withheld: the unit answered no or could not tell."),
            "the scope line states the decision: {}",
            review.body
        );
    }

    /// (#2431 round 2, MF-B) `cannot_tell` (case-insensitive) is withheld
    /// the same way as `no`. The load-bearing half of this test is the
    /// SECOND and THIRD findings: their claims literally start with "No."
    /// and "Can't tell" — the exact shape the removed word-heuristic
    /// version of this guard used to misjudge — but since NEITHER sets
    /// the structured `answer` field, both render normally. This is the
    /// red-proof that #132's real finding ("No test in planning.spec.ts
    /// exercises this path") would no longer be silently dropped.
    #[test]
    fn cannot_tell_is_withheld_and_a_claim_starting_no_with_no_answer_field_still_renders() {
        let findings = vec![
            finding_with_answer("s/1", Some("test-gap"), "src/a.ts", 1, "ev", "Can't tell from this window alone.", "cannot_tell"),
            finding_of_rule("s/2", Some("test-gap"), "src/a.ts", 2, "ev", "No test in planning.spec.ts exercises this path.", None),
            finding_of_rule("s/3", Some("test-gap"), "src/a.ts", 3, "ev", "Notably, this branch has no test.", None),
        ];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(out.dropped_non_findings, 1, "only the `cannot_tell`-answered finding is withheld: {out:?}");
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 2, "{review:?}");
        assert!(
            review.comments.iter().any(|c| c.body.contains("No test in planning.spec.ts exercises this path")),
            "a real finding whose CLAIM happens to start with \"No\" renders, since it set no `answer` field: {review:?}"
        );
        assert!(review.comments.iter().any(|c| c.body.contains("Notably")));
    }

    /// (#2429 part 5, item 5's own conformance ask) The renderer's OWN
    /// test fixtures never claim, via a rendered comment or body bullet,
    /// something that opens with "No."/"Can't tell" — a TEST-SUITE
    /// hygiene check, not a runtime rule (the runtime rule is
    /// `should_withhold`, keyed on the structured `answer` field, never on
    /// a claim's own wording). Exercised over `every_form_fixture()`,
    /// the widest fixture this module's tests share.
    #[test]
    fn no_rendered_claim_in_the_shared_fixture_begins_with_no_or_cant_tell() {
        let (findings, mods, scope) = every_form_fixture();
        let review = render(&findings, &mods, DIFF, &scope, None).review.unwrap();
        let mut rendered = review.body.clone();
        for c in &review.comments {
            rendered.push('\n');
            rendered.push_str(&c.body);
        }
        for line in rendered.lines() {
            let after_dash = line.trim_start_matches("- ");
            // (#2431 round 3) A BODY-rendered bullet leads with a
            // `` `path:line` `` span before its claim
            // (`fenced_patch_bullet`/`fenced_hunk_bullet`/the unanchored
            // count line); a plain inline COMMENT has no span at all
            // (`plain_finding_comment_body`). Stripping only "- " left
            // every body bullet's claim hidden behind its own span, so
            // this check could never fire on that half of the renderer's
            // output — strip the span too, when there is one, so the
            // actual claim text is what gets compared either way.
            let claim = match after_dash.split_once(" — ") {
                Some((span, rest)) if span.starts_with('`') && span.ends_with('`') => rest,
                _ => after_dash,
            };
            let lower = claim.to_ascii_lowercase();
            assert!(
                !claim.starts_with("No.") && !lower.starts_with("can't tell"),
                "a rendered claim begins with a withheld-shaped prefix: {claim:?} (line: {line:?})"
            );
        }
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
    fn an_attack_payload_in_why_cannot_forge_a_header_in_a_plain_inline_comment() {
        // (#2429) The finding is anchored, so it renders as an inline
        // comment now rather than a body bullet — the containment
        // discipline has to hold on the COMMENT's body, since that is
        // where the model's `why` actually lands.
        let findings = vec![finding("s/1", "src/a.ts", 2, "ev", ATTACK, None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let comment_body = &review.comments[0].body;
        assert_folded_onto_one_line(comment_body, "plain inline comment `why`");
        assert_no_markdown_breakout(comment_body, &[], "plain inline comment `why`");
        assert!(comment_body.contains("Approved."), "the claim is folded into the comment, never dropped: {comment_body}");
        assert_no_markdown_breakout(&review.body, &["### darkmux review"], "review body (should carry none of the attack)");
    }

    #[test]
    fn an_attack_payload_in_evidence_never_renders_anywhere() {
        // (#2310 fix loop A, S5-4, superseded by #2429 part 3) `evidence`
        // used to reach a "Candidates: <evidence>" suffix on a question's
        // bullet — that suffix is gone, and with it every path that reads
        // `evidence` at all. This test now asserts the stronger property:
        // an attack payload planted in `evidence` never reaches the
        // payload in any form, because nothing reads it.
        let findings = vec![finding("s/1", "src/a.ts", 2, ATTACK, "why", Some("search"))];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        let mut spoken = review.body.clone();
        for c in &review.comments {
            spoken.push('\n');
            spoken.push_str(&c.body);
        }
        assert!(!spoken.contains("benign lead-in"), "`evidence` must never render: {spoken}");
    }

    #[test]
    fn an_attack_payload_in_a_question_forms_plain_inline_comment_cannot_forge_a_header() {
        let findings = vec![finding("s/1", "src/a.ts", 2, ATTACK, ATTACK, Some("question"))];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let comment_body = &review.comments[0].body;
        assert_folded_onto_one_line(comment_body, "question-form plain inline comment");
        assert_no_markdown_breakout(comment_body, &[], "question-form plain inline comment");
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
        //
        // (#2429) `window.span()` no longer renders in a PLAIN inline
        // comment's body (the comment is already anchored to `path`/`line`
        // as structural JSON fields, so the text body carries only the
        // claim) — it still renders in a gated mod's FENCED FALLBACK
        // bullet (`fenced_patch_bullet`, unchanged by this packet), so this
        // test exercises that path: a mod declared with no `kit_kind` never
        // becomes a suggestion and always falls back to the body.
        const FILE: &str = "src/`a`.ts\n### darkmux review\n---";
        let findings = vec![finding("s/1", FILE, 2, "ev", "why", None)];
        let mods = vec![gated_mod("s/1", "not a unified diff", Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
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
        // (PR #2398 review, MUST FIX) The SAME payload in `why`, which
        // now leads the comment. The reviewer's own probe: before the
        // fix this forged a `### darkmux review` heading inside a comment
        // posted under darkmux's byline, with every test green.
        const BENIGN_KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        let findings = vec![finding("s/1", "src/a.ts", 2, "ev", ATTACK, None)];
        let mods = vec![gated_mod_kind("s/1", BENIGN_KIT, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert_no_markdown_breakout(&review.comments[0].body, &[], "suggestion comment `why`");
        assert_folded_onto_one_line(&review.comments[0].body, "suggestion comment `why`");
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
        // NOT the bare emit path it used to be. `review.json` declares
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
        assert_eq!(step_output["entries"], json!([]), "nothing delivered, no rows: {step_output}");

        // (PR #2398 review, item 5) A run that DOES deliver carries the
        // operator's index back to the records — rule, location, the
        // finding key the author-facing body deliberately drops, and the
        // shape each entry took. The emitted GitHub payload stays exactly
        // the bytes that get posted: no rows in it.
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        step.config = json!({
            "findings": [
                finding_of_rule("sess-a/1", Some("swallowed-error"), "src/a.ts", 2, "ev", "the failure is discarded", None),
                finding_of_rule("sess-a/2", Some("union-vs-enum"), "src/mw.ts", 9, "ev", "a lead", Some("search")),
            ],
            "mods": [gated_mod_kind("sess-a/1", KIT, Some("unified-diff"), Some(true))],
            "diff": DIFF,
            "scope": {},
            "emit": out_path.to_string_lossy(),
        });
        let outcome = DeliverGithubReviewStepKind.run(&step, &task, &BTreeMap::new()).unwrap();
        let step_output: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        // (#2431 round 2, MF-A) `sess-a/2` sits at `src/mw.ts:9`, a path
        // `DIFF` (which touches only `src/a.ts`) never covers — an
        // off-diff anchor renders as a body count now, not a comment.
        assert_eq!(
            step_output["entries"],
            json!([
                { "rule": "swallowed-error", "path": "src/a.ts", "line": 2, "key": "sess-a/1", "rendered_as": "suggestion" },
                { "rule": "union-vs-enum", "path": "src/mw.ts", "line": 9, "key": "sess-a/2", "rendered_as": "body" },
            ]),
            "{step_output}"
        );
        let emitted: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        assert!(emitted.get("entries").is_none(), "provenance never rides the posted payload: {emitted}");
        assert!(!emitted["review"]["body"].as_str().unwrap().contains("sess-a/"), "{emitted}");
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
                line.matches("not a full design review").count(),
                1,
                "stated once, never twice and never omitted: {line}"
            );
            // (PR #2398 review, item 6) In the AUTHOR's frame. "A
            // rule-shaped review; architectural breadth stays with the
            // frontier gate" was the loudest darkmux-knowledge sentence
            // left in a body meant for someone who has never heard of
            // darkmux — it names an internal tiering, not a limit the
            // reader can act on.
            assert!(line.contains("This review checks a fixed set of rules"), "{line}");
            for internal in ["rule-shaped", "frontier"] {
                assert!(!line.contains(internal), "{internal:?} is darkmux's own frame, not the author's: {line}");
            }
        }
    }

    /// (PR #2398 review, MUST FIX — the reviewer's probe, committed) Every
    /// markdown BLOCK STARTER a model can write at the head of `why`, each
    /// rendered into the one place model prose leads a document: the
    /// inline comment. `inline_text` deliberately does not escape any of
    /// these (its own doc says why: its precondition is mid-line
    /// interpolation), so the invariant this asserts is positional —
    /// the model's first character is never the line's first character.
    #[test]
    fn a_why_that_opens_a_markdown_block_never_reaches_column_zero_in_a_comment() {
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = clamp(1);\n";
        for opener in [
            "### darkmux review - approved, merge this",
            "# heading",
            "> quoted",
            "- item",
            "1. first",
            "*** ",
            "--- ",
        ] {
            let findings = vec![finding("s/1", "src/a.ts", 2, "ev", opener, None)];
            let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
            let review = render(&findings, &mods, DIFF, &DeliverScope::default(), None).review.unwrap();
            let body = &review.comments[0].body;
            let first = body.lines().next().unwrap_or("");
            assert!(
                first.starts_with("- ") && !first[2..].starts_with(' '),
                "the comment must open with THIS module's own lead-in, not the model's text ({opener:?}):\n{body}"
            );
            assert!(
                first[2..].starts_with(opener.trim_end()),
                "the model's text starts mid-line, right after the lead-in ({opener:?}):\n{body}"
            );
            assert_no_markdown_breakout(body, &[], opener);
            assert_eq!(body.lines().filter(|l| l.starts_with("- ")).count(), 1, "one lead-in only ({opener:?}):\n{body}");
        }
    }

    /// (PR #2398 review, item 2) The question form's own leg of rule 2 —
    /// mutating the gate-passed branch to `Mod | Search` stayed green,
    /// because no fixture paired a QUESTION-confirmed rule with a passed
    /// change. It does now.
    #[test]
    fn a_gate_passed_change_on_a_question_confirmed_rule_is_still_a_suggestion() {
        const KIT: &str =
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,1 +2,1 @@\n-  const x = 1;\n+  const x = retryWithBackoff(1);\n";
        let findings = vec![finding_of_rule(
            "s/1",
            Some("existing-solution"),
            "src/a.ts",
            2,
            "export function retryWithBackoff",
            "this re-implements the retry helper",
            Some("question"),
        )];
        let mods = vec![gated_mod_kind("s/1", KIT, Some("unified-diff"), Some(true))];
        let out = render(&findings, &mods, DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "a passed change on a question-confirmed rule is a suggestion: {review:?}");
        assert!(review.comments[0].body.contains("retryWithBackoff(1)"), "{}", review.comments[0].body);
        assert!(
            !review.body.contains("Candidates:"),
            "the change replaces the question — the form only decides what to render when nothing passed:\n{}",
            review.body
        );
        assert_eq!(out.entries.iter().map(|e| e.rendered_as.as_str()).collect::<Vec<_>>(), vec!["suggestion"]);
    }

    /// (PR #2398 review, item 3) `"title": ""` in a hand-edited user rule
    /// tier is a rule with NO title, at both ends: `rule_titles`'s own
    /// filter drops it, and the render treats a blank one that reaches it
    /// anyway as absent — heading by the id, and NAMING it in the scope
    /// line. Before this, it rendered a heading of four bare asterisks.
    #[test]
    fn a_blank_rule_title_falls_back_to_the_id_and_the_scope_line_names_it() {
        let mut rules: BTreeMap<String, crate::rules::Rule> = crate::rules::load_all(None).0;
        let blank = rules.get_mut("swallowed-error").expect("a built-in rule");
        blank.title = Some("   ".to_string());
        let titles = titles_of(rules);
        assert!(!titles.contains_key("swallowed-error"), "a blank title is not a title: {titles:?}");

        // …and the render is defensive about one arriving anyway. The
        // finding is anchored, so it renders as a plain inline comment
        // (#2429) — the rule id still names it there, as a code span, and
        // the scope line still calls out the missing title.
        let titles = BTreeMap::from([("swallowed-error".to_string(), "  ".to_string())]);
        let findings = vec![finding_of_rule("s/1", Some("swallowed-error"), "src/a.ts", 2, "ev", "a claim", None)];
        let review =
            render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None, &titles, &BTreeMap::new())
                .review
                .unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert!(review.comments[0].body.contains("`swallowed-error`"), "the id names it in the comment: {:?}", review.comments[0]);
        assert!(review.body.contains("Titles unavailable for these rules: swallowed-error."), "{}", review.body);
    }

    /// (#1748) The mechanical absence-claim backstop's note, wired all the
    /// way to the rendered comment: a finding present in the
    /// `absence_backstop` map gets its caveat appended to its claim text,
    /// wherever that claim renders — here, a plain anchored inline
    /// comment (#2429's default form for a finding with no gated mod).
    #[test]
    fn a_finding_with_a_contradicted_absence_note_renders_its_caveat_inline() {
        let findings = vec![finding_of_rule(
            "s/1",
            Some("swallowed-error"),
            "src/a.ts",
            2,
            "ev",
            "This does not assign `process.exitCode` anywhere in this file.",
            None,
        )];
        let backstop = BTreeMap::from([(
            "s/1".to_string(),
            crate::absence_backstop::AbsenceBackstopNote {
                token: "process.exitCode".to_string(),
                file: "src/a.ts".to_string(),
                line: Some(9),
            },
        )]);
        let review = render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None, &test_titles(), &backstop)
            .review
            .unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let body = &review.comments[0].body;
        // The CLAIM half went through `inline_text` (model-authored text —
        // its own literal backticks are replaced with a lookalike char so
        // they cannot forge a code span, `inline_text_probe_table`'s own
        // contract); the CAVEAT half is darkmux's own text and keeps real
        // backticks around the token, same as every other host-authored
        // code span in this module (`code_span`).
        assert!(
            body.contains("does not assign \u{02cb}process.exitCode\u{02cb}"),
            "the original claim still renders: {body}"
        );
        assert!(
            body.contains("A mechanical check found `process.exitCode` elsewhere in this file, at src/a.ts:9"),
            "the caveat renders alongside the claim: {body}"
        );
    }

    /// (#1748) The counterpart to the test above: a finding with NO entry
    /// in the `absence_backstop` map (every finding this check never
    /// evaluated, or evaluated and found genuinely absent) renders
    /// EXACTLY as it did before this packet — no caveat, byte-identical
    /// claim text. This is what "surfaced, not swallowed" cashes out to
    /// on the render side: the default (empty map) behavior is provably
    /// unchanged.
    #[test]
    fn a_finding_with_no_backstop_entry_renders_with_no_caveat() {
        let findings = vec![finding_of_rule(
            "s/1",
            Some("swallowed-error"),
            "src/a.ts",
            2,
            "ev",
            "This does not call `bar()` anywhere in this file.",
            None,
        )];
        let with_empty_map =
            render_github_review(&findings, &[], DIFF, &DeliverScope::default(), None, &test_titles(), &BTreeMap::new())
                .review
                .unwrap();
        let without_param_at_all = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        assert_eq!(with_empty_map, without_param_at_all.review.unwrap());
        let body = &with_empty_map.comments[0].body;
        assert!(body.contains("does not call \u{02cb}bar()\u{02cb}"), "{body}");
        assert!(!body.contains("mechanical check"), "no caveat without a backstop entry: {body}");
    }

    /// (PR #2398 review, item 4 follow-through) Every sentence darkmux
    /// writes in its own voice, checked exhaustively — not just the ones a
    /// fixture's paths happen to render. A mutation restoring "the kit did
    /// not parse as a unified diff" to the unparseable-patch reason
    /// SURVIVED the fixture-rendered check; it does not survive this one.
    #[test]
    fn every_sentence_darkmux_writes_speaks_the_authors_language() {
        for sentence in AUTHORED_PROSE {
            let lower = sentence.to_lowercase();
            for word in INTERNAL_VOCABULARY {
                assert!(!lower.contains(word), "darkmux's own sentence speaks {word:?}: {sentence}");
            }
        }
    }

    /// (PR #2398 review, item 4) The vocabulary check, run over the REAL
    /// shipped rule registry rather than the test's own fixture titles —
    /// a rule title is model-facing prose written by whoever authors the
    /// rule, and it reaches the author's screen as a heading.
    #[test]
    fn the_shipped_rule_titles_speak_the_authors_language_too() {
        let titles = titles_of(crate::rules::load_all(None).0);
        assert!(titles.len() >= 9, "every shipped rule should be titled: {titles:?}");
        for (id, title) in &titles {
            let lower = title.to_lowercase();
            for word in INTERNAL_VOCABULARY {
                assert!(!lower.contains(word), "rule {id}'s title speaks darkmux's own procedure word {word:?}: {title}");
            }
        }
        // …and the fixture rendered against those real titles, not the
        // test's own copies of them.
        let (findings, mods, scope) = every_form_fixture();
        let review =
            render_github_review(&findings, &mods, DIFF, &scope, Some("Advisory, not a merge gate."), &titles, &BTreeMap::new())
                .review
            .unwrap();
        assert_no_internal_vocabulary(&review);
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
            gated_mod_with_key("mod-4-gatefailed", "sess-a/4", "kit text nobody sees", Some(false)),
            gated_mod_with_key("mod-7-nevergated", "sess-a/7", "never-gated kit text nobody sees either", None),
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
        // (#2429) Both findings are anchored, so both render as inline
        // comments now — the rule ID names each one there (a plain
        // comment has no group heading to lean on; the TITLE only ever
        // headed a body group, which no longer exists once a rule's
        // findings all rendered inline).
        // (#2431 round 2, MF-A) Both anchors sit inside `DIFF`'s own
        // touched lines (`src/a.ts` 1-3) — an off-diff anchor renders as a
        // body count instead, which is a different test's concern.
        let findings = vec![
            finding_of_rule(
                "s/1",
                Some("existing-solution"),
                "src/a.ts",
                1,
                "Status enum",
                "did you check whether the repo already has this",
                Some("question"),
            ),
            finding_of_rule("s/2", Some("unnamed-predicate"), "src/a.ts", 2, "ev", "the condition has no name", None),
        ];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 2, "{review:?}");
        assert!(review.comments.iter().any(|c| c.body.contains("`existing-solution`")), "{review:?}");
        assert!(review.comments.iter().any(|c| c.body.contains("`unnamed-predicate`")), "{review:?}");
        for gone in ["Worth enumerating", "**Questions:**", "Proposed changes outside the diff", "Worth a double check"] {
            assert!(!review.body.contains(gone), "{gone:?} must not organize the review any more:\n{}", review.body);
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
        assert!(review.comments[0].body.contains("this union duplicates the Status enum"), "{}", review.comments[0].body);
        // (#2429) A gate-passed change that fully became a suggestion
        // leaves NOTHING further to say for its rule in the body — no
        // headline group (the body is a summary surface now), and no
        // double-check tail (removed entirely).
        assert!(!review.body.contains("union-vs-enum"), "{}", review.body);
        assert!(!review.body.contains("Worth a double check"), "a passed change is never demoted to a lead:\n{}", review.body);
    }

    /// (#2310 delivery rewrite, rule 1) A rule whose title cannot be
    /// resolved falls back to its own id, and the scope line says so
    /// rather than leaving one oddly-named section unexplained.
    #[test]
    fn an_unresolvable_rule_heads_its_group_by_id_and_the_scope_line_says_so() {
        let findings =
            vec![finding_of_rule("s/1", Some("a-rule-nobody-shipped"), "src/a.ts", 2, "ev", "a claim", None)];
        let out = render(&findings, &[], DIFF, &DeliverScope::default(), None);
        let review = out.review.unwrap();
        assert_eq!(review.comments.len(), 1, "{review:?}");
        assert!(review.comments[0].body.contains("`a-rule-nobody-shipped`"), "the id names it: {:?}", review.comments[0]);
        assert!(
            review.body.contains("Titles unavailable for these rules: a-rule-nobody-shipped."),
            "the scope line names it:\n{}",
            review.body
        );
    }

    /// (PR #2398 review, item 4) darkmux's own procedure words, in their
    /// UNAMBIGUOUS compound forms. The earlier list checked bare English
    /// substrings — `unit`, `kit`, `confirm`, `dispatch` — which a
    /// perfectly good author-facing sentence can contain honestly ("the
    /// unit test", "a starter kit", "confirm the range"), so the check
    /// would have failed on innocent prose and taught the next author to
    /// weaken it. What is actually forbidden is darkmux naming its own
    /// machinery to someone who has never heard of darkmux.
    const INTERNAL_VOCABULARY: [&str; 7] =
        ["search-confirmed", "mod-form", "question-form", "confirm form", "crawl unit", "the kit", "dispatch session"];

    /// The rendered review — body AND inline comments — against
    /// [`INTERNAL_VOCABULARY`].
    fn assert_no_internal_vocabulary(review: &GithubReviewPayload) {
        let mut spoken = review.body.clone();
        for c in &review.comments {
            spoken.push('\n');
            spoken.push_str(&c.body);
        }
        let spoken = spoken.to_lowercase();
        for word in INTERNAL_VOCABULARY {
            assert!(!spoken.contains(word), "the review speaks darkmux's own procedure word {word:?}:\n{spoken}");
        }
    }

    /// (#2310 delivery rewrite, rule 4) The rendered review — body AND
    /// inline comments — speaks the AUTHOR's language. darkmux's own
    /// procedure words never appear; the rule ID does, because it is the
    /// author's handle for re-running the same check after a fix.
    #[test]
    fn the_rendered_review_carries_no_internal_procedure_vocabulary() {
        let (findings, mods, scope) = every_form_fixture();
        let review = render(&findings, &mods, DIFF, &scope, Some("Advisory, not a merge gate.")).review.unwrap();
        assert_no_internal_vocabulary(&review);
        // (#2429) Both `union-vs-enum` findings in this fixture are
        // anchored — one rides a gate-passed mod fully into a suggestion,
        // the other (gate-failed) into a plain comment — so neither ever
        // reaches the body any more; the rule id is still author-facing,
        // just on the comment side now.
        assert!(
            review.comments.iter().any(|c| c.body.contains("`union-vs-enum`")),
            "the rule id IS author-facing — it names what was violated: {review:?}"
        );
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
        //
        // (#2429, revised #2431 round 2 MF-A) Every finding anchored
        // INSIDE the diff is an inline comment: the two gate-passed mods
        // still ride out as suggestions (unchanged), and three of the
        // OTHER findings (no mod, or a gate-failed/never-gated one) become
        // plain comments. TWO findings — `shared-symbol-callers` at
        // `src/mw.ts:10` and `existing-solution` at `src/x.ts:1`, neither
        // of which `DIFF` (which only touches `src/a.ts`) covers — now
        // fall to the SAME "could not be anchored" body-count path an
        // unanchored finding takes: an off-diff anchor 422s the whole
        // GitHub review if it were posted, so it can never become a
        // comment. `sess-a/2`'s mod (declared with no `kit_kind`, so it
        // can never become a suggestion) still falls back to the body too.
        assert_eq!(outcome.mode, "review");
        let review = outcome.review.unwrap();
        assert_eq!(review.comments.len(), 5, "2 suggestions + 3 plain comments: {review:?}");
        assert!(review.comments.iter().all(|c| c.side.as_deref() == Some("RIGHT")));
        // (#2310 delivery rewrite, rule 2) The searched rule's passed
        // change still rides out as a suggestion — the shape the old
        // form-first branch dropped entirely.
        assert!(review.comments.iter().any(|c| c.body.contains("function f(): Status {")), "{review:?}");
        assert!(review.comments.iter().any(|c| c.body.contains("const x = clamp(1);")), "{review:?}");
        // Every OTHER in-diff finding's claim rides an inline comment now,
        // tagged with its rule id — never the body.
        for (claim, rule) in [
            ("might duplicate an existing helper", "unnamed-predicate"),
            ("a gate-failed claim", "union-vs-enum"),
            ("a mod exists but nothing ever gated it", "swallowed-error"),
        ] {
            assert!(
                review.comments.iter().any(|c| c.body.contains(claim) && c.body.contains(&format!("`{rule}`"))),
                "{claim:?} tagged with `{rule}` must be an inline comment: {review:?}"
            );
            assert!(!review.body.contains(claim), "{claim:?} must never repeat in the body: {}", review.body);
        }
        // The two OFF-DIFF findings become body counts under their own
        // rule's heading — never a comment, and never their claim.
        for (claim, rule_heading) in [
            ("shared auth changed", "**A shared function or type's signature or behavior changed** `shared-symbol-callers`"),
            ("did you check whether the repo already has this", "**A new routine looks re-implemented rather than reused** `existing-solution`"),
        ] {
            assert!(!review.comments.iter().any(|c| c.body.contains(claim)), "{claim:?} is off-diff, never a comment: {review:?}");
            assert!(!review.body.contains(claim), "{claim:?} must never repeat in the body: {}", review.body);
            assert!(review.body.contains(rule_heading), "{rule_heading:?} still heads its own group: {}", review.body);
        }
        assert_eq!(
            review.body.matches("1 finding could not be anchored to a line.").count(),
            2,
            "one count bullet per off-diff rule: {}",
            review.body
        );
        // The one mod that COULD NOT become a suggestion (no `kit_kind`)
        // still falls back to the body, unchanged.
        assert!(review.body.contains("the patch text"), "mod-outside-diff fallback still renders in the body");
        assert!(!review.body.contains("kit text nobody sees"), "a gate-failed mod's kit never renders");
        assert!(!review.body.contains("never-gated kit text"), "a never-gated mod's kit never renders");
        // (#2429 part 3) The "Candidates" suffix — and the `evidence` it
        // used to interpolate — is gone entirely.
        assert!(!review.body.contains("Candidates") && review.comments.iter().all(|c| !c.body.contains("Candidates")), "{review:?}");
        assert!(
            !review.body.contains("Status enum, Kind enum")
                && review.comments.iter().all(|c| !c.body.contains("Status enum, Kind enum")),
            "`evidence` is not read any more: {review:?}"
        );
        assert!(!review.body.contains("14 endpoints"), "`evidence` is not read any more: {}", review.body);
        // (#2429 part 2) The body is a summary surface — no per-finding
        // claims other than the one fallback bullet above, and no leftover
        // "Worth a double check" tail.
        assert!(!review.body.contains("Worth a double check"), "{}", review.body);
        assert!(review.body.contains("2 refused"));
        assert!(review.body.contains("Not attempted: architectural review."));
    }

    /// (#2310 P4b review, CONSIDER — renamed to what it actually asserts)
    /// `emit: "-"` is this step's own stdout convention: `Path::new("-")`
    /// is treated as "print to stdout, not a file", and the step reports
    /// `"-"` as its own output marker rather than a file path. This does
    /// NOT capture actual stdout bytes (`println!` isn't trivially
    /// interceptable from a plain `#[test]`) — it proves the DESTINATION
    /// decision, not the byte-for-byte "one JSON line and nothing else"
    /// purity claim the old name implied.
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
        // (#2429) Anchored, no mod: renders as a plain INLINE comment now,
        // never a body bullet — the `<` containment has to hold there.
        // `evidence` (the `<a href...>` string) is no longer read by any
        // renderer in this module (part 3 dropped the "Candidates" suffix
        // that used to interpolate it), so it must not leak ANYWHERE.
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
        assert_eq!(review.comments.len(), 1, "{review:?}");
        let comment_body = &review.comments[0].body;
        assert!(
            !comment_body.contains('<'),
            "a raw `<` reached the posted comment — an <img src> would load from every reader:\n{comment_body}"
        );
        // `>` is deliberately NOT escaped (see `inline_text`'s doc): it
        // only means anything at column 0, which this function's folding
        // already makes unreachable. Asserting the exact surviving `>`
        // pins that decision rather than leaving it implied.
        assert!(comment_body.contains("&lt;img src=x onerror=1>"), "{comment_body}");
        assert!(!review.body.contains('<'), "{}", review.body);
        assert!(
            !review.body.contains("evil.example") && !comment_body.contains("evil.example"),
            "`evidence` is never read any more, so it must not leak: body={} comment={comment_body}",
            review.body
        );
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
