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
}
