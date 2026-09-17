//! Resolves the active darkmux workspace directory.
//!
//!   1. ./.darkmux/         — project-local (preferred when present)
//!   2. ~/.darkmux/         — cross-project user state (fallback)
//!
//! Lab runs, sandboxes, profiles, crews, and notebooks all live under one
//! of these. Relative paths only — never absolute paths in any shipped
//! manifest.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

/// `profiles` is the canonical registry path (`<root>/profiles.json`).
/// Reserved public-API surface — the active loader in `profiles.rs`
/// has its own resolution today, but downstream tools that want the
/// canonical location read it from here.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DarkmuxPaths {
    pub root: PathBuf,
    /// The lab-run root. `pub(crate)` ON PURPOSE (#1882): outside this crate,
    /// go through `config_access::lab_dir()`, which adds the env/config tiers
    /// and the test-build isolation (#994) that this raw field has neither of.
    /// Three call sites resolved this directly and each one wrote lab runs to a
    /// root the lab reader does not scan; one of them put real run directories
    /// into the operator's ~/.darkmux/runs from `cargo test`. Making the bypass
    /// unrepresentable is cheaper than remembering not to take it.
    pub(crate) runs: PathBuf,
    pub sandboxes: PathBuf,
    pub crew: PathBuf,
    pub notebook: PathBuf,
    pub profiles: PathBuf,
    /// (#661) The config.json location (`<root>/config.json`). The config
    /// subsystem reads + `darkmux init` writes here.
    pub config: PathBuf,
    pub scope: Scope,
}

/// `ForceProject` / `ForceUser` are used in tests (which the release-mode
/// dead-code lint doesn't see) and reserved for explicit-override
/// callers (e.g. an agent that wants to write a notebook entry into a
/// specific scope regardless of the default Auto-resolve).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub enum ResolveScope {
    #[default]
    Auto,
    ForceProject,
    ForceUser,
}

/// Test-only constructor: every darkmux directory under one throwaway root.
///
/// Exists because `runs` is `pub(crate)` (#1882), so tests in OTHER crates can
/// no longer build this struct literally — which is the point: production code
/// must reach the lab root through `config_access::lab_dir()`. Tests still need
/// a fully tmp-rooted value, and this gives them one without reopening the
/// field to the whole workspace.
#[cfg(any(test, feature = "test-support"))]
impl DarkmuxPaths {
    pub fn under_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        DarkmuxPaths {
            runs: root.join("runs"),
            sandboxes: root.join("sandboxes"),
            crew: root.join("crew"),
            notebook: root.join("notebook"),
            profiles: root.join("profiles.json"),
            config: root.join("config.json"),
            scope: Scope::User,
            root,
        }
    }
}

/// (#2777) The scratch-dir prefix every test-build fallback root sits
/// under. Kept as its own constant because `darkmux-doctor`'s temp-residue
/// check reports on it by family name (`temp_residue_family` strips the
/// trailing pid segment back to exactly this string), and a rename here
/// with a stale literal there would silently stop reporting.
#[cfg(any(test, feature = "test-support"))]
pub const TEST_ISOLATED_DIR_NAME: &str = "darkmux-test-isolated";

/// (#2777) The scratch root a test / `test-support` build falls back to when
/// `DARKMUX_HOME` is unset and nothing else isolated the process:
/// `<system temp>/darkmux-test-isolated-<pid>`.
///
/// # What this guarantees, and what it does not
///
/// **It guarantees:** a test that forgot to isolate itself never resolves
/// onto the operator's real `~/.darkmux`. That is #2653's guarantee and it
/// is preserved here EXACTLY — this is still, unconditionally, not a home
/// directory. #2653 was a good fix for a genuinely worse problem:
/// `dispatch_liveness`'s prune pass DELETES files, so an un-isolated test
/// did not merely leave a stray file behind, it destroyed real operator
/// history (proved 2026-09-11 — one unrelated unit test deleted three
/// seeded heartbeat files outright).
///
/// **It did NOT guarantee, before this change:** isolation of test
/// PROCESSES from each other. The old fallback was a single FIXED,
/// machine-global path, so every un-isolated test process on the machine
/// shared one directory — and the name `darkmux-test-isolated` invited
/// reading it as though it did more than it does. Measured on one laptop:
/// 260 liveness entries and 1.9 MB of residue, across TEN separate shared
/// subtrees (liveness, acks, audit, hooks, flows, findings, mods, runs,
/// runtime, cache) declared independently in four crates. A forgotten
/// `DARKMUX_HOME` shared not just heartbeats but flow records, findings,
/// mods, run artifacts and the audit chain with every concurrent test
/// process.
///
/// The `-<pid>` suffix closes that everywhere at once, because all ten of
/// those sites now resolve through this ONE function instead of repeating
/// the literal — and because the helper it delegates to also SWEEPS, the
/// 1.9 MB drains rather than being re-spread one directory per process.
///
/// # Why a per-pid split is safe here
///
/// Checked rather than assumed: nothing depends on a parent process and a
/// spawned child both FALLING THROUGH to the same shared path. Every
/// binary-spawning integration test pins `DARKMUX_HOME` explicitly
/// (`tests/e2e/harness.rs`, plus `lab_concurrent_register_no_lost_writes`,
/// `state_leak_execution_guard`, `state_files_owner_only_mode`,
/// `fleet_concurrent_add_no_lost_writes`), and an explicit `DARKMUX_HOME`
/// wins over this fallback in every accessor, parent and child alike. And
/// `test-support` is a `[dev-dependencies]`-only feature, so a SHIPPED
/// binary never resolves this at all.
///
/// # Why this delegates to [`crate::test_isolation::process_scratch_dir`]
///
/// Because that is already the repo's answer to "a throwaway directory
/// owned by this process", and it answers two things a hand-rolled
/// `temp_dir().join(…).join(pid)` does not:
///
/// * **It collects itself.** `atexit(3)` removes it on a normal exit, and
///   the creation path sweeps siblings whose owning process is gone — so a
///   hard kill (`.config/nextest.toml`'s `terminate-after`, which CLAUDE.md
///   records firing twice) bounds the population instead of growing it.
///   Without that, splitting one shared directory into one-per-pid would
///   have traded a shared tree for an unbounded pile of private ones, which
///   is not obviously the better failure.
/// * **It starts empty**, removing any tree a recycled pid inherited, so a
///   test never reads a dead process's leftovers as its own state.
///
/// The name it produces is `<system temp>/darkmux-test-isolated-<pid>`.
/// `std::env::temp_dir()` rather than a literal `/tmp` matters for two
/// reasons of its own: `/tmp` is world-writable and shared between
/// accounts, so a `darkmux-test-isolated` owned by another user is a
/// permission failure nothing here can recover from, while `temp_dir()` is
/// the per-user location the platform nominates (and honors `TMPDIR`); and
/// it is the same root `darkmux doctor`'s temp-residue check scans, so the
/// directory is VISIBLE to the operator's housekeeping rather than sitting
/// where that check never looks.
///
/// **`darkmux doctor` needs no change for the new shape**, which is worth
/// stating because it was an open question on the issue: `temp_residue_family`
/// already strips a trailing all-digits segment, so
/// `darkmux-test-isolated-41213` reports under the family
/// `darkmux-test-isolated` — the exact string that check's own unit test
/// already pins.
#[cfg(any(test, feature = "test-support"))]
pub fn test_isolated_root() -> PathBuf {
    crate::test_isolation::process_scratch_dir(TEST_ISOLATED_DIR_NAME)
}

/// (#2777) [`test_isolated_root`] with one named subdirectory — the form
/// every call site actually wants (`…/flows`, `…/acks`, `…/audit`).
#[cfg(any(test, feature = "test-support"))]
pub fn test_isolated_dir(name: &str) -> PathBuf {
    test_isolated_root().join(name)
}

pub fn resolve(scope: ResolveScope) -> DarkmuxPaths {
    // (#661) DARKMUX_HOME is the bootstrap pointer — it overrides the darkmux
    // root directory entirely (a relocated install, or test isolation), and
    // wins over the project/user auto-resolve below. The pointer can't live
    // inside the config it locates, so it stays a direct env read. Tilde-
    // expanded for ergonomics.
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_HOME");
    if let Some(root) = env::var("DARKMUX_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
    {
        return paths_from_root(root, Scope::User);
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_root = cwd.join(".darkmux");
    let user_root = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".darkmux");

    let (chosen, chosen_scope) = match scope {
        ResolveScope::ForceProject => (project_root, Scope::Project),
        ResolveScope::ForceUser => (user_root, Scope::User),
        ResolveScope::Auto => {
            if project_root.exists() {
                (project_root, Scope::Project)
            } else {
                (user_root, Scope::User)
            }
        }
    };

    paths_from_root(chosen, chosen_scope)
}

/// Build the full `DarkmuxPaths` from a chosen root, applying the per-dir
/// env overrides. Shared by the `DARKMUX_HOME` override path and the normal
/// project/user resolution so both stay in sync.
fn paths_from_root(chosen: PathBuf, chosen_scope: Scope) -> DarkmuxPaths {
    // The notebook dir can be overridden via DARKMUX_NOTEBOOK_DIR — useful
    // for pointing notebook entries at an iCloud-synced (or otherwise
    // shared) path so multiple machines write to the same notebook. When
    // unset, falls back to the standard `<root>/notebook` location.
    //
    // Tilde expansion is supported for ergonomics — most operators write
    // `~/Library/...` rather than the literal expanded path.
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_NOTEBOOK_DIR");
    let notebook = env::var("DARKMUX_NOTEBOOK_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
        .unwrap_or_else(|| chosen.join("notebook"));

    DarkmuxPaths {
        runs: chosen.join("runs"),
        sandboxes: chosen.join("sandboxes"),
        crew: chosen.join("crew"),
        notebook,
        profiles: chosen.join("profiles.json"),
        config: chosen.join("config.json"),
        scope: chosen_scope,
        root: chosen,
    }
}

/// Expand a leading `~` to the user's home directory. Pass-through for
/// any other shape. Returns the original path unchanged if no home is
/// available (which is unusual; would mean a misconfigured environment).
/// `pub(crate)` so `config_access` can expand `~` in `config.dirs.*` values
/// (#661 Slice 3).
pub(crate) fn expand_tilde(s: &str) -> PathBuf {
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

pub fn ensure(paths: &DarkmuxPaths) -> Result<()> {
    for p in [
        &paths.root,
        &paths.runs,
        &paths.sandboxes,
        &paths.crew,
        &paths.notebook,
    ] {
        if !p.exists() {
            fs::create_dir_all(p)
                .with_context(|| format!("creating {}", p.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// (#2643) `resolve` reads `DARKMUX_HOME` FIRST, unconditionally,
    /// BEFORE it ever looks at `scope` — so an ambient `DARKMUX_HOME` in
    /// the shell running `cargo test` silently pre-empts every scope this
    /// module exercises (`ForceProject`, `ForceUser`, and `Auto`'s own
    /// cwd-vs-home branch), not just `resolve_honors_darkmux_home_override`,
    /// which means to test the override itself. Reproduced directly:
    /// `DARKMUX_HOME=/tmp/w24a-scratch-home cargo test -p darkmux-types
    /// --lib paths::tests` failed `resolve_force_project_uses_cwd`,
    /// `resolve_force_user_uses_home`, and
    /// `resolve_auto_prefers_project_when_present` — each asserting a root
    /// ending in `.darkmux` (the scratch path doesn't) or a `Scope` the
    /// override silently swapped. Structural fix: clear `DARKMUX_HOME` for
    /// the duration of any test that means to exercise the NON-override
    /// resolution path, rather than assume the ambient shell already has
    /// it unset. RAII (not raw save/clear/restore) so a panicking
    /// assertion still restores the prior value.
    struct ClearDarkmuxHomeGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl ClearDarkmuxHomeGuard {
        fn new() -> Self {
            let prev = env::var_os("DARKMUX_HOME");
            unsafe { env::remove_var("DARKMUX_HOME") };
            Self { prev }
        }
    }

    impl Drop for ClearDarkmuxHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_HOME", v),
                    None => env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn resolve_force_project_uses_cwd() {
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let tmp = TempDir::new().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();

        let paths = resolve(ResolveScope::ForceProject);
        assert_eq!(paths.scope, Scope::Project);
        assert!(
            paths.root.starts_with(&canonical_tmp) || paths.root.starts_with(tmp.path()),
            "root {:?} doesn't start with tmp {:?} or canonical {:?}",
            paths.root,
            tmp.path(),
            canonical_tmp
        );
        assert!(paths.root.ends_with(".darkmux"));

        env::set_current_dir(prev).unwrap();
    }

    // `resolve` reads DARKMUX_HOME, which serial siblings in this module
    // mutate — without the serial guard this can observe
    // resolve_honors_darkmux_home_override's tempdir root and fail the
    // `.darkmux` suffix assertion.
    #[serial_test::serial]
    #[test]
    fn resolve_force_user_uses_home() {
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let paths = resolve(ResolveScope::ForceUser);
        assert_eq!(paths.scope, Scope::User);
        assert!(paths.root.ends_with(".darkmux"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_auto_prefers_project_when_present() {
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join(".darkmux");
        fs::create_dir_all(&project_root).unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();

        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();

        let paths = resolve(ResolveScope::Auto);
        assert_eq!(paths.scope, Scope::Project);
        assert!(
            paths.root.starts_with(&canonical_tmp) || paths.root.starts_with(tmp.path()),
            "root {:?} doesn't start with canonical tmp {:?}",
            paths.root,
            canonical_tmp
        );

        env::set_current_dir(prev).unwrap();
    }

    #[serial_test::serial]
    #[test]
    fn resolve_auto_falls_back_to_user_when_project_missing() {
        // (#2643) Didn't hard-fail under an ambient `DARKMUX_HOME` in the
        // repro run above (the override happens to also land on
        // `Scope::User`), but that is a VACUOUS pass, not a real one — the
        // assertion would still go green even if the actual fallback
        // branch this test names were broken. Same guard as the three
        // tests above, for the same reason.
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let tmp = TempDir::new().unwrap();
        // Crucially do NOT create .darkmux in tmp.
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();

        let paths = resolve(ResolveScope::Auto);
        assert_eq!(paths.scope, Scope::User);

        env::set_current_dir(prev).unwrap();
    }

    #[serial_test::serial]
    #[test]
    fn resolve_honors_darkmux_notebook_dir_env_var() {
        let tmp = TempDir::new().unwrap();
        let custom = tmp.path().join("iCloud-Drive").join("darkmux-notebook");
        let prev = env::var("DARKMUX_NOTEBOOK_DIR").ok();
        unsafe { env::set_var("DARKMUX_NOTEBOOK_DIR", &custom); }

        let paths = resolve(ResolveScope::ForceUser);
        assert_eq!(paths.notebook, custom, "notebook dir should be the env-var value");
        // Other paths still resolve to the user root, not the custom path.
        assert!(paths.runs.ends_with("runs"));
        assert!(!paths.runs.starts_with(tmp.path()));

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_NOTEBOOK_DIR", v),
                None => env::remove_var("DARKMUX_NOTEBOOK_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn resolve_falls_back_when_env_var_empty() {
        let prev = env::var("DARKMUX_NOTEBOOK_DIR").ok();
        unsafe { env::set_var("DARKMUX_NOTEBOOK_DIR", ""); }

        let paths = resolve(ResolveScope::ForceUser);
        // Empty env var should NOT override; notebook stays at <root>/notebook.
        assert!(paths.notebook.ends_with("notebook"));
        assert!(paths.notebook.starts_with(&paths.root));

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_NOTEBOOK_DIR", v),
                None => env::remove_var("DARKMUX_NOTEBOOK_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn resolve_honors_darkmux_home_override() {
        let tmp = TempDir::new().unwrap();
        let custom_root = tmp.path().join("relocated-darkmux");
        let prev = env::var("DARKMUX_HOME").ok();
        unsafe { env::set_var("DARKMUX_HOME", &custom_root); }

        // DARKMUX_HOME (#661) wins over the project/user auto-resolve and IS
        // the root directly — config + profiles hang off it.
        let paths = resolve(ResolveScope::Auto);
        assert_eq!(paths.root, custom_root, "DARKMUX_HOME overrides the root");
        assert_eq!(paths.config, custom_root.join("config.json"));
        assert_eq!(paths.profiles, custom_root.join("profiles.json"));
        assert_eq!(paths.scope, Scope::User);

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_HOME", v),
                None => env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn expand_tilde_handles_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            let expanded = expand_tilde("~/foo/bar");
            assert_eq!(expanded, home.join("foo").join("bar"));
            let just_tilde = expand_tilde("~");
            assert_eq!(just_tilde, home);
        }
        // Non-tilde paths pass through unchanged.
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("relative/path"), PathBuf::from("relative/path"));
    }

    #[test]
    fn ensure_creates_all_subdirs() {
        let tmp = TempDir::new().unwrap();
        let paths = DarkmuxPaths {
            root: tmp.path().join(".darkmux"),
            runs: tmp.path().join(".darkmux/runs"),
            sandboxes: tmp.path().join(".darkmux/sandboxes"),
            crew: tmp.path().join(".darkmux/crew"),
            notebook: tmp.path().join(".darkmux/notebook"),
            profiles: tmp.path().join(".darkmux/profiles.json"),
            config: tmp.path().join(".darkmux/config.json"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        assert!(paths.root.exists());
        assert!(paths.runs.exists());
        assert!(paths.sandboxes.exists());
        assert!(paths.crew.exists());
        assert!(paths.notebook.exists());
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = DarkmuxPaths {
            root: tmp.path().join(".darkmux"),
            runs: tmp.path().join(".darkmux/runs"),
            sandboxes: tmp.path().join(".darkmux/sandboxes"),
            crew: tmp.path().join(".darkmux/crew"),
            notebook: tmp.path().join(".darkmux/notebook"),
            profiles: tmp.path().join(".darkmux/profiles.json"),
            config: tmp.path().join(".darkmux/config.json"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        ensure(&paths).unwrap(); // second call is a no-op
        assert!(paths.runs.exists());
    }
    // ── (#2777) the test-build scratch root ──

    /// The guarantee #2653 bought and #2777 had to preserve exactly: this
    /// is never the operator's real `~/.darkmux`. An un-isolated test that
    /// touches a liveness call site PRUNES files, so a regression here does
    /// not leave a stray file behind, it deletes real operator history.
    #[test]
    fn the_scratch_root_is_never_the_operators_real_darkmux_home() {
        let root = test_isolated_root();
        let real = dirs::home_dir().map(|h| h.join(".darkmux"));
        assert_ne!(Some(root.clone()), real);
        if let Some(home) = dirs::home_dir() {
            assert!(
                !root.starts_with(&home) || root.starts_with(std::env::temp_dir()),
                "the scratch root must live in the system temp root, not under HOME: {}",
                root.display()
            );
        }
        assert!(
            root.starts_with(std::env::temp_dir()),
            "{} is not under the system temp root",
            root.display()
        );
    }

    /// The property #2653 did NOT have and the name implied: separation
    /// between test PROCESSES. The old fallback was one fixed
    /// machine-global path, so every un-isolated process shared flow
    /// records, findings, mods, run artifacts, the audit chain and dispatch
    /// acks with every other one.
    #[test]
    fn the_scratch_root_is_scoped_to_this_process() {
        let root = test_isolated_root();
        assert_eq!(
            root.file_name().and_then(|s| s.to_str()),
            Some(format!("{TEST_ISOLATED_DIR_NAME}-{}", std::process::id()).as_str()),
            "the name must carry this process's pid, or two concurrent test \
             processes share one tree again"
        );
        assert_eq!(
            root.parent(),
            Some(std::env::temp_dir().as_path()),
            "it sits directly in the temp root darkmux doctor's residue check \
             scans, so the tree is visible to the operator's housekeeping"
        );
    }

    /// The residue question the issue left open, answered as an assertion
    /// rather than as prose: `darkmux doctor`'s residue check groups by
    /// FAMILY, and its family derivation strips a trailing all-digits
    /// segment — so every per-pid root reports under the one family name
    /// that check's own unit test already pins. No doctor change is needed
    /// for the new shape, and this fails if either side drifts.
    #[test]
    fn every_per_pid_root_reports_under_the_one_family_name_doctor_knows() {
        let name = test_isolated_root()
            .file_name()
            .and_then(|s| s.to_str())
            .expect("a utf-8 directory name")
            .to_string();
        // The same derivation `darkmux_doctor::temp_residue_family` applies:
        // split on `-`, drop all-digit segments, rejoin.
        let family: Vec<&str> = name
            .split('-')
            .filter(|seg| !seg.is_empty() && !seg.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        // The LITERAL, not `TEST_ISOLATED_DIR_NAME` — comparing against the
        // constant the name was built from would be tautological in exactly
        // the thing being pinned. This string is the one
        // `darkmux-doctor`'s own `temp_residue_family` unit test asserts, so
        // this is a genuine cross-file pin: renaming the prefix here without
        // teaching doctor's check about it turns this red.
        assert_eq!(family.join("-"), "darkmux-test-isolated");
        // And the namespace prefix the check filters on before it ever
        // derives a family — an entry not starting `darkmux-`/`dmx-` is
        // skipped outright, so a rename out of the namespace would make the
        // whole tree invisible to the residue report rather than mis-grouped.
        assert!(name.starts_with("darkmux-"), "{name} must be in the darkmux namespace");
    }

    /// Every subtree that used to declare the literal independently now
    /// derives from ONE root — which is what makes "fix it once, not ten
    /// times" true rather than aspirational.
    #[test]
    fn every_named_subtree_hangs_off_the_one_root() {
        let root = test_isolated_root();
        for name in ["hooks", "flows", "findings", "mods", "runs", "runtime", "cache", "acks", "audit"] {
            let d = test_isolated_dir(name);
            assert_eq!(d.parent(), Some(root.as_path()), "{name} must hang off the shared root");
            assert_eq!(d.file_name().and_then(|s| s.to_str()), Some(name));
        }
    }

    /// Stable within a process: two calls in the same run must agree, or a
    /// writer and a reader inside one test would land in different trees.
    #[test]
    fn the_scratch_root_is_stable_within_one_process() {
        assert_eq!(test_isolated_root(), test_isolated_root());
    }
}
