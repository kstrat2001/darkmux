//! (#2693) The conformance assertion every module-local state guard in
//! this crate has to pass, in ONE place so the two modules that hold one
//! cannot drift apart.
//!
//! # Why an assertion rather than a comment
//!
//! `step_kinds::records_gather` and `absence_backstop` both used to carry
//! a hand-rolled `HomeGuard` that pinned `DARKMUX_HOME` and nothing else.
//! Both write mission fixtures through `loader::missions_dir()`, which
//! resolves `user_state_root()` — and that reads
//! `env(DARKMUX_CREW_DIR) > config.dirs.crew > <DARKMUX_HOME root>`. The
//! crew variable OUTRANKS the pinned root, so for anyone who had exported
//! one the guards isolated nothing and every test in a module wrote into
//! one shared directory. `records_gather` was 5–11 failures per run of
//! the module alone, a different set each time; `absence_backstop` was
//! silently green while leaking its plan fixtures into the operator's
//! tree.
//!
//! Both now hold [`darkmux_types::test_isolation::IsolatedState`], which
//! pins the whole set. The assertion below is what stops that being
//! narrowed back one variable at a time — the erosion mode
//! `test_isolation`'s own module doc names, measured again here: with the
//! assertion present in `records_gather` only, reverting
//! `absence_backstop`'s alias to a home-only guard left `cargo test -p
//! darkmux-crew --lib` at EXIT=0, 1705 passed, while two plan fixtures
//! leaked. Half a diff, fully green.
//!
//! # Two limits, stated so the assertion is not over-trusted
//!
//! **It brings its OWN `DARKMUX_CREW_DIR` rather than inheriting the
//! ambient environment.** That is the whole point: the defect it replaces
//! was invisible to anyone whose environment had none set, and visible —
//! as 5–11 failures — to anyone whose did. A conformance test that
//! inherits the ambient environment inherits that same blind spot, which
//! is exactly backwards from what a conformance test is for.
//!
//! **It can only catch a variable that is EXPORTED.** Adding a new tier
//! above everything in `crew_dir_override()` and leaving it unset leaves
//! these tests at EXIT=0, blind. The net for that case is held one
//! package over, by `darkmux-doctor`'s
//! `every_state_root_resolves_under_an_isolated_darkmux_home`, which
//! names the membership of the pinned set directly and so goes red on an
//! unexported addition. This assertion proves the module USES the guard;
//! that one proves the guard COVERS the variable. Neither substitutes for
//! the other.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

const VAR: &str = "DARKMUX_CREW_DIR";

/// What a module's own guard + fixture write was observed to do, captured
/// while the guard was still held.
pub(crate) struct GuardProbe {
    /// The root the module's guard claims to isolate under.
    pub root: PathBuf,
    /// Where the fixture the closure wrote actually resolved to.
    pub written: PathBuf,
    /// Whether that path existed **while the guard was still held** — the
    /// guard's temp root is deleted on drop, so this cannot be re-checked
    /// afterwards, and without it the two `starts_with` assertions would
    /// both pass for a write that never happened.
    pub written_exists: bool,
}

/// A `DARKMUX_CREW_DIR` standing in for the operator who ran this suite
/// with a scratch crew dir exported — the environment in which the
/// modules were red.
///
/// RAII, and deliberately so: restoring by straight-line code at the end
/// of a test leaves a panicking assertion to unwind past the restore, and
/// the module guard's own `Drop` then points the process at this
/// directory just before it is deleted — every later unguarded test in
/// the process would resolve crew state under a path that no longer
/// exists. The `Drop` below restores the ambient value FIRST; `tmp` is a
/// field, so it is dropped after this body runs, never before.
struct SentinelCrewDir {
    tmp: tempfile::TempDir,
    prev: Option<OsString>,
}

impl SentinelCrewDir {
    fn pin() -> Self {
        let tmp = tempfile::TempDir::new().expect("sentinel crew dir");
        let prev = std::env::var_os(VAR);
        // SAFETY: caller holds #[serial_test::serial].
        unsafe { std::env::set_var(VAR, tmp.path()) };
        Self { tmp, prev }
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }
}

impl Drop for SentinelCrewDir {
    fn drop(&mut self) {
        // SAFETY: caller holds #[serial_test::serial].
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
    }
}

/// Assert that a module's guard isolates crew state even when a
/// `DARKMUX_CREW_DIR` is already pinned.
///
/// `probe` must construct the module's OWN guard alias, write the
/// module's OWN fixture through the production resolver, and return where
/// that landed — dropping the guard before it returns. Calling it with
/// anything else (a hand-built path, `IsolatedState` named directly
/// rather than through the module's alias) makes the assertion vacuous:
/// it would then pass with the module's guard narrowed back to
/// `DARKMUX_HOME`.
///
/// The caller must hold `#[serial_test::serial]` — this mutates
/// process-global environment.
pub(crate) fn assert_guard_isolates_crew_state(probe: impl FnOnce() -> GuardProbe) {
    let sentinel = SentinelCrewDir::pin();
    let observed = probe();

    assert!(
        observed.written.starts_with(&observed.root),
        "a fixture write must land under the guard's own root ({}), got {}",
        observed.root.display(),
        observed.written.display()
    );
    assert!(
        !observed.written.starts_with(sentinel.path()),
        "a fixture write reached the pinned DARKMUX_CREW_DIR at {} — every test in this \
         module then shares one state directory and reads the others' records",
        observed.written.display()
    );
    assert!(
        observed.written_exists,
        "the fixture must actually exist where it resolved ({}) — two path assertions over \
         a write that never happened prove nothing",
        observed.written.display()
    );
    assert_eq!(
        std::env::var_os(VAR).as_deref(),
        Some(sentinel.path().as_os_str()),
        "the guard must restore the DARKMUX_CREW_DIR it displaced"
    );
}

/// (#2718) The RECORD-destination half of the same conformance claim.
///
/// [`assert_guard_isolates_crew_state`] above covers `missions/`,
/// `phases/`, `roles/` — everything that resolves through
/// `loader::user_state_root()`. That is the boundary #2693's probe was
/// placed at, and it is structurally blind to this class:
/// `config_access::findings_dir()`, `config_access::mods_dir()` and
/// `darkmux_flow::record()` never call it. So a `-p darkmux-crew --lib`
/// run could be — and was — EXIT=0 with the crew dir at zero files while
/// writing a findings record, 29 flow records across 12 distinct
/// (action, source, session) groups, 214 hash-chained audit records and a
/// liveness heartbeat into the operator's real tree.
///
/// The lesson was about probe PLACEMENT, so this assertion is placed at
/// the other boundary rather than widening the first one.
pub(crate) struct RecordDestinationProbe {
    /// The root the guard under test claims to isolate under.
    pub root: PathBuf,
    /// Every path the probe wrote through a production resolver, with
    /// whether it existed **while the guard was still held**. The guard's
    /// root is a `TempDir`, deleted the moment the probe returns, so this
    /// cannot be re-checked afterwards — and without it the path
    /// assertions would both pass for a write that never happened.
    pub written: Vec<(PathBuf, bool)>,
}

/// The operator's `findings/`, `mods/`, `flows/` and audit destinations,
/// standing in for a real machine's.
///
/// `DARKMUX_AUDIT_DIR` is included and PINNED rather than merely named:
/// its presence is what switches the hash-chained sink on, so pinning it
/// is what makes the audit destination reachable at all. That is the
/// destination that matters most, because an append-only chained record
/// cannot be removed without breaking the chain (#2697).
///
/// RAII for the same reason [`SentinelCrewDir`] is: a panicking assertion
/// unwinds past straight-line restore code, and the guard under test would
/// then hand the process a set of directories that are about to be
/// deleted.
struct SentinelRecordDirs {
    tmp: tempfile::TempDir,
    prev: Vec<(&'static str, Option<OsString>)>,
}

impl SentinelRecordDirs {
    const VARS: [(&'static str, &'static str); 4] = [
        ("DARKMUX_FINDINGS_DIR", "findings"),
        ("DARKMUX_MODS_DIR", "mods"),
        ("DARKMUX_FLOWS_DIR", "flows"),
        ("DARKMUX_AUDIT_DIR", "audit"),
    ];

    fn pin() -> Self {
        let tmp = tempfile::TempDir::new().expect("sentinel record dirs");
        let mut prev = Vec::new();
        for (var, sub) in Self::VARS {
            prev.push((var, std::env::var_os(var)));
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::set_var(var, tmp.path().join(sub)) };
        }
        Self { tmp, prev }
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// Every regular file under the sentinel, recursively — the assertion
    /// the issue asks for, made on FILES rather than on whether the run
    /// was green. A `-p darkmux-crew --lib` run was EXIT=0 throughout.
    fn files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => walk(&path, out),
                    Ok(_) => out.push(path),
                    Err(_) => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(self.tmp.path(), &mut out);
        out.sort();
        out
    }
}

impl Drop for SentinelRecordDirs {
    fn drop(&mut self) {
        // SAFETY: caller holds #[serial_test::serial].
        unsafe {
            for (var, prev) in &self.prev {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

/// Assert that the guard this crate's tests bring isolates the RECORD
/// destinations even when an operator has all four pinned.
///
/// `probe` must construct the guard, write through the PRODUCTION
/// resolvers (`darkmux_flow::record()`, `findings::findings_dir()`), and
/// report where those writes landed — dropping the guard before it
/// returns. Handing it hand-built paths would make this vacuous.
///
/// The caller must hold `#[serial_test::serial]`.
pub(crate) fn assert_guard_isolates_the_record_destinations(
    probe: impl FnOnce() -> RecordDestinationProbe,
) {
    let sentinel = SentinelRecordDirs::pin();
    let observed = probe();

    assert!(
        !observed.written.is_empty(),
        "the probe reported no writes at all; two path assertions over a write that never \
         happened prove nothing"
    );
    for (written, existed) in &observed.written {
        assert!(
            written.starts_with(&observed.root),
            "a record write must land under the guard's own root ({}), got {}",
            observed.root.display(),
            written.display()
        );
        assert!(
            existed,
            "the probe says it wrote {} but nothing was there while the guard was held",
            written.display()
        );
    }

    let leaked = sentinel.files();
    assert!(
        leaked.is_empty(),
        "{} file(s) reached the operator's pinned record destinations under {}. With a \
         normal environment these are their real findings store, their real flow day-file \
         and their hash-chained audit sink — and a chained record cannot be removed \
         without breaking the chain:\n{}",
        leaked.len(),
        sentinel.path().display(),
        leaked.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
    );

    for (var, sub) in SentinelRecordDirs::VARS {
        assert_eq!(
            std::env::var_os(var).as_deref(),
            Some(sentinel.path().join(sub).as_os_str()),
            "the guard must restore the {var} it displaced"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#2693) The sentinel's restore has to survive a FAILING
    /// conformance assertion, which is the only time it matters.
    ///
    /// The first cut of this helper restored by straight-line code at the
    /// end of the test body. A panicking assertion unwinds past that, and
    /// the module guard's own `Drop` then points the process at the
    /// sentinel directory immediately before that directory is deleted —
    /// so every later test in the process resolves crew state under a
    /// path that no longer exists, for a reason unconnected to its own
    /// change. That is the non-RAII shape #2698 names, and a guard whose
    /// restore only works when nothing goes wrong is not a guard.
    ///
    /// Red-proved by moving the restore out of [`SentinelCrewDir::drop`]
    /// into straight-line code at the end of
    /// [`assert_guard_isolates_crew_state`]: this then observes the
    /// sentinel path (now deleted) instead of the ambient value.
    #[test]
    #[serial_test::serial] // mutates DARKMUX_CREW_DIR, a process-global
    fn the_sentinel_restores_the_ambient_crew_dir_even_when_the_assertion_panics() {
        let ambient = std::env::var_os(VAR);
        // SAFETY: #[serial]. A value to be handed back, so "restored" is a
        // real observation rather than a coincidence of the environment.
        unsafe { std::env::set_var(VAR, "/darkmux-sentinel-ambient") };

        let prior_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_guard_isolates_crew_state(|| panic!("a probe that blows up mid-write"));
        }));
        std::panic::set_hook(prior_hook);

        assert!(unwound.is_err(), "the probe's panic must propagate, not be swallowed");
        assert_eq!(
            std::env::var_os(VAR).as_deref(),
            Some(std::ffi::OsStr::new("/darkmux-sentinel-ambient")),
            "a panicking assertion must still hand the ambient DARKMUX_CREW_DIR back — \
             leaving the sentinel pinned points every later test in this process at a \
             directory that is about to be deleted"
        );

        // SAFETY: #[serial].
        unsafe {
            match ambient {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
    }

    /// (#2718) The record-destination conformance claim, driven through
    /// the PRODUCTION resolvers and the guard this crate's tests actually
    /// bring.
    ///
    /// The whole-run property behind it, measured on this branch with all
    /// twelve state variables exported to a fresh root and `$HOME`
    /// redirected:
    ///
    /// | | before | after |
    /// |---|---|---|
    /// | `findings/` | 1 file (`sess-emit/1/finding.json`) | 0 |
    /// | `flows/` | 29 records, 12 distinct (action, source, session) groups | 0 |
    /// | audit chain | 214 records | 0 |
    /// | `liveness/` | 1 file | 0 |
    ///
    /// EXIT=0 and 1707 passed on BOTH sides, which is why the assertion is
    /// on files and never on the status.
    ///
    /// What this test holds is narrower than that table, and stated so it
    /// is not over-trusted: it proves the GUARD covers findings, mods,
    /// flows and the audit chain, so the twenty-four call sites that now
    /// hold one are closed by something that has been measured rather than
    /// assumed. It cannot prove a twenty-fifth writer has not been added
    /// since — that is the execution-side check's job
    /// (`tests/state_leak_execution_guard.rs`, #2717), which runs this
    /// package under a sentinel tree and counts.
    #[test]
    #[serial_test::serial]
    fn the_guard_isolates_findings_mods_flows_and_the_audit_chain() {
        assert_guard_isolates_the_record_destinations(|| {
            let isolated = darkmux_types::test_isolation::IsolatedState::new();
            let root = isolated.path().to_path_buf();

            // A flow record through the process-wide default sink — the
            // same call every emitter in this crate makes.
            darkmux_flow::record(darkmux_flow::FlowRecord {
                ts: darkmux_flow::ts_utc_now(),
                level: darkmux_flow::Level::Info,
                category: darkmux_flow::Category::Work,
                tier: darkmux_flow::Tier::Local,
                stage: darkmux_flow::Stage::Dispatch,
                action: "dispatch.tool".to_string(),
                handle: "conformance".to_string(),
                phase_id: None,
                session_id: Some("sess-conformance".to_string()),
                source: Some("crew_dispatch".to_string()),
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            })
            .expect("the flow write must succeed, or this probe proves nothing");

            // A finding through the production store resolver.
            let record = crate::findings::build_record(
                "sess-conformance",
                1,
                darkmux_flow::ts_utc_now(),
                "create_finding",
                crate::findings::Proposer {
                    handle: "conformance".to_string(),
                    model: "test-model".to_string(),
                    machine_id: None,
                },
                crate::findings::Scope::default(),
                None,
                serde_json::json!({"note": "conformance"}),
            );
            let findings_root = crate::findings::findings_dir();
            crate::findings::materialize(&findings_root, &record).expect("finding write");

            let finding_path =
                crate::findings::record_path_at(&findings_root, "sess-conformance", 1);
            let finding_exists = finding_path.is_file();
            let mut written = vec![(finding_path, finding_exists)];
            // The day-file's name is today's date; read it back rather
            // than recomputing it, so a clock-boundary run still reports
            // the file that was actually created.
            let flows = darkmux_types::config_access::flows_dir();
            if let Ok(entries) = std::fs::read_dir(&flows) {
                let mut day_files: Vec<PathBuf> =
                    entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
                day_files.sort();
                written.extend(day_files.into_iter().map(|p| (p, true)));
            }

            RecordDestinationProbe { root, written }
        });
    }

}
