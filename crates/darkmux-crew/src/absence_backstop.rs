//! (#1748) The mechanical absence-claim backstop: a cheap, zero-token,
//! deterministic check run against a finding's own "X is missing" / "X is
//! never called" / "X is not handled" claim, before that claim reaches a
//! reader as a plain, unqualified assertion.
//!
//! **Origin.** A `confirmed` PR-review finding once claimed a line of code
//! was ABSENT ("does not assign `process.exitCode`", "there is no
//! `.catch`") when both were present in the file — the reviewing seat had
//! been shown a truncated excerpt and reported honestly about ITS window;
//! the pipeline then promoted that into a claim about the WHOLE file. This
//! module is the mechanical check that would have caught it: an earlier
//! version of it (`AbsenceBackstopNote`, `apply_absence_backstop`) shipped
//! against the bespoke review funnel and was deleted along with that
//! funnel in #2310 P4d — this is a from-scratch reimplementation against
//! the funnel's replacement, the `plan.sites` + `crawl.unit` +
//! `records.gather` + `deliver.github_review` mission-config pipeline.
//!
//! **What this is: a text-search LINT, not a registry walk.** State this
//! plainly because it bounds what the check can promise. It runs two
//! purely mechanical passes over strings, never a language parser, never
//! an AST, never a symbol table, never a call graph:
//!
//!   1. [`detect_absence_claim`] — a keyword/phrase match over the
//!      finding's OWN claim text (`emitted.why`), looking for a
//!      recognized absence phrasing ("does not", "never calls",
//!      "missing", "not handled", "there is no", …) plus a
//!      backtick-quoted token somewhere in the same sentence — the thing
//!      the claim says is absent.
//!   2. [`check_absence_claim`] — a plain substring/line search for that
//!      token over the WHOLE FILE's text, not the hunk/window excerpt a
//!      reviewing seat happened to be shown.
//!
//! **What it catches.** A claim phrased as a single backtick-quoted
//! identifier/call/member-access span ("does not call `foo()`", "there is
//! no `.catch`", "`processExit` is never invoked", "missing
//! `import Bar`") whose named token DOES appear elsewhere in the same
//! file — exactly the #1748 failure shape (`process.exitCode`/`.catch`
//! both present, the finding claimed neither existed).
//!
//! **What it misses, by construction — do not overstate this check:**
//!   - a claim with no backtick-quoted token at all ("the error path is
//!     never handled") — nothing to search for, so it abstains.
//!   - a token that exists elsewhere under a DIFFERENT spelling (renamed,
//!     aliased, destructured, produced by a macro/template expansion) — a
//!     substring search cannot see through that.
//!   - a token quoted as a whole clause or sentence rather than a single
//!     identifier/call span — `looks_like_identifier_span` (private —
//!     see this module's own source) refuses to search for those, on
//!     the theory that "searching" for a sentence as
//!     a literal substring produces noise, not signal.
//!   - a token that genuinely IS absent from the reviewed code but
//!     appears in a COMMENT or a STRING LITERAL discussing it
//!     (`// TODO: call foo()`) — the search is textual, not semantic, so
//!     this can FALSE-POSITIVE a contradiction (flag a claim that is
//!     actually correct). This is the known cost of a lint over a
//!     registry: it is why a contradiction only ever DEMOTES/ANNOTATES a
//!     finding (see [`AbsenceBackstopNote`]) rather than deleting or
//!     silently "correcting" it — a human still makes the final call.
//!   - a token that genuinely IS absent as a CALL but appears as a
//!     SUBSTRING of an unrelated, longer identifier or word — "does not
//!     call `run` anywhere" false-positives against
//!     `"// we should probably rerun this later"` (`run` is a substring
//!     of `rerun`) just as readily as against a real call. [`MIN_TOKEN_LEN`]
//!     bounds how SHORT a token this lint will search for; it does not
//!     bound what surrounds a match, so a short, common identifier is
//!     exactly the shape most exposed to this (#1748 review CONSIDER 5,
//!     PROVEN, deliberately left unfixed rather than adding a
//!     word-boundary notion this module's plain substring/line search
//!     does not otherwise carry — the same FALSE-POSITIVE-only,
//!     demote-never-delete cost as the comment/string-literal case above).
//!   - a sentence carrying MORE THAN ONE backtick span in the direction
//!     the phrase requires ("does not call `foo()` or `bar()`") — the
//!     token extractor binds to the NEAREST one in that direction, which
//!     is a heuristic, not a parse; a model that lists two candidates and
//!     means the second one produces a miss, not a false flag (this
//!     lint's search still runs against the FIRST one, so at worst it
//!     is silent where a human would have caught both).
//!
//! A check that WALKS A REAL REGISTRY (a language server's symbol index,
//! an AST-level call graph) could rule the false-positive case above out
//! entirely and would be closer to actually ENDING this class of bug.
//! Nothing on this stack builds one; this is the cheap mechanical layer,
//! not that.
//!
//! **Abstention is not silence.** When the claim text yields no
//! extractable, identifier-shaped token, or the whole file cannot be
//! resolved/read, [`check_absence_claim`] /
//! [`check_absence_claim_against_file`] return
//! [`AbsenceCheckOutcome::Inconclusive`], and every caller in this module
//! (see [`run_backstop`]) leaves that finding COMPLETELY UNCHANGED — no
//! note, no demotion, still delivered exactly as the model wrote it. A
//! finding this check cannot evaluate is surfaced untouched, never
//! dropped; see [`run_backstop`]'s own doc for the same rule stated at
//! the pipeline-wiring level.

use crate::findings::FindingRecord;
use crate::step_kinds::rule_id_of;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Below this length a token is too likely to be a common substring
/// (`id`, `Ok`, `run`) for a bare `contains()` hit to mean anything.
pub const MIN_TOKEN_LEN: usize = 3;

/// One phrase this lint recognizes as claiming something is absent.
/// Lowercase; matched against a lowercased claim. Deliberately a flat
/// list of common English absence idioms rather than a grammar — see this
/// module's own doc on what that costs.
const ABSENCE_PHRASES: &[&str] = &[
    "does not call",
    "does not invoke",
    "does not use",
    "does not handle",
    "does not assign",
    "does not set",
    "does not check",
    "doesn't call",
    "doesn't invoke",
    "doesn't handle",
    "doesn't assign",
    "never calls",
    "never invokes",
    "never assigns",
    "never sets",
    "never checks",
    "never handles",
    "is never called",
    "is never invoked",
    "is never assigned",
    "is never set",
    "not handled",
    "not called",
    "not invoked",
    "not assigned",
    "not present",
    "not implemented",
    "not defined",
    "not found",
    "isn't handled",
    "isn't called",
    "isn't invoked",
    "no call to",
    "no calls to",
    "no handling",
    "there is no",
    "there's no",
    "there is not",
    "missing a call to",
    "missing",
    "lacks",
    "is absent",
];

/// The outcome of running the backstop against ONE finding's claim text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsenceCheckOutcome {
    /// The claim text matched no recognized absence phrasing, OR matched
    /// one but no identifier-shaped, backtick-quoted token could be
    /// extracted near it. The finding is left untouched either way — this
    /// is an ABSTENTION, not agreement or disagreement with the claim.
    Inconclusive,
    /// The claim named `token`, and `token` genuinely does not appear
    /// anywhere in the whole file this backstop checked — the claim is
    /// consistent with the WHOLE file, not just whatever excerpt the
    /// reviewing seat saw. Left untouched; this variant exists so a
    /// caller/test can tell "checked and agreed" from "never checked".
    Confirmed { token: String },
    /// The claim named `token`, and `token` DOES appear in the whole
    /// file — outside whatever window the model was shown. The finding
    /// should be flagged, never silently confirmed as correct. `line` is
    /// `Some` in every case this module's own constructor ever produces
    /// (`check_token_against_whole_file`'s doc, private — see this
    /// module's own source — explains why a `token`
    /// can never straddle a line boundary); it stays `Option` so a caller
    /// building one by hand (a test fixture, say) is not forced to invent
    /// a line number it doesn't have.
    Contradicted { token: String, line: Option<u32> },
}

/// Recognized absence phrases whose natural English grammar puts the
/// absent token BEFORE the phrase — a passive construction ("`X` is
/// never called") where the subject precedes the passive verb phrase.
/// Every other phrase in [`ABSENCE_PHRASES`] is active voice, and active
/// voice puts its object AFTER the verb ("does not call `X`", "does not
/// handle `X`") — see [`detect_absence_claim`]'s own doc for why that
/// direction is not optional.
const BEFORE_PHRASE_TOKEN: &[&str] =
    &["is never called", "is never invoked", "is never assigned", "is never set", "isn't handled", "isn't called", "isn't invoked"];

/// Extract the claimed-absent token from a finding's own claim text.
///
/// Looks for a recognized absence PHRASE (case-insensitive) anywhere in
/// the text, then extracts the backtick-quoted span GOVERNED by that
/// phrase, in the direction its grammar actually puts it: after the
/// phrase for an active-voice claim ("does not call `X`", "does not
/// handle `X`" — `X` is the verb's OBJECT, the thing claimed absent), or
/// before it for the passive-voice claims in `BEFORE_PHRASE_TOKEN`
/// (private — see this module's own source)
/// ("`X` is never called" — `X` is the verb's SUBJECT). When no span
/// exists in the required direction, this returns `None` rather than
/// falling back to a wrong-direction span — falling back is exactly the
/// #1748-review MUST-FIX-4 bug this function used to have: given "`X`
/// does not call `Y`" it took the FIRST backtick span in the whole text
/// regardless of direction, which is `X`, the phrase's SUBJECT. `X` is a
/// symbol in the file under review and is present BY CONSTRUCTION, so
/// binding to it turned every correctly-phrased active-voice finding into
/// a caveat whose justification is a non-sequitur ("a mechanical check
/// found `X` elsewhere in this file" — of course it did, `X` is what the
/// finding is ABOUT). Worse: an active-voice phrase with NO span after it
/// at all ("the `fetchUser` helper does not handle the 404 case" — the
/// real claimed-absent thing, "the 404 case", carries no backtick token)
/// used to fall back to the only span it could find, `fetchUser`, same
/// failure. `None` also when no phrase matches, when the direction-
/// correct span is empty, or when it does not look like a single
/// identifier/call/member-access (`looks_like_identifier_span`, private —
/// see this module's own source).
pub fn detect_absence_claim(claim: &str) -> Option<String> {
    let lower = claim.to_ascii_lowercase();
    let phrase = ABSENCE_PHRASES.iter().find(|p| lower.contains(**p))?;
    let phrase_start = lower.find(*phrase)?;
    let phrase_end = phrase_start + phrase.len();
    let expects_before = BEFORE_PHRASE_TOKEN.contains(phrase);
    let token = extract_directional_token(claim, phrase_start, phrase_end, expects_before)?;
    if token.chars().count() < MIN_TOKEN_LEN || !looks_like_identifier_span(&token) {
        return None;
    }
    Some(token)
}

/// Every backtick-delimited span in `text`, left to right, as
/// `(open_backtick_byte_offset, byte_offset_just_past_the_close_backtick,
/// trimmed_content)`. An unterminated trailing backtick (an odd count)
/// yields no span for it — there is nothing to pair it with.
fn backtick_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'`' {
            match text[idx + 1..].find('`') {
                Some(rel_end) => {
                    let content_start = idx + 1;
                    let content_end = content_start + rel_end;
                    spans.push((idx, content_end + 1, text[content_start..content_end].trim()));
                    idx = content_end + 1;
                }
                None => break,
            }
        } else {
            idx += 1;
        }
    }
    spans
}

/// The backtick span nearest `[phrase_start, phrase_end)` in the required
/// direction: the LAST span ending at or before `phrase_start` when
/// `expects_before`, or the FIRST span starting at or after `phrase_end`
/// otherwise. `None` when no span exists in that direction, or the
/// nearest one is empty after trimming.
fn extract_directional_token(claim: &str, phrase_start: usize, phrase_end: usize, expects_before: bool) -> Option<String> {
    let spans = backtick_spans(claim);
    let content = if expects_before {
        spans.into_iter().rfind(|(_, end, _)| *end <= phrase_start).map(|(_, _, c)| c)
    } else {
        spans.into_iter().find(|(start, _, _)| *start >= phrase_end).map(|(_, _, c)| c)
    }?;
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Whether `token` reads as a single identifier/call/member-access span
/// (`foo`, `foo()`, `a.b.c`, `.catch`, `process.exitCode`) rather than a
/// whole clause or sentence a model over-quoted ("the retry loop never
/// backs off"). A search for the latter as a literal substring would
/// almost never hit (prose is rarely repeated verbatim) and would waste
/// the check's one signal on noise, so this bounds what
/// [`detect_absence_claim`] will hand to the whole-file search at all.
fn looks_like_identifier_span(token: &str) -> bool {
    !token.is_empty()
        && !token.contains(' ')
        && !token.contains('\n')
        && token.chars().all(|c| c.is_alphanumeric() || "._$()[]->:<>!#".contains(c))
}

/// Run the backstop: detect an absence claim in `claim_text`, and if one
/// is found, check its token against `whole_file` (the FULL file's text,
/// never a hunk/window excerpt) via a plain substring/line search.
pub fn check_absence_claim(claim_text: &str, whole_file: &str) -> AbsenceCheckOutcome {
    let Some(token) = detect_absence_claim(claim_text) else {
        return AbsenceCheckOutcome::Inconclusive;
    };
    check_token_against_whole_file(token, whole_file)
}

/// The second half of [`check_absence_claim`], split out so
/// [`check_absence_claim_against_file`] can detect the claim BEFORE
/// touching disk and hand the already-extracted token straight in,
/// without re-running the phrase/token extraction a second time.
///
/// (review CONSIDER 6) `find_line` alone is the whole check, and that is
/// not an approximation: [`looks_like_identifier_span`] refuses any
/// token containing `\n`, so a `token` that appears ANYWHERE in
/// `whole_file` necessarily appears within the span of some single line
/// `.lines()` produces — it cannot straddle a line boundary without
/// containing the newline that boundary is made of. A second
/// `whole_file.contains(&token)` fallback here (once present, to catch a
/// "wrap" this reasoning shows can't occur) was therefore dead code that
/// could never run, and its only two tests never actually reached it —
/// removed along with `AbsenceCheckOutcome::Contradicted`'s now-unused
/// `line: None` construction site.
fn check_token_against_whole_file(token: String, whole_file: &str) -> AbsenceCheckOutcome {
    match find_line(whole_file, &token) {
        Some(line) => AbsenceCheckOutcome::Contradicted { token, line: Some(line) },
        None => AbsenceCheckOutcome::Confirmed { token },
    }
}

fn find_line(whole_file: &str, token: &str) -> Option<u32> {
    whole_file.lines().enumerate().find(|(_, line)| line.contains(token)).map(|(idx, _)| idx as u32 + 1)
}

/// Upper bound on how much of a candidate file this lint will read.
///
/// `file` is a MODEL-authored, unvalidated string
/// ([`run_backstop`]'s own doc) — on a public-PR review it is exactly as
/// attacker-influenced as any other finding field. Before this bound
/// existed, a `file` naming a device node (`/dev/zero`) made
/// [`read_bounded`]'s predecessor (a bare `std::fs::read_to_string`) read
/// forever, growing memory without limit and wedging the host process
/// this check runs on (never a container — see this module's own doc on
/// what it is). 4 MiB comfortably covers any real source file a review
/// would ever check; a candidate this large is itself a signal the lint
/// should abstain on, not a limit worth raising.
const MAX_FILE_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Resolve `file` against `tree_root`, refusing to leave the tree.
///
/// `file` is model-authored and unvalidated — `findings.rs` maps only a
/// `/workspace/<source>/` or `<source>/` prefix and otherwise leaves it
/// exactly as the model wrote it. Two failure shapes this closes:
///
/// - **An absolute `file` used to escape the tree entirely.**
///   `Path::join` with an absolute right-hand side discards the base
///   path, so `tree_root.join("/etc/passwd")` was `/etc/passwd`, not
///   `tree_root/etc/passwd`. `file` is rejected outright when it is
///   absolute, or carries a `..` component (`"../../outside.env"`),
///   before any join happens.
/// - **A relative path escaping via a symlink inside the tree.** The
///   join is canonicalized and the result is required to still sit under
///   `tree_root`'s own canonical form — a `..`-free relative path can
///   still walk out through a symlink, and string-level checks alone
///   cannot see that.
///
/// Returns `None` on any rejection, or on any canonicalization failure
/// (a nonexistent path, most commonly) — both read identically to the
/// caller as "cannot evaluate this finding mechanically", the same
/// abstention contract every other failure mode in this module already
/// has.
fn resolve_within_tree(tree_root: &Path, file: &str) -> Option<PathBuf> {
    let rel = Path::new(file);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let canon_root = tree_root.canonicalize().ok()?;
    let canon_joined = tree_root.join(rel).canonicalize().ok()?;
    if canon_joined.starts_with(&canon_root) {
        Some(canon_joined)
    } else {
        None
    }
}

/// Read at most [`MAX_FILE_READ_BYTES`] of `path` as UTF-8.
///
/// `None` on any I/O failure OR when the byte cap lands mid-codepoint (a
/// truncated multi-byte UTF-8 sequence at the boundary) — both collapse to
/// the same "abstain" outcome the caller already applies to every other
/// unreadable-file case, never an error.
fn read_bounded(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    file.take(MAX_FILE_READ_BYTES).read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// [`check_absence_claim`], reading the whole file itself.
///
/// **Detects before it reads.** `detect_absence_claim` runs FIRST, on
/// `claim_text` alone (no disk access) — a finding whose `why` carries no
/// recognized absence phrasing never touches the filesystem at all, which
/// both saves the read for the common case and caps the blast radius of
/// `resolve_within_tree`/`read_bounded` (both private — see this
/// module's own source) to findings that could actually be flagged.
///
/// Returns [`AbsenceCheckOutcome::Inconclusive`] — never an error — when
/// no claim is detected, when `file` cannot be resolved inside
/// `tree_root` (`resolve_within_tree`), or when the file cannot be read
/// (moved, deleted, not valid UTF-8, too large — `read_bounded`): the
/// caller's rule is to leave a finding untouched on any outcome that
/// isn't `Contradicted`, so every one of these behaves exactly like "no
/// claim detected" from the caller's point of view. This is the ONE place
/// this module touches disk.
pub fn check_absence_claim_against_file(claim_text: &str, tree_root: &Path, file: &str) -> AbsenceCheckOutcome {
    let Some(token) = detect_absence_claim(claim_text) else {
        return AbsenceCheckOutcome::Inconclusive;
    };
    let Some(path) = resolve_within_tree(tree_root, file) else {
        return AbsenceCheckOutcome::Inconclusive;
    };
    let Some(whole_file) = read_bounded(&path) else {
        return AbsenceCheckOutcome::Inconclusive;
    };
    check_token_against_whole_file(token, &whole_file)
}

/// The mechanical backstop's per-finding outcome — present only for a
/// finding this check actually CONTRADICTED. Never constructed for
/// `Inconclusive`/`Confirmed`, so [`run_backstop`]'s returned map holds
/// exactly the findings a reader needs to see a caveat on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AbsenceBackstopNote {
    /// The token the finding claimed was absent.
    pub token: String,
    /// The repo-relative file the token was found in — the same `file`
    /// the finding itself named.
    pub file: String,
    /// 1-indexed line the token was found on, when it is confined to a
    /// single line. `None` when the token spans a line wrap — still a
    /// real contradiction, just without a precise line to cite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Resolve the absolute path to the checked-out source tree
/// `plan.sites`/`crawl.plan` wrote for `(mission_id, rule_id, source_id)`
/// — the SAME `plan/<rule>.json` file `records_gather::plan_totals`
/// already reads for its own coverage counting, read again here purely
/// for its `sources[].tree` (`darkmux_lab::crawl::plan::PlanSource`,
/// read as loose JSON rather than that typed struct because
/// `darkmux-crew` cannot depend on `darkmux-lab` — the same crate-
/// boundary reason `records_gather.rs`'s own module doc states for its
/// `scan_unit_and_plan_steps`).
///
/// Returns `None` on ANY failure to resolve (missing plan file, malformed
/// JSON, no matching source) — every such case reads identically to the
/// caller: "cannot evaluate this finding mechanically", never an error.
pub fn resolve_source_tree(mission_id: &str, rule_id: &str, source_id: &str) -> Option<PathBuf> {
    let path = crate::loader::missions_dir().join(mission_id).join("plan").join(format!("{rule_id}.json"));
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let sources = value.pointer("/body/sources")?.as_array()?;
    sources
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(source_id))
        .and_then(|s| s.get("tree").and_then(|v| v.as_str()))
        .map(PathBuf::from)
}

/// Run the mechanical absence backstop over one mission's findings,
/// returning one [`AbsenceBackstopNote`] per finding key whose claim was
/// CONTRADICTED by the whole file.
///
/// **Never mutates or drops a finding.** This function only ever ADDS
/// entries to the returned map; every finding this check cannot evaluate
/// — no `why` text, no resolvable rule/source on its `context`, no
/// resolvable source tree, an unreadable file, or a claim/whole-file pair
/// this module's lint calls `Inconclusive`/`Confirmed` — is simply absent
/// from the map, which is exactly [`AbsenceCheckOutcome::Inconclusive`]/
/// `Confirmed`'s own contract: the finding is surfaced downstream
/// unchanged, never swallowed. The caller ([`crate::step_kinds::
/// records_gather`]) is what threads this map onward to the render step,
/// which likewise only ever ANNOTATES a finding's rendered claim (see
/// `deliver_github_review`'s `FindingWindow::absence_backstop`) — the
/// finding itself, and the count of findings delivered, are never
/// affected by what this function returns.
pub fn run_backstop(mission_id: &str, findings: &[FindingRecord]) -> BTreeMap<String, AbsenceBackstopNote> {
    let mut notes = BTreeMap::new();
    for finding in findings {
        let Some(claim) = finding.emitted.get("why").and_then(|v| v.as_str()) else { continue };
        // Detect before resolving anything — a finding with no absence
        // claim at all never pays for a plan-JSON read or a source-tree
        // resolution. `check_absence_claim_against_file` re-detects (it
        // has no way to receive an already-extracted token across this
        // boundary without a bigger signature change), which is a second
        // cheap string scan, not a second disk read.
        if detect_absence_claim(claim).is_none() {
            continue;
        }
        let Some(rule) = rule_id_of(finding) else { continue };
        let Some(source_id) = finding.source.as_deref() else { continue };
        let Some(file) = finding.emitted.get("file").and_then(|v| v.as_str()) else { continue };
        let Some(tree) = resolve_source_tree(mission_id, &rule, source_id) else { continue };
        if let AbsenceCheckOutcome::Contradicted { token, line } =
            check_absence_claim_against_file(claim, &tree, file)
        {
            notes.insert(finding.key.clone(), AbsenceBackstopNote { token, file: file.to_string(), line });
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_absence_claim / check_absence_claim: the pure lint ──────

    #[test]
    fn detects_a_backtick_token_after_an_absence_phrase() {
        let claim = "This function does not call `foo()` anywhere in this file.";
        assert_eq!(detect_absence_claim(claim), Some("foo()".to_string()));
    }

    #[test]
    fn detects_a_backtick_token_before_an_absence_phrase() {
        let claim = "`process.exitCode` is never assigned on this path.";
        assert_eq!(detect_absence_claim(claim), Some("process.exitCode".to_string()));
    }

    #[test]
    fn abstains_when_no_absence_phrase_is_present() {
        // Mentions a backtick token, but claims nothing is missing.
        assert_eq!(detect_absence_claim("This calls `foo()` twice."), None);
    }

    #[test]
    fn abstains_when_an_absence_phrase_has_no_quoted_token() {
        assert_eq!(detect_absence_claim("The error path is never handled."), None);
    }

    #[test]
    fn abstains_when_the_quoted_span_is_a_whole_sentence_not_a_token() {
        // A model over-quoting a clause rather than a single symbol —
        // searching for this as a literal substring would be noise.
        let claim = "There is no `handling of the retry budget once it runs out` in this file.";
        assert_eq!(detect_absence_claim(claim), None);
    }

    #[test]
    fn abstains_when_the_quoted_token_is_too_short() {
        assert_eq!(detect_absence_claim("There is no `.` in this file."), None);
    }

    // ── RED-PROVE, review MUST FIX 4: binds to the phrase's OBJECT, ────
    // never its SUBJECT. Both examples are the reviewer's own — the
    // failure mode was `detect_absence_claim` taking the FIRST backtick
    // span in the whole text, which for "X does not call Y" is X, the
    // subject: a symbol in the file under review, present BY
    // CONSTRUCTION, so binding to it produced a caveat whose
    // justification is a non-sequitur on every correctly-phrased finding
    // of this shape.

    #[test]
    fn binds_to_the_span_after_an_active_voice_phrase_not_the_subject_before_it() {
        let claim = "`handleRequest` does not call `cleanup()` before returning.";
        assert_eq!(detect_absence_claim(claim), Some("cleanup()".to_string()));
    }

    #[test]
    fn abstains_when_the_only_backtick_span_is_the_subject_not_the_object() {
        // The claimed-absent thing ("the 404 case") carries no backtick
        // token at all — the only span in the text, `fetchUser`, is the
        // SUBJECT of "does not handle", not its object. Binding to it
        // (the pre-fix behavior) would flag every correct finding of this
        // shape with an irrelevant fact.
        let claim = "The `fetchUser` helper does not handle the 404 case.";
        assert_eq!(detect_absence_claim(claim), None);
    }

    #[test]
    fn a_genuine_active_voice_absence_claim_still_contradicts_end_to_end() {
        // The property the fix above must not regress: a real,
        // correctly-directional single-span claim whose object token
        // actually exists elsewhere in the file must still contradict.
        let claim = "`handleRequest` does not call `cleanup()` before returning.";
        let whole_file = "function handleRequest() {}\nfunction cleanup() { teardown(); }\n";
        let outcome = check_absence_claim(claim, whole_file);
        assert_eq!(outcome, AbsenceCheckOutcome::Contradicted { token: "cleanup()".to_string(), line: Some(2) });
    }

    // ── RED-PROVE, direction (a): a genuinely-present claim gets caught ─
    // ("a finding claiming absence of something that DOES exist elsewhere
    // in the file gets demoted/flagged")

    #[test]
    fn contradicts_a_false_absence_claim_found_elsewhere_in_the_file() {
        let claim = "This script does not assign `process.exitCode` anywhere, so a failure exits 0.";
        let whole_file = "function main() {\n  doWork();\n}\n\nmain().catch((e) => {\n  process.exitCode = 1;\n});\n";
        let outcome = check_absence_claim(claim, whole_file);
        assert_eq!(outcome, AbsenceCheckOutcome::Contradicted { token: "process.exitCode".to_string(), line: Some(6) });
    }

    #[test]
    fn contradicts_a_second_false_absence_claim_in_the_same_shape_the_issue_reported() {
        let claim = "There is no `.catch` on the promise chain, so a rejection is unhandled.";
        let whole_file = "main()\n  .then(() => process.exit(0))\n  .catch((e) => {\n    console.error(e);\n  });\n";
        let outcome = check_absence_claim(claim, whole_file);
        assert_eq!(outcome, AbsenceCheckOutcome::Contradicted { token: ".catch".to_string(), line: Some(3) });
    }

    // ── RED-PROVE, direction (b): a genuinely-absent claim survives ────
    // ("a finding claiming absence of something genuinely absent survives
    // untouched")

    #[test]
    fn confirms_a_true_absence_claim_and_does_not_flag_it() {
        let claim = "This script does not call `bar()` anywhere in this file.";
        let whole_file = "function main() {\n  doWork();\n}\n\nmain().catch((e) => {\n  process.exitCode = 1;\n});\n";
        let outcome = check_absence_claim(claim, whole_file);
        assert_eq!(outcome, AbsenceCheckOutcome::Confirmed { token: "bar()".to_string() });
    }

    /// (review CONSIDER 6) A token genuinely SPLIT by a line wrap — here,
    /// a string concatenation puts half the identifier on each of two
    /// lines — is not a contiguous substring of `whole_file` at all,
    /// wrap or no wrap: `check_token_against_whole_file`'s doc explains
    /// why `Contradicted { line: None }` cannot be reached by this
    /// module's own constructor. This is the CONFIRMED path (the token
    /// genuinely does not appear), not a wrap-hit — a prior version of
    /// this test claimed the opposite in its name; renamed to match what
    /// it actually asserts.
    #[test]
    fn a_token_split_across_a_string_concatenation_is_genuinely_absent_and_confirms() {
        let claim = "There is no `longToken` anywhere in this file.";
        let whole_file = "const x = \"long\" +\n  \"Token\";\n";
        assert_eq!(check_absence_claim(claim, whole_file), AbsenceCheckOutcome::Confirmed { token: "longToken".to_string() });
    }

    /// (review CONSIDER 6) A token repeated within one physical line, and
    /// again later in the same line — `find_line`'s per-line `.contains`
    /// finds it on the FIRST line it ever appears on, citing that line
    /// number; there is no "wrap" case to reach here (a prior version of
    /// this test's comment claimed one, incorrectly — a token can never
    /// contain `\n`, so it can never straddle a line boundary; see
    /// `check_token_against_whole_file`'s doc).
    #[test]
    fn a_token_repeated_on_one_line_contradicts_and_cites_that_line() {
        let claim = "There is no `shared_helper` anywhere in this file.";
        let whole_file = "one line with shared_helper embedded, repeated on the same line: shared_helper";
        let outcome = check_absence_claim(claim, whole_file);
        assert_eq!(outcome, AbsenceCheckOutcome::Contradicted { token: "shared_helper".to_string(), line: Some(1) });
    }

    // ── Abstention (the "surfaced, not swallowed" requirement) ─────────

    #[test]
    fn inconclusive_when_no_token_is_extractable_even_over_a_matching_file() {
        let claim = "The retry logic is never handled correctly in this module.";
        let whole_file = "function retry() { return handled(); }\n";
        assert_eq!(check_absence_claim(claim, whole_file), AbsenceCheckOutcome::Inconclusive);
    }

    #[test]
    fn check_absence_claim_against_file_is_inconclusive_on_an_unreadable_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = check_absence_claim_against_file(
            "This does not call `foo()` anywhere.",
            tmp.path(),
            "does/not/exist.ts",
        );
        assert_eq!(outcome, AbsenceCheckOutcome::Inconclusive);
    }

    #[test]
    fn check_absence_claim_against_file_reads_the_real_file_and_contradicts() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ts"), "export function foo() { return 1; }\n").unwrap();
        let outcome =
            check_absence_claim_against_file("This does not call `foo()` anywhere.", tmp.path(), "a.ts");
        assert_eq!(outcome, AbsenceCheckOutcome::Contradicted { token: "foo()".to_string(), line: Some(1) });
    }

    // ── check_absence_claim_against_file: path containment (RED-PROVE, review MUST FIX 3) ──
    //
    // `file` is model-authored and unvalidated. Before `resolve_within_tree`
    // existed, `tree_root.join(file)` let an absolute `file` (or a `..`
    // relative one) escape the checked-out tree entirely and read whatever
    // it named — flipping "reject and abstain" (Inconclusive) into "read
    // and contradict" (Contradicted) for a file that was never inside the
    // reviewed source at all.

    #[test]
    fn an_absolute_file_cannot_escape_the_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tree = tmp.path().join("checkout");
        std::fs::create_dir_all(&tree).unwrap();
        // A file OUTSIDE the tree, containing the very token the claim
        // says is absent — proof positive this is escape-and-read, not a
        // coincidental miss.
        let outside = tmp.path().join("outside.env");
        std::fs::write(&outside, "export function foo() { return 1; }\n").unwrap();

        let outcome = check_absence_claim_against_file(
            "This does not call `foo()` anywhere.",
            &tree,
            outside.to_str().unwrap(),
        );
        assert_eq!(
            outcome,
            AbsenceCheckOutcome::Inconclusive,
            "an absolute `file` must be rejected before any read, never joined-and-escaped"
        );
    }

    #[test]
    fn a_parent_dir_relative_file_cannot_escape_the_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tree = tmp.path().join("checkout").join("app");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tmp.path().join("outside.env"), "export function foo() { return 1; }\n").unwrap();

        let outcome =
            check_absence_claim_against_file("This does not call `foo()` anywhere.", &tree, "../../outside.env");
        assert_eq!(
            outcome,
            AbsenceCheckOutcome::Inconclusive,
            "a `..`-bearing `file` must be rejected before any read, never joined-and-escaped"
        );
    }

    #[test]
    fn reading_a_device_file_is_bounded_not_blocking() {
        // (review MUST FIX 3) Reproduces the reviewer's exact case:
        // `/dev/zero` never reaches EOF, so an unbounded read grows memory
        // and blocks forever. This test bounds ITSELF with a background
        // thread + a generous-but-finite wait, so a regression here fails
        // fast instead of wedging the suite the way the unbounded read
        // wedged the host process.
        if !std::path::Path::new("/dev/zero").exists() {
            return; // POSIX device node; nothing to prove on a platform without it.
        }
        let handle = std::thread::spawn(|| {
            check_absence_claim_against_file("This does not call `foo()` anywhere.", Path::new("/dev"), "zero")
        });
        for _ in 0..50 {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            handle.is_finished(),
            "reading a device file must be bounded by MAX_FILE_READ_BYTES, not block indefinitely"
        );
        // The read completed within the bound; the specific verdict
        // doesn't matter here (NUL bytes are valid UTF-8, so this likely
        // reads MAX_FILE_READ_BYTES of them and lands on Confirmed) — the
        // property under test is boundedness, not this outcome.
        let _ = handle.join().unwrap();
    }

    // ── resolve_source_tree + run_backstop: the pipeline-wiring seam ───

    struct HomeGuard(Option<String>);
    impl HomeGuard {
        fn set(p: &std::path::Path) -> Self {
            let prior = std::env::var("DARKMUX_HOME").ok();
            std::env::set_var("DARKMUX_HOME", p);
            Self(prior)
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    fn write_plan_with_source(mission_id: &str, rule_id: &str, source_id: &str, tree: &std::path::Path) {
        let plan_dir = crate::loader::missions_dir().join(mission_id).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        let body = serde_json::json!({
            "kind": "crawl.plan",
            "schema_version": "1",
            "body": {
                "schema_version": "1.1",
                "workspace": "ws",
                "planned_at": "2026-09-10T00:00:00Z",
                "sources": [{"id": source_id, "sha": "abc123", "ref": "main", "tree": tree.to_string_lossy(), "files_walked": 1}],
                "units": [],
                "totals": {"units": 0, "est_tokens": 0, "by_rule": {}, "skipped": [], "edges": []},
                "rules": [rule_id],
            }
        });
        std::fs::write(plan_dir.join(format!("{rule_id}.json")), serde_json::to_string(&body).unwrap()).unwrap();
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn resolve_source_tree_finds_the_matching_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let tree = tmp.path().join("checkout").join("app");
        std::fs::create_dir_all(&tree).unwrap();
        write_plan_with_source("m-1", "swallowed-error", "app", &tree);

        let resolved = resolve_source_tree("m-1", "swallowed-error", "app");
        assert_eq!(resolved, Some(tree));
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn resolve_source_tree_is_none_when_nothing_was_ever_planned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        assert_eq!(resolve_source_tree("no-such-mission", "swallowed-error", "app"), None);
    }

    fn a_finding(key: &str, mission: &str, rule: &str, source: &str, file: &str, why: &str) -> FindingRecord {
        crate::findings::build_record(
            key.split('/').next().unwrap(),
            key.split('/').nth(1).unwrap().parse().unwrap(),
            "2026-09-10T00:00:00Z".to_string(),
            "create_finding",
            crate::findings::Proposer { handle: "reviewer".into(), model: "test".into(), machine_id: None },
            crate::findings::Scope { mission_id: Some(mission.to_string()), phase_id: None, step_id: None },
            Some(serde_json::json!({"rule": rule, "source": source})),
            serde_json::json!({"file": file, "line": 2, "pattern": rule, "evidence": "e", "why": why}),
        )
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn run_backstop_flags_only_the_contradicted_finding_in_a_mixed_batch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let tree = tmp.path().join("checkout").join("app");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a.ts"), "export function foo() { return 1; }\n").unwrap();
        write_plan_with_source("m-2", "existing-solution", "app", &tree);

        let contradicted = a_finding(
            "sess-a/1",
            "m-2",
            "existing-solution",
            "app",
            "a.ts",
            "This module does not call `foo()` anywhere.",
        );
        let genuinely_absent = a_finding(
            "sess-a/2",
            "m-2",
            "existing-solution",
            "app",
            "a.ts",
            "This module does not call `bar()` anywhere.",
        );
        let not_an_absence_claim = a_finding(
            "sess-a/3",
            "m-2",
            "existing-solution",
            "app",
            "a.ts",
            "This re-implements a retry helper that already exists.",
        );

        let notes = run_backstop("m-2", &[contradicted, genuinely_absent, not_an_absence_claim]);

        assert_eq!(notes.len(), 1, "{notes:?}");
        let note = notes.get("sess-a/1").expect("the contradicted finding is flagged");
        assert_eq!(note.token, "foo()");
        assert_eq!(note.file, "a.ts");
        assert_eq!(note.line, Some(1));
        assert!(!notes.contains_key("sess-a/2"), "a genuinely absent claim must not be flagged: {notes:?}");
        assert!(!notes.contains_key("sess-a/3"), "a non-absence claim must not be flagged: {notes:?}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn run_backstop_abstains_and_returns_nothing_when_the_tree_cannot_be_resolved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        // No plan file written for this mission at all — the tree cannot
        // be resolved, so the check cannot evaluate the claim. This must
        // not be an error and must not remove the finding from anything
        // downstream — it is simply absent from the returned map.
        let finding =
            a_finding("sess-b/1", "m-3", "existing-solution", "app", "a.ts", "This does not call `foo()`.");
        let notes = run_backstop("m-3", &[finding]);
        assert!(notes.is_empty(), "{notes:?}");
    }
}
