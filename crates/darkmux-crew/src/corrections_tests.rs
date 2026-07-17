//! (#849 / #1426) Tests for the shared adjudication-corrections reader — the
//! single definition of "what a correction is" that both the coder-brief
//! injection path and `darkmux memory correction list` read.

use super::*;

/// A day-file carrying the load-bearing cases: two corrections in one mission
/// family, an exact duplicate, a SIBLING mission whose id is a hyphen-extension
/// (the #849 prefix-bleed regression), and a wrong-source note.
const DAY: &str = concat!(
    r#"{"ts":"2026-06-21T10:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
    r#"{"ts":"2026-06-21T11:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s2","handle":"Use cargo test -p foo."}"#, "\n",
    r#"{"ts":"2026-06-21T11:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
    r#"{"ts":"2026-06-21T11:45:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-v2-s1","handle":"Belongs to auth-v2 ONLY."}"#, "\n",
    r#"{"ts":"2026-06-21T12:00:00Z","action":"note","source":"orchestrator","session_id":"mission-run-auth-s1","handle":"crew shipped it!"}"#, "\n",
    // An adjudication note with empty text — never a correction.
    r#"{"ts":"2026-06-21T12:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"   "}"#, "\n",
    // Unparsable line — skipped, must not poison the rest of the file.
    "{not json at all", "\n",
);

/// Run `f` with `DARKMUX_FLOWS_DIR` pointed at a temp dir seeded with `DAY`,
/// restoring the previous value afterward. Callers are `#[serial]` — this
/// mutates a process-wide env var the reader resolves live per-access.
fn with_flows<T>(f: impl FnOnce() -> T) -> T {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("2026-06-21.jsonl"), DAY).unwrap();
    let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
    // SAFETY: serialized via #[serial]; restored below.
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
    let out = f();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }
    out
}

fn ids(list: &[&str]) -> HashSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// An exact-set scope reads only that mission's sessions. The load-bearing
/// assertion is the sibling-mission exclusion: `auth-v2`'s note must NOT appear
/// for `auth` (a prefix match would bleed it). Duplicates are NOT collapsed here
/// — dedup is the brief path's policy, not the reader's.
#[test]
#[serial_test::serial]
fn scan_exact_set_scopes_to_the_mission_family_and_excludes_siblings() {
    let got = with_flows(|| {
        scan(
            ADJUDICATION_LOOKBACK_DAYS,
            Some(&ids(&["mission-run-auth-s1", "mission-run-auth-s2"])),
        )
    });
    assert_eq!(got.len(), 3, "two unique + one verbatim duplicate, undeduped: {got:?}");
    assert!(
        !got.iter().any(|c| c.text.contains("auth-v2")),
        "sibling mission auth-v2 must NOT bleed into auth (#849 prefix-bleed regression): {got:?}"
    );
    assert!(
        !got.iter().any(|c| c.text.contains("crew shipped")),
        "an orchestrator-source note is not a correction: {got:?}"
    );
    assert!(
        !got.iter().any(|c| c.text.trim().is_empty()),
        "an empty-text note is not a correction: {got:?}"
    );
    // Oldest→newest within the window.
    assert_eq!(got[0].ts, "2026-06-21T10:00:00Z", "{got:?}");
    assert_eq!(got[0].session_id, "mission-run-auth-s1");
    assert_eq!(got[2].ts, "2026-06-21T11:30:00Z", "{got:?}");
}

/// `None` = unscoped: every adjudication note in the window, across missions.
/// This is what `memory correction list` reads with no `--mission`/`--session`.
#[test]
#[serial_test::serial]
fn scan_unscoped_reads_every_session() {
    let got = with_flows(|| scan(ADJUDICATION_LOOKBACK_DAYS, None));
    assert_eq!(got.len(), 4, "three auth + one auth-v2, undeduped: {got:?}");
    assert!(
        got.iter().any(|c| c.session_id == "mission-run-auth-v2-s1"),
        "unscoped includes the sibling mission: {got:?}"
    );
}

/// An empty scope set matches nothing — and short-circuits before any IO.
#[test]
#[serial_test::serial]
fn scan_empty_scope_reads_as_none() {
    let got = with_flows(|| scan(ADJUDICATION_LOOKBACK_DAYS, Some(&HashSet::new())));
    assert!(got.is_empty(), "an empty session-id set reads as none: {got:?}");
}

/// A missing flows dir is best-effort-empty, never an error — the injection
/// path must never fail a dispatch over an unreadable trail.
#[test]
#[serial_test::serial]
fn scan_missing_flows_dir_is_empty_not_an_error() {
    let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
    // SAFETY: serialized via #[serial]; restored below.
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", "/nonexistent/darkmux/flows/xyzzy") };
    let got = scan(ADJUDICATION_LOOKBACK_DAYS, None);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }
    assert!(got.is_empty(), "{got:?}");
}

/// `days` bounds the scan to the most-recent N day-files.
#[test]
#[serial_test::serial]
fn scan_day_window_bounds_the_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("2026-06-20.jsonl"),
        concat!(r#"{"ts":"2026-06-20T10:00:00Z","action":"note","source":"adjudication","session_id":"s-old","handle":"older day"}"#, "\n"),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("2026-06-21.jsonl"),
        concat!(r#"{"ts":"2026-06-21T10:00:00Z","action":"note","source":"adjudication","session_id":"s-new","handle":"newer day"}"#, "\n"),
    )
    .unwrap();
    let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
    // SAFETY: serialized via #[serial]; restored below.
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
    let one = scan(1, None);
    let two = scan(2, None);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }
    assert_eq!(one.len(), 1, "a 1-day window reads only the newest day-file: {one:?}");
    assert_eq!(one[0].text, "newer day");
    assert_eq!(two.len(), 2, "a 2-day window reads both, oldest→newest: {two:?}");
    assert_eq!(two[0].text, "older day", "oldest first: {two:?}");
}
