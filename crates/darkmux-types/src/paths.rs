//! Resolves the active darkmux workspace directory.
//!
//!   1. `$DARKMUX_HOME`     — the one relocation
//!   2. `~/.darkmux/`       — the default
//!
//! A `./.darkmux/` in the working directory is never consulted: it does not
//! move the root (4.0). Lab runs, sandboxes, and profiles all live under
//! the root. Relative paths only — never absolute paths in any shipped
//! manifest.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

/// The resolved darkmux directories. `profiles` is the canonical registry
/// path (`<root>/profiles.json`).
#[derive(Debug, Clone)]
pub struct DarkmuxPaths {
    pub root: PathBuf,
    /// The lab-run root. `pub(crate)` ON PURPOSE (#1882): outside this crate,
    /// go through `config_access::lab_dir()`, which adds the env/config tiers
    /// and the test-build isolation (#994) that this raw field has neither of.
    /// Three call sites resolved this directly and each one wrote lab runs to a
    /// root the lab reader does not scan; one of them put real run directories
    /// into the operator's ~/.darkmux/lab from `cargo test`. Making the bypass
    /// unrepresentable is cheaper than remembering not to take it.
    pub(crate) lab: PathBuf,
    pub sandboxes: PathBuf,
    pub profiles: PathBuf,
    /// (#661) The config.json location (`<root>/config.json`). The config
    /// subsystem reads + `darkmux init` writes here.
    pub config: PathBuf,
    pub scope: Scope,
}

/// Which root [`resolve`] returns. There is no auto-detecting variant: a
/// `./.darkmux/` in the working directory never becomes the root on its own.
/// `ForceProject` exists for the per-repo lessons database only.
#[derive(Debug, Clone, Copy)]
pub enum ResolveScope {
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
            lab: root.join("lab"),
            sandboxes: root.join("sandboxes"),
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
/// subtrees (liveness, audit, hooks, flows, findings, mods, runs,
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
/// every call site actually wants (`…/flows`, `…/audit`).
#[cfg(any(test, feature = "test-support"))]
pub fn test_isolated_dir(name: &str) -> PathBuf {
    test_isolated_root().join(name)
}

/// Suffix of a dispatch out-dir's host-only resume-origin record (#2972). The
/// record lives BESIDE the out-dir (`<out-dir>.resume_origin.json`, in its
/// parent), never inside the directory the container mounts read-write.
pub const RESUME_ORIGIN_SUFFIX: &str = ".resume_origin.json";

/// The host-only resume-origin record for `out_dir`: the one place the path is
/// derived. `None` when `out_dir` has no name to derive from.
pub fn resume_origin_record_path(out_dir: &Path) -> Option<PathBuf> {
    let parent = out_dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = out_dir.file_name()?.to_str()?;
    Some(parent.join(format!("{name}{RESUME_ORIGIN_SUFFIX}")))
}

/// Suffix of a dispatch out-dir's execution lock file. Like the resume-origin
/// record it sits BESIDE the out-dir, never inside the directory the
/// container mounts read-write: a model must not be able to delete or
/// replace the file whose lock keeps a second resume out.
pub const EXECUTION_LOCK_SUFFIX: &str = ".execution.lock";

/// The execution lock file for `out_dir`: the one place the path is derived.
/// `None` when `out_dir` has no name to derive from.
pub fn execution_lock_path(out_dir: &Path) -> Option<PathBuf> {
    let parent = out_dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = out_dir.file_name()?.to_str()?;
    Some(parent.join(format!("{name}{EXECUTION_LOCK_SUFFIX}")))
}

/// Remove an out-dir AND its sibling resume-origin record and execution lock
/// file, so no orphaned sibling outlives its directory. Every darkmux path
/// that removes a dispatch out-dir goes through here. A missing sibling is
/// fine.
pub fn remove_out_dir(dir: &Path) -> std::io::Result<()> {
    let result = std::fs::remove_dir_all(dir);
    for sibling in [resume_origin_record_path(dir), execution_lock_path(dir)].into_iter().flatten() {
        let _ = std::fs::remove_file(sibling);
    }
    result
}

/// Whether `name` (a directory-entry name) is a resume-origin record or an
/// execution lock file whose out-dir no longer exists in `parent`: an orphan
/// `darkmux doctor` counts.
pub fn is_orphaned_resume_origin(parent: &Path, name: &str) -> bool {
    [RESUME_ORIGIN_SUFFIX, EXECUTION_LOCK_SUFFIX]
        .iter()
        .filter_map(|suffix| name.strip_suffix(suffix))
        .any(|stem| !stem.is_empty() && !parent.join(stem).is_dir())
}

/// What a `<repo>/.darkmux/` directory legitimately holds: the files darkmux
/// still reads from a repo (the per-repo lessons database with its SQLite
/// side files, and `conventions.json`). Everything else in it is stranded.
const PER_REPO_FILES: &[&str] =
    &["lessons.db", "lessons.db-wal", "lessons.db-shm", "lessons.db-journal", "conventions.json"];

/// State in the working directory that darkmux ignores. 4.0 dropped
/// project-local discovery: only [`PER_REPO_FILES`] are read from a repo's
/// `.darkmux/`, and a `./.darkmux.json` registry is not read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredProjectState {
    /// Each stranded entry: a `./.darkmux/` child that is not a per-repo
    /// file, and a `./.darkmux.json`. Sorted.
    pub stranded: Vec<PathBuf>,
    /// The `./.darkmux/` directory when it holds a `config.json` or
    /// `profiles.json`, so `DARKMUX_HOME=<it>` would adopt real root state.
    pub adoptable_dir: Option<PathBuf>,
}

/// [`ignored_project_state_at`] for the process's working directory and
/// resolved root. `None` when nothing is stranded.
pub fn ignored_project_state() -> Option<IgnoredProjectState> {
    let cwd = env::current_dir().ok()?;
    ignored_project_state_at(&cwd, &resolve(ResolveScope::ForceUser).root)
}

fn ignored_project_state_at(cwd: &Path, root: &Path) -> Option<IgnoredProjectState> {
    let project = cwd.join(".darkmux");
    let same = |a: &Path, b: &Path| fs::canonicalize(a).ok() == fs::canonicalize(b).ok();
    let mut stranded: Vec<PathBuf> = Vec::new();
    let mut adoptable_dir = None;
    if project.is_dir() && !same(&project, root) {
        let entries = fs::read_dir(&project).into_iter().flatten().flatten();
        stranded.extend(
            entries
                .filter(|e| !PER_REPO_FILES.contains(&e.file_name().to_string_lossy().as_ref()))
                .map(|e| e.path()),
        );
        if project.join("config.json").is_file() || project.join("profiles.json").is_file() {
            adoptable_dir = Some(project);
        }
    }
    let registry = cwd.join(".darkmux.json");
    if registry.is_file() {
        stranded.push(registry);
    }
    stranded.sort();
    (!stranded.is_empty()).then_some(IgnoredProjectState { stranded, adoptable_dir })
}

pub fn resolve(scope: ResolveScope) -> DarkmuxPaths {
    // (#661) DARKMUX_HOME is the bootstrap pointer — it overrides the darkmux
    // root directory entirely (a relocated install, or test isolation), and
    // wins over the user default below. The pointer can't live
    // inside the config it locates, so it stays a direct env read. Tilde-
    // expanded for ergonomics; a blank value is unset, and a set one is used
    // trimmed.
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_HOME");
    if let Some(root) = env::var("DARKMUX_HOME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| expand_tilde(&s))
    {
        return paths_from_root(root, Scope::User);
    }

    let (chosen, chosen_scope) = match scope {
        ResolveScope::ForceProject => {
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (cwd.join(".darkmux"), Scope::Project)
        }
        ResolveScope::ForceUser => {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            (home.join(".darkmux"), Scope::User)
        }
    };

    paths_from_root(chosen, chosen_scope)
}

/// The darkmux root for the per-process registries that must work before any
/// config is read (`dispatch_liveness`, `residency_lease`): exactly
/// `resolve(ForceUser).root`, so they agree with every other directory on
/// where "home" is. In a test build a root that IS the operator's real
/// `~/.darkmux` is redirected to [`test_isolated_root`]: a test that forgot to
/// set `DARKMUX_HOME` must not touch real state, and these registries prune
/// (delete) old files on write (proved: an unrelated unit test deleted three
/// seeded heartbeat files). A test that isolated itself is honored verbatim.
pub fn user_root_guarded() -> PathBuf {
    let root = resolve(ResolveScope::ForceUser).root;
    #[cfg(any(test, feature = "test-support"))]
    if dirs::home_dir().map(|h| h.join(".darkmux")).as_ref() == Some(&root) {
        return test_isolated_root();
    }
    root
}

/// Build the full `DarkmuxPaths` from a chosen root, applying the per-dir
/// env overrides. Shared by the `DARKMUX_HOME` override path and the normal
/// project/user resolution so both stay in sync.
fn paths_from_root(chosen: PathBuf, chosen_scope: Scope) -> DarkmuxPaths {
    DarkmuxPaths {
        lab: chosen.join("lab"),
        sandboxes: chosen.join("sandboxes"),
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
    // Not the lab dir: its one writer (`lab run`) creates it per run.
    for p in [&paths.root, &paths.sandboxes] {
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
    /// module exercises (`ForceProject` and `ForceUser`), not just `resolve_honors_darkmux_home_override`,
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

    /// 4.0: a `./.darkmux/` in the working directory never becomes the root.
    /// The cwd holds one here and only the explicit `ForceProject` scope sees it.
    #[serial_test::serial]
    #[test]
    fn resolve_ignores_a_project_darkmux_dir() {
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();

        let user = resolve(ResolveScope::ForceUser);
        let project = resolve(ResolveScope::ForceProject);

        env::set_current_dir(prev).unwrap();
        assert_eq!(user.scope, Scope::User);
        assert_ne!(user.root, project.root, "the cwd dir is not the user root");
    }

    /// Only what darkmux does not read is stranded: the per-repo files are
    /// not, and a `config.json` / `profiles.json` makes the directory
    /// adoptable through `DARKMUX_HOME`.
    #[test]
    fn ignored_state_names_only_stranded_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("elsewhere");
        let project = tmp.path().join(".darkmux");
        fs::create_dir_all(&project).unwrap();
        for f in ["lessons.db", "lessons.db-wal", "conventions.json"] {
            fs::write(project.join(f), "x").unwrap();
        }
        assert_eq!(ignored_project_state_at(tmp.path(), &root), None, "per-repo files only");

        fs::write(project.join("profiles.json"), "{}").unwrap();
        fs::write(tmp.path().join(".darkmux.json"), "{}").unwrap();
        let state = ignored_project_state_at(tmp.path(), &root).unwrap();
        assert_eq!(state.stranded, vec![project.join("profiles.json"), tmp.path().join(".darkmux.json")]);
        assert_eq!(state.adoptable_dir, Some(project));
    }

    /// `DARKMUX_HOME` pointing at the cwd's `.darkmux` is the documented way
    /// to use it: the directory is the root, so its contents are not stranded.
    #[test]
    fn ignored_state_is_empty_when_the_root_is_the_project_dir() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join(".darkmux");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("config.json"), "{}").unwrap();
        assert_eq!(ignored_project_state_at(tmp.path(), &project), None);
    }

    /// `DARKMUX_HOME` is used trimmed, the same way it is tested for blank.
    #[serial_test::serial]
    #[test]
    fn darkmux_home_is_used_trimmed() {
        let _clear_home = ClearDarkmuxHomeGuard::new();
        let tmp = TempDir::new().unwrap();
        unsafe { env::set_var("DARKMUX_HOME", format!("  {}  ", tmp.path().display())) };
        let root = resolve(ResolveScope::ForceUser).root;
        assert_eq!(root, tmp.path());
    }

    #[serial_test::serial]
    #[test]
    fn resolve_honors_darkmux_home_override() {
        let tmp = TempDir::new().unwrap();
        let custom_root = tmp.path().join("relocated-darkmux");
        let prev = env::var("DARKMUX_HOME").ok();
        unsafe { env::set_var("DARKMUX_HOME", &custom_root); }

        // DARKMUX_HOME (#661) IS the root directly — config + profiles hang off it.
        let paths = resolve(ResolveScope::ForceUser);
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
            lab: tmp.path().join(".darkmux/lab"),
            sandboxes: tmp.path().join(".darkmux/sandboxes"),
            profiles: tmp.path().join(".darkmux/profiles.json"),
            config: tmp.path().join(".darkmux/config.json"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        assert!(paths.root.exists());
        assert!(!paths.lab.exists(), "the lab dir is created by its writer, not by ensure");
        assert!(paths.sandboxes.exists());
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = DarkmuxPaths {
            root: tmp.path().join(".darkmux"),
            lab: tmp.path().join(".darkmux/lab"),
            sandboxes: tmp.path().join(".darkmux/sandboxes"),
            profiles: tmp.path().join(".darkmux/profiles.json"),
            config: tmp.path().join(".darkmux/config.json"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        ensure(&paths).unwrap(); // second call is a no-op
        assert!(paths.sandboxes.exists());
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
    /// records, findings, mods, run artifacts, the audit chain with every other
    /// one.
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
        for name in ["hooks", "flows", "findings", "mods", "lab", "runtime", "cache", "audit"] {
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

#[cfg(test)]
mod origin_record_tests {
    use super::*;

    /// (#2972) Removing an out-dir through the cleanup path leaves no record.
    #[test]
    fn remove_out_dir_removes_the_sibling_record_too() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("darkmux-out-coder-1");
        fs::create_dir_all(&dir).unwrap();
        let record = resume_origin_record_path(&dir).unwrap();
        fs::write(&record, "{}").unwrap();
        remove_out_dir(&dir).unwrap();
        assert!(!dir.exists() && !record.exists(), "no orphaned record may outlive its dir");
    }

    #[test]
    fn remove_out_dir_also_removes_the_execution_lock_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("darkmux-out-coder-3");
        fs::create_dir_all(&dir).unwrap();
        let lock = execution_lock_path(&dir).unwrap();
        fs::write(&lock, "").unwrap();
        remove_out_dir(&dir).unwrap();
        assert!(!lock.exists(), "no orphaned lock file may outlive its dir");
    }

    #[test]
    fn a_lock_file_is_orphaned_only_when_its_dir_is_gone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let name = format!("darkmux-out-coder-4{EXECUTION_LOCK_SUFFIX}");
        assert!(is_orphaned_resume_origin(tmp.path(), &name));
        fs::create_dir_all(tmp.path().join("darkmux-out-coder-4")).unwrap();
        assert!(!is_orphaned_resume_origin(tmp.path(), &name));
    }

    #[test]
    fn a_record_is_orphaned_only_when_its_dir_is_gone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("darkmux-out-coder-2");
        let name = format!("darkmux-out-coder-2{RESUME_ORIGIN_SUFFIX}");
        assert!(is_orphaned_resume_origin(tmp.path(), &name));
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_orphaned_resume_origin(tmp.path(), &name));
        assert!(!is_orphaned_resume_origin(tmp.path(), "darkmux-out-coder-2.json"));
    }
}
