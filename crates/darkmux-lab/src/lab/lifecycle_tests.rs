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
