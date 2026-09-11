//! Exit-path coverage for [`RunLifecycle`].
//!
//! A lifecycle function is done when EVERY exit path is covered, not when the
//! suite is green — partial coverage here leaves a corrupted record, which is
//! worse than a crash because nothing reports it. So there is one test per
//! path a process can actually observe: explicit completion, explicit error,
//! an early `return`, a `?` propagation, and an unwinding panic. The paths
//! `Drop` genuinely cannot reach (`SIGKILL`, `panic = "abort"`) are named in
//! the module doc rather than pretended away.

use super::*;
use tempfile::TempDir;

fn status_of(dir: &Path) -> LifecycleStatus {
    read(dir).expect("a lifecycle record must exist").status
}

#[test]
fn start_writes_the_running_bookend_before_any_work() {
    let tmp = TempDir::new().unwrap();
    let _guard = RunLifecycle::start(tmp.path(), "run-1", "long-agentic", "coder").unwrap();

    let rec = read(tmp.path()).expect("record written at start, not at end");
    assert_eq!(rec.status, LifecycleStatus::Running);
    assert_eq!(rec.run_id, "run-1");
    assert_eq!(rec.kind, "lab", "the scan keys on this to classify the run");
    assert_eq!(rec.workload, "long-agentic");
    assert_eq!(rec.profile, "coder");
    assert!(rec.ended_at_ms.is_none());
    std::mem::forget(_guard); // this test is about START only
}

#[test]
fn explicit_completion_is_terminal() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap().finish_complete();
    assert_eq!(status_of(tmp.path()), LifecycleStatus::Complete);
    assert!(read(tmp.path()).unwrap().ended_at_ms.is_some());
}

#[test]
fn explicit_error_records_the_reason() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p")
        .unwrap()
        .finish_error("dispatching to model: qwen/example-27b");

    let rec = read(tmp.path()).unwrap();
    assert_eq!(rec.status, LifecycleStatus::Error);
    assert_eq!(
        rec.error.as_deref(),
        Some("dispatching to model: qwen/example-27b"),
        "an errored run must say WHY, not merely that it stopped"
    );
}

/// (#2462) The explicit signal-interrupted path — distinct from BOTH
/// `finish_error` (would misclassify a signal-caused failure as "the
/// endpoint broke") and the bare `Drop` path (which knows a run didn't
/// finish, but not why). `finish_interrupted` records the SAME status
/// `Drop` would, but keeps the reason a caller already worked out.
#[test]
fn explicit_interrupted_records_the_reason() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p")
        .unwrap()
        .finish_interrupted("hosted dispatch interrupted by an operator signal (SIGTERM)");

    let rec = read(tmp.path()).unwrap();
    assert_eq!(
        rec.status,
        LifecycleStatus::Interrupted,
        "a signal-caused failure must not be archived as Error — that is precisely the \
         evidence-pointing-at-the-wrong-cause bug #2462 is about"
    );
    assert_eq!(
        rec.error.as_deref(),
        Some("hosted dispatch interrupted by an operator signal (SIGTERM)"),
        "the WHY must survive, same as finish_error"
    );
    assert!(rec.ended_at_ms.is_some());
}

// ── session-id join (#2511) ───────────────────────────────────────────────

/// A provider that has minted its dispatch session id reports it back
/// mid-run, WHILE the record is still `Running` — this is the whole point:
/// a live row must be joinable to its own flow session before it finishes,
/// not only afterward.
#[test]
fn set_session_id_attaches_it_while_still_running() {
    let tmp = TempDir::new().unwrap();
    let mut lc = RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap();
    lc.set_session_id("darkmux-coding-demo-123");

    let rec = read(tmp.path()).expect("record must exist");
    assert_eq!(rec.status, LifecycleStatus::Running, "attaching the id must not terminate the run");
    assert_eq!(rec.session_id.as_deref(), Some("darkmux-coding-demo-123"));
    std::mem::forget(lc); // this test is about the mid-run write only
}

/// The id set mid-run must survive the terminal write — `terminate()` only
/// touches `status`/`ended_at_ms`/`error`; a session id already on the
/// in-memory record must not be clobbered back to `None` by the final
/// serialize.
#[test]
fn set_session_id_survives_the_terminal_write() {
    let tmp = TempDir::new().unwrap();
    let mut lc = RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap();
    lc.set_session_id("darkmux-prompt-demo-456");
    lc.finish_complete();

    let rec = read(tmp.path()).unwrap();
    assert_eq!(rec.status, LifecycleStatus::Complete);
    assert_eq!(
        rec.session_id.as_deref(),
        Some("darkmux-prompt-demo-456"),
        "the terminal write must not erase a session id set earlier in the run"
    );
}

/// The inverted case (required alongside the positive one): a run whose
/// provider never calls `set_session_id` — `tool-bench`'s real shape, and
/// any run that fails before minting one — must NOT gain a fabricated id
/// anywhere along the way, including at the terminal write.
#[test]
fn a_run_that_never_mints_a_session_id_never_gains_one() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap().finish_complete();

    let rec = read(tmp.path()).unwrap();
    assert_eq!(rec.status, LifecycleStatus::Complete);
    assert_eq!(
        rec.session_id, None,
        "no mint call means nothing to claim — never a fallback to the run id \
         or any other guessable string"
    );
}

/// (#2511 review CONSIDER 5) An empty string is not a session id — assigning
/// one would still satisfy every downstream `Option::is_some()` read while
/// joining to nothing. Must never reach disk, in either direction: not as
/// the first value, and not as an attempted overwrite of a real one.
#[test]
fn set_session_id_ignores_an_empty_string() {
    let tmp = TempDir::new().unwrap();
    let mut lc = RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap();
    lc.set_session_id("");
    assert_eq!(read(tmp.path()).unwrap().session_id, None, "an empty string must never be claimed");

    lc.set_session_id("darkmux-coding-real-1");
    lc.set_session_id("");
    assert_eq!(
        read(tmp.path()).unwrap().session_id.as_deref(),
        Some("darkmux-coding-real-1"),
        "an empty string offered AFTER a real id must not clobber it"
    );
    std::mem::forget(lc);
}

/// (#2511 review CONSIDER 5) The trait's own doc says `on_session_id` is
/// "Called AT MOST ONCE" — this is the loud, debug-time half of enforcing
/// that, so a provider bug (or a future caller that doesn't honor the
/// contract) is caught where the mistake was made, not silently tolerated.
#[test]
#[should_panic(expected = "AT MOST ONCE")]
fn set_session_id_called_twice_panics_in_a_debug_build() {
    let tmp = TempDir::new().unwrap();
    let mut lc = RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap();
    lc.set_session_id("darkmux-coding-first-1");
    lc.set_session_id("darkmux-coding-second-1");
    std::mem::forget(lc);
}

/// (#2511 review CONSIDER 5) The other half: a caller that ignores the
/// debug assertion above (or a release build where `debug_assert!` compiles
/// out) must still keep the FIRST value, never silently adopt the second —
/// a session already claimed by a flow-session consumer must not be
/// re-pointed at a different one out from under it. Exercises the same
/// double-call the test above proves panics, but continues past the panic
/// (`catch_unwind`) to inspect the record the debug assertion guards.
#[test]
fn set_session_id_called_twice_keeps_the_first_value_past_the_assertion() {
    let tmp = TempDir::new().unwrap();
    let mut lc = RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap();
    lc.set_session_id("darkmux-coding-first-2");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lc.set_session_id("darkmux-coding-second-2");
    }));
    assert!(result.is_err(), "the debug build must have asserted on the second call");
    assert_eq!(
        read(tmp.path()).unwrap().session_id.as_deref(),
        Some("darkmux-coding-first-2"),
        "the record on disk must still hold the FIRST claimed session, not the second"
    );
    std::mem::forget(lc);
}

// ── the paths nobody writes on purpose ────────────────────────────────────

#[test]
fn early_return_leaves_interrupted_not_running() {
    // `needless_return` is correct in general and wrong here: the bare
    // `return` IS the exit path under test. Rewriting it to satisfy the lint
    // would leave a test that no longer exercises what its name claims.
    #[allow(clippy::needless_return)]
    fn bails_out(dir: &Path) {
        let _lc = RunLifecycle::start(dir, "r", "w", "p").unwrap();
        return; // the shape of every guard clause in a long function
    }
    let tmp = TempDir::new().unwrap();
    bails_out(tmp.path());
    assert_eq!(
        status_of(tmp.path()),
        LifecycleStatus::Interrupted,
        "a run abandoned by an early return must not still read as live"
    );
}

#[test]
fn question_mark_propagation_leaves_interrupted() {
    fn fails(dir: &Path) -> Result<()> {
        let _lc = RunLifecycle::start(dir, "r", "w", "p")?;
        // The real shape: `with_provider(..)??` propagates straight out of
        // `lab_run`, skipping every write below it.
        Err(anyhow::anyhow!("provider blew up"))?;
        unreachable!()
    }
    let tmp = TempDir::new().unwrap();
    assert!(fails(tmp.path()).is_err());
    assert_eq!(status_of(tmp.path()), LifecycleStatus::Interrupted);
}

#[test]
fn unwinding_panic_leaves_interrupted() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let res = std::panic::catch_unwind(move || {
        let _lc = RunLifecycle::start(&dir, "r", "w", "p").unwrap();
        panic!("provider panicked mid-run");
    });
    assert!(res.is_err(), "the panic must actually have happened");
    assert_eq!(
        status_of(tmp.path()),
        LifecycleStatus::Interrupted,
        "Drop runs while unwinding — a panicked run must not read as live"
    );
}

#[test]
fn a_finished_run_is_not_reopened_by_drop() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap().finish_error("real cause");

    let rec = read(tmp.path()).unwrap();
    assert_eq!(rec.status, LifecycleStatus::Error, "Drop must not overwrite a terminal status");
    assert_eq!(rec.error.as_deref(), Some("real cause"), "nor erase the reason");
}

// ── reader leniency ───────────────────────────────────────────────────────

#[test]
fn a_malformed_record_reads_as_absent_rather_than_failing_the_scan() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join(LIFECYCLE_FILE), b"{ truncated").unwrap();
    assert!(
        read(tmp.path()).is_none(),
        "one bad file must not be able to fail a whole lab-dir scan"
    );
}

#[test]
fn a_status_from_a_newer_writer_reads_as_unknown_not_a_parse_error() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join(LIFECYCLE_FILE),
        br#"{"schema_version":"9.9","run_id":"r","kind":"lab","workload":"w",
             "profile":"p","started_at_ms":1,"status":"quantum_superposition"}"#,
    )
    .unwrap();

    let rec = read(tmp.path()).expect("lenient on read — a newer status still parses");
    assert_eq!(rec.status, LifecycleStatus::Unknown);
    assert!(
        !rec.status.is_terminal(),
        "an uninterpretable status must never be treated as a terminal verdict"
    );
}

#[test]
fn the_write_is_atomic_and_leaves_no_temp_behind() {
    let tmp = TempDir::new().unwrap();
    RunLifecycle::start(tmp.path(), "r", "w", "p").unwrap().finish_complete();

    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
}
