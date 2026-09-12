//! (#2695/#2697/#2698) One RAII guard that pins EVERY darkmux write
//! destination under a single throwaway root.
//!
//! # Why this exists
//!
//! Test isolation in this repo used to be enforced **per variable**: each
//! test suite grew its own little guard pinning the one environment
//! variable whose leak had most recently been noticed — a `CrewGuard` for
//! `DARKMUX_CREW_DIR`, a `FlowsDirGuard` for `DARKMUX_FLOWS_DIR`, a
//! `DarkmuxHomeGuard` for `DARKMUX_HOME` (later amended to also pin the
//! crew dir, because `user_state_root()` resolves that first). Every such
//! guard is correct about its own variable and silent about the other
//! twelve, so each newly-added write destination leaks until someone
//! notices — and "someone notices" has meant a reviewer running with a
//! sentinel directory, four separate times.
//!
//! Three measured consequences, filed as #2697, #2695 and #2698:
//!
//! * `darkmux-serve`'s `runs::` tests hold a `CrewGuard` — which isolates
//!   mission state — and then emit fabricated `mission close` records
//!   ("mission errored", "mission completed (degraded)") through a flow
//!   sink that resolves `flows_dir()`, which that guard does not pin. Once
//!   in the operator's stream those records feed `run list`, the viewer,
//!   and — when `audit.enabled` — the hash-chained audit sink, where they
//!   cannot be removed without breaking the chain.
//! * The isolation guard test itself only neutralized `DARKMUX_HOME`, so
//!   it went RED for anyone who pinned a scratch `DARKMUX_CREW_DIR` (the
//!   careful thing to do) and stayed GREEN for anyone who pinned nothing
//!   — exactly backwards from what a guard is for.
//! * A guard's restore could be made a no-op with the suite fully green,
//!   because nothing asserted the restore.
//!
//! The structural answer is one guard that pins the whole set, derived
//! from the resolvers rather than from the last incident, plus a keystone
//! test that asserts the *isolation property* over that set. Remembering
//! to combine three guards does not scale; that failure to scale is the
//! bug class itself.
//!
//! # What "the whole set" means
//!
//! Two lists, because the variables are not all the same kind of thing.
//!
//! [`PINNED_STATE_VARS`] are variables that only ever NAME a location.
//! Setting one to a path under the guard's root is pure isolation: no
//! behavior changes, the write just lands somewhere throwaway.
//!
//! [`CLEARED_STATE_VARS`] are variables whose PRESENCE carries meaning
//! beyond the location, so pinning them would change what the code under
//! test does. `DARKMUX_AUDIT_DIR` is the sharp one — `audit_enabled()` is
//! true whenever it is set, so pinning it would switch the hash-chained
//! sink ON for every guarded test. These are removed instead, which is
//! still isolation: with the override gone, each falls back to a default
//! derived from the root, and the root is pinned.
//!
//! # The residual, stated out loud
//!
//! Clearing the env tier does not neutralize the **config tier**. A
//! resolver reads `env > config.json > default`, and a crate whose test
//! build does not enable `darkmux-types/test-support` gets a `config()`
//! that reads the operator's real `~/.darkmux/config.json`. So a
//! destination the operator has relocated in config — this machine has
//! `dirs.notebook` set, and `hooks.outbox_dir` has no env var at all —
//! can still escape a root-only pin. That is precisely why
//! [`PINNED_STATE_VARS`] pins the env tier for every destination that HAS
//! an env var: the env tier outranks config, so pinning it closes the
//! config tier too. The destinations left exposed are the ones with no
//! env var, and the keystone test in `darkmux-doctor`
//! (`every_state_root_resolves_under_an_isolated_darkmux_home`) asserts
//! over those as well, so the exposure is measured rather than assumed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Variables that NAME a write destination and nothing more. The guard
/// points each at a path under its own throwaway root.
///
/// Each entry is `(variable, subpath under the root)`. The subpath mirrors
/// the layout the built-in defaults produce, so a guarded test sees the
/// same shape it would see in production — just rooted somewhere
/// disposable.
///
/// Derived from the resolvers, NOT from any incident: every accessor in
/// `config_access` that returns a path and reads an env tier appears here,
/// plus `darkmux_crew::loader::user_state_root`'s `DARKMUX_CREW_DIR`
/// (which outranks `DARKMUX_HOME`, and is the one that made the keystone
/// test fail) and the `DARKMUX_HOME` root itself.
pub const PINNED_STATE_VARS: &[(&str, &str)] = &[
    // The root. Every default below derives from it; pinned first so the
    // rest are belt-and-braces rather than load-bearing.
    ("DARKMUX_HOME", ""),
    // OUTRANKS `DARKMUX_HOME` in `user_state_root()`/`crew_root()`, which
    // is what every `missions/`, `phases/`, `roles/`, `crews/` and
    // `skills/` write resolves through. The variable names the directory
    // CONTAINING those subdirs, so it pins to the root itself, not a
    // subpath.
    ("DARKMUX_CREW_DIR", ""),
    ("DARKMUX_FLOWS_DIR", "flows"),
    ("DARKMUX_FINDINGS_DIR", "findings"),
    ("DARKMUX_MODS_DIR", "mods"),
    ("DARKMUX_LAB_DIR", "runs"),
    ("DARKMUX_NOTEBOOK_DIR", "notebook"),
    ("DARKMUX_ACK_DIR", "acks"),
    ("DARKMUX_FLEET_FILE", "fleet.json"),
    ("DARKMUX_IDENTITY_PATH", "identity.json"),
];

/// Variables the guard REMOVES rather than pins, because their presence
/// means something beyond "the destination is here".
///
/// * `DARKMUX_AUDIT_DIR` — `config_access::audit_enabled()` is true
///   whenever this is set (the documented env path to turning the
///   hash-chained sink on; there is deliberately no
///   `DARKMUX_AUDIT_ENABLED`). Pinning it would enable audit for every
///   guarded test. Removed instead: the caller's default is
///   `<root>/audit`, and the root is pinned.
/// * `DARKMUX_PROFILES` — short-circuits the whole profile search chain
///   and attributes load errors to itself. Removing it restores the
///   normal chain, whose first user-scope candidate derives from the
///   pinned root.
/// * `DARKMUX_TEMPLATES_DIR` / `DARKMUX_SKILLS_DIR` — search-path
///   PREPENDS (read side, not write). An ambient value would have a
///   guarded test reading the operator's real templates/skills; removing
///   it leaves only the root-derived candidates.
pub const CLEARED_STATE_VARS: &[&str] = &[
    "DARKMUX_AUDIT_DIR",
    "DARKMUX_PROFILES",
    "DARKMUX_TEMPLATES_DIR",
    "DARKMUX_SKILLS_DIR",
];

/// (#2698) RAII pin for the runtime inactivity budget —
/// `DARKMUX_INACTIVITY_TIMEOUT_SECONDS`, a documented operator knob read
/// LIVE per access.
///
/// Same bug class as the directory leaks above, one axis over: a fixture
/// whose assertion is measured against a threshold the ENVIRONMENT owns.
/// `stale_after_ms()` is `inactivity_timeout_seconds() * 2`, so a test
/// that places a record "5 seconds after the last one" and expects that to
/// read *fresh* is quietly asserting that the operator has not exported a
/// small budget — and one that places a sample a day later and expects it
/// *excluded* is asserting they have not exported a large one. Measured on
/// `crates/darkmux-serve/src/lib_tests.rs`: `cargo test -p darkmux-serve
/// --lib` is EXIT=0 with the knob unset and at `600`, and EXIT=101 at both
/// `1` and `86400`. An operator who has set it ran a red suite for a
/// reason unconnected to their change.
///
/// This is the clock rule applied to the DENOMINATOR: freeze the budget
/// the distance is measured against, not just the timestamps.
///
/// It lives here beside [`IsolatedState`] because three separate copies of
/// this guard had already grown in three test modules (`mission_status`,
/// `runs`, and — absent entirely — `lib_tests`, which is why those two
/// tests were left behind). Copy number four is not the fix.
///
/// Caller must hold `#[serial_test::serial]`.
pub struct InactivityBudget {
    prev: Option<OsString>,
}

impl InactivityBudget {
    const VAR: &'static str = "DARKMUX_INACTIVITY_TIMEOUT_SECONDS";

    /// Pin the budget to an explicit number of seconds.
    pub fn seconds(secs: u64) -> Self {
        let prev = std::env::var_os(Self::VAR);
        // SAFETY: caller holds #[serial_test::serial].
        unsafe { std::env::set_var(Self::VAR, secs.to_string()) };
        Self { prev }
    }

    /// Pin the budget to the BUILT-IN default by clearing the override —
    /// the only way to reach the default tier, which a test asserting the
    /// shipped value has to do.
    pub fn unset() -> Self {
        let prev = std::env::var_os(Self::VAR);
        // SAFETY: caller holds #[serial_test::serial].
        unsafe { std::env::remove_var(Self::VAR) };
        Self { prev }
    }
}

impl Drop for InactivityBudget {
    fn drop(&mut self) {
        // SAFETY: caller holds #[serial_test::serial].
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(Self::VAR, v),
                None => std::env::remove_var(Self::VAR),
            }
        }
    }
}

/// Pins every darkmux state destination under one throwaway root for the
/// lifetime of the value, and restores the previous environment exactly on
/// drop — including variables that were UNSET, which are removed again
/// rather than left behind as the empty string.
///
/// The caller must hold `#[serial_test::serial]`: this mutates
/// process-global environment, and `std::env::set_var` is `unsafe` in
/// Rust 2024 for precisely that reason.
///
/// ```ignore
/// #[test]
/// #[serial_test::serial]
/// fn writes_nothing_into_the_operators_tree() {
///     let state = IsolatedState::new();
///     // … exercise code that writes darkmux state …
///     assert!(some_written_path.starts_with(state.path()));
/// }
/// ```
pub struct IsolatedState {
    tmp: tempfile::TempDir,
    /// Every variable this guard touched, with the value it displaced.
    /// `None` means the variable was unset and must be removed again.
    prev: Vec<(&'static str, Option<OsString>)>,
}

impl IsolatedState {
    /// Pin everything. Creates the root eagerly (a `TempDir`), then sets
    /// [`PINNED_STATE_VARS`] and removes [`CLEARED_STATE_VARS`].
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("IsolatedState: could not create a temp root");
        let root = tmp.path().to_path_buf();
        let mut prev: Vec<(&'static str, Option<OsString>)> = Vec::new();

        for (var, subpath) in PINNED_STATE_VARS {
            prev.push((var, std::env::var_os(var)));
            let target = if subpath.is_empty() { root.clone() } else { root.join(subpath) };
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::set_var(var, &target) };
        }
        for var in CLEARED_STATE_VARS {
            prev.push((var, std::env::var_os(var)));
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::remove_var(var) };
        }

        Self { tmp, prev }
    }

    /// The throwaway root. Every pinned destination is at or under it.
    pub fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// A path under the isolated root, for a test that wants to plant a
    /// fixture where a resolver will find it.
    pub fn join(&self, sub: impl AsRef<Path>) -> PathBuf {
        self.tmp.path().join(sub)
    }
}

impl Default for IsolatedState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IsolatedState {
    fn drop(&mut self) {
        for (var, prev) in &self.prev {
            // SAFETY: caller holds #[serial_test::serial].
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two lists must stay disjoint — a variable that is both pinned
    /// and cleared would have its restore entry pushed twice and its
    /// final value decided by list order, which is exactly the kind of
    /// silent ordering dependence this guard exists to remove.
    #[test]
    fn the_pinned_and_cleared_sets_do_not_overlap() {
        for (var, _) in PINNED_STATE_VARS {
            assert!(
                !CLEARED_STATE_VARS.contains(var),
                "{var} is both pinned and cleared; pick one"
            );
        }
    }

    /// [`PINNED_STATE_VARS`] is the SPECIFICATION of what isolation means
    /// here, so it gets an assertion rather than only a comment: while the
    /// guard lives, every entry must resolve under the guard's root, at
    /// its documented subpath, and never into the operator's real tree.
    ///
    /// This is what makes each entry load-bearing. Measured during the
    /// fix: deleting the `DARKMUX_FLOWS_DIR` entry alone left BOTH the
    /// keystone test and the real leak probe green, because `DARKMUX_HOME`
    /// is pinned too and `flows_dir_default()` re-roots off it — so every
    /// individual entry looks removable when judged only by "does the
    /// suite still pass". That reasoning is how a list like this erodes
    /// one entry at a time until a build WITHOUT
    /// `darkmux-types/test-support` — where `config()` reads the
    /// operator's real `config.json` and `config.dirs.<x>` outranks the
    /// root-derived default — leaks again. The list is the contract; this
    /// test holds the contract.
    #[test]
    #[serial_test::serial]
    fn every_pinned_variable_points_under_the_isolated_root() {
        let real = dirs::home_dir().map(|h| h.join(".darkmux"));
        let state = IsolatedState::new();
        for (var, subpath) in PINNED_STATE_VARS {
            let value = std::env::var_os(var)
                .unwrap_or_else(|| panic!("{var} is listed in PINNED_STATE_VARS but was not set"));
            let path = PathBuf::from(value);
            assert!(
                path.starts_with(state.path()),
                "{var} must resolve under the isolated root, got {}",
                path.display()
            );
            let expected =
                if subpath.is_empty() { state.path().to_path_buf() } else { state.join(subpath) };
            assert_eq!(path, expected, "{var} must pin to its documented subpath");
            if let Some(real) = real.as_ref() {
                assert!(
                    !path.starts_with(real),
                    "{var} pinned into the operator's real tree: {}",
                    path.display()
                );
            }
        }
    }

    /// The other list's contract: a cleared variable must be ABSENT while
    /// the guard lives. `DARKMUX_AUDIT_DIR` is the one that matters most —
    /// its mere presence is what turns the hash-chained audit sink on — so
    /// "cleared" has to mean removed, never set-to-empty.
    #[test]
    #[serial_test::serial]
    fn every_cleared_variable_is_absent_while_the_guard_lives() {
        let saved: Vec<(&str, Option<OsString>)> =
            CLEARED_STATE_VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
        // SAFETY: #[serial]. Give each one a value first, so "absent" is a
        // real observation rather than a coincidence of the ambient shell.
        unsafe {
            for var in CLEARED_STATE_VARS {
                std::env::set_var(var, "/darkmux-sentinel-cleared");
            }
        }
        {
            let _state = IsolatedState::new();
            for var in CLEARED_STATE_VARS {
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} is listed in CLEARED_STATE_VARS but survived the guard"
                );
            }
        }
        // SAFETY: #[serial].
        unsafe {
            for (var, prev) in saved {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    /// Restoration is the half a guard can silently lose (#2698 finding
    /// 1: a restore made a no-op left the suite fully green, 100 passed).
    /// So it is asserted per variable, for BOTH shapes — a variable that
    /// had a value must get that exact value back, and one that was unset
    /// must be UNSET again, not left as an empty string.
    #[test]
    #[serial_test::serial]
    fn every_touched_variable_is_restored_exactly() {
        // One of each shape, chosen from the two lists so the test covers
        // the pinned path and the cleared path.
        const HAD_VALUE: &str = "DARKMUX_FLOWS_DIR";
        const WAS_UNSET: &str = "DARKMUX_MODS_DIR";
        const CLEARED_WITH_VALUE: &str = "DARKMUX_AUDIT_DIR";

        let saved: Vec<(&str, Option<OsString>)> = [HAD_VALUE, WAS_UNSET, CLEARED_WITH_VALUE]
            .iter()
            .map(|v| (*v, std::env::var_os(v)))
            .collect();

        // SAFETY: #[serial].
        unsafe {
            std::env::set_var(HAD_VALUE, "/darkmux-sentinel-flows");
            std::env::remove_var(WAS_UNSET);
            std::env::set_var(CLEARED_WITH_VALUE, "/darkmux-sentinel-audit");
        }

        {
            let state = IsolatedState::new();
            assert!(
                PathBuf::from(std::env::var(HAD_VALUE).unwrap()).starts_with(state.path()),
                "a pinned variable must resolve under the isolated root while the guard lives"
            );
            assert!(
                PathBuf::from(std::env::var(WAS_UNSET).unwrap()).starts_with(state.path()),
                "a pinned variable is pinned whether or not it had a previous value"
            );
            assert!(
                std::env::var_os(CLEARED_WITH_VALUE).is_none(),
                "a cleared variable must be REMOVED while the guard lives — \
                 DARKMUX_AUDIT_DIR's presence is what enables the audit sink"
            );
        }

        assert_eq!(
            std::env::var_os(HAD_VALUE).as_deref(),
            Some(std::ffi::OsStr::new("/darkmux-sentinel-flows")),
            "the displaced value must come back byte-for-byte"
        );
        assert!(
            std::env::var_os(WAS_UNSET).is_none(),
            "a variable that was UNSET must be removed again, not left as an empty string"
        );
        assert_eq!(
            std::env::var_os(CLEARED_WITH_VALUE).as_deref(),
            Some(std::ffi::OsStr::new("/darkmux-sentinel-audit")),
            "a cleared variable's displaced value must come back too"
        );

        // SAFETY: #[serial]. Put the ambient environment back.
        unsafe {
            for (var, prev) in saved {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}
