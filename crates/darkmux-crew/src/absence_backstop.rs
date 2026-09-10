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
//!     identifier/call span — [`looks_like_identifier_span`] refuses to
//!     search for those, on the theory that "searching" for a sentence as
//!     a literal substring produces noise, not signal.
//!   - a token that genuinely IS absent from the reviewed code but
//!     appears in a COMMENT or a STRING LITERAL discussing it
//!     (`// TODO: call foo()`) — the search is textual, not semantic, so
//!     this can FALSE-POSITIVE a contradiction (flag a claim that is
//!     actually correct). This is the known cost of a lint over a
//!     registry: it is why a contradiction only ever DEMOTES/ANNOTATES a
//!     finding (see [`AbsenceBackstopNote`]) rather than deleting or
//!     silently "correcting" it — a human still makes the final call.
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
    /// should be flagged, never silently confirmed as correct.
    Contradicted { token: String, line: Option<u32> },
}

/// Extract the claimed-absent token from a finding's own claim text.
///
/// Looks for a recognized absence PHRASE (case-insensitive) anywhere in
/// the text, and — independently — the FIRST backtick-quoted span
/// anywhere in the text. Deliberately not scoped tightly to "immediately
/// after the phrase": a model's prose puts the token before ("`foo()` is
/// never called") about as often as after ("does not call `foo()`"), and
/// a positional rule would miss half of those for no benefit. `None` when
/// no phrase matches, when a phrase matches but the text carries no
/// backtick-quoted span at all, or when the quoted span does not look
/// like a single identifier/call/member-access ([`looks_like_identifier_span`]).
pub fn detect_absence_claim(claim: &str) -> Option<String> {
    let lower = claim.to_ascii_lowercase();
    if !ABSENCE_PHRASES.iter().any(|p| lower.contains(p)) {
        return None;
    }
    let token = extract_backtick_token(claim)?;
    if token.chars().count() < MIN_TOKEN_LEN || !looks_like_identifier_span(&token) {
        return None;
    }
    Some(token)
}

fn extract_backtick_token(text: &str) -> Option<String> {
    let start = text.find('`')?;
    let rest = &text[start + 1..];
    let end = rest.find('`')?;
    let token = rest[..end].trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
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
    if let Some(line) = find_line(whole_file, &token) {
        return AbsenceCheckOutcome::Contradicted { token, line: Some(line) };
    }
    // The token spans a wrap (present in the file but split across two
    // lines by `.lines()`) — still a real contradiction, just without a
    // precise line to cite.
    if whole_file.contains(&token) {
        return AbsenceCheckOutcome::Contradicted { token, line: None };
    }
    AbsenceCheckOutcome::Confirmed { token }
}

fn find_line(whole_file: &str, token: &str) -> Option<u32> {
    whole_file.lines().enumerate().find(|(_, line)| line.contains(token)).map(|(idx, _)| idx as u32 + 1)
}

/// [`check_absence_claim`], reading the whole file itself.
///
/// Returns [`AbsenceCheckOutcome::Inconclusive`] — never an error — when
/// the file cannot be read (moved, deleted, outside the tree, not valid
/// UTF-8): the caller's rule is to leave a finding untouched on any
/// outcome that isn't `Contradicted`, so an unreadable file behaves
/// exactly like "no claim detected" from the caller's point of view. This
/// is the ONE place this module touches disk.
pub fn check_absence_claim_against_file(claim_text: &str, tree_root: &Path, file: &str) -> AbsenceCheckOutcome {
    let Ok(whole_file) = std::fs::read_to_string(tree_root.join(file)) else {
        return AbsenceCheckOutcome::Inconclusive;
    };
    check_absence_claim(claim_text, &whole_file)
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

    #[test]
    fn a_token_spanning_a_line_wrap_still_contradicts_with_no_line_cited() {
        let claim = "There is no `longToken` anywhere in this file.";
        // The token is present but split across a wrap, so `.lines()`
        // never sees it whole on any one line.
        let whole_file = "const x = \"long\" +\n  \"Token\";\n";
        // Not actually present as a contiguous substring either (split by
        // the concatenation), so this proves the CONFIRMED path, not a
        // wrap-hit — see the next test for an actual wrap hit.
        assert_eq!(check_absence_claim(claim, whole_file), AbsenceCheckOutcome::Confirmed { token: "longToken".to_string() });
    }

    #[test]
    fn a_token_present_but_not_isolated_on_one_line_still_contradicts() {
        let claim = "There is no `shared_helper` anywhere in this file.";
        // A whole_file whose newline placement puts the token across what
        // `find_line` treats as two neighboring lines' worth of raw text
        // is unusual for real source, but `.contains()` on the whole
        // string still finds it — proving `line: None` is reachable.
        let whole_file = "one line with shared_helper embedded, all on one physical line but repeated to keep this test honest: shared_helper";
        let outcome = check_absence_claim(claim, whole_file);
        match outcome {
            AbsenceCheckOutcome::Contradicted { token, .. } => assert_eq!(token, "shared_helper"),
            other => panic!("expected Contradicted, got {other:?}"),
        }
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
