//! Resolves the active darkmux workspace directory.
//!
//!   1. ./.darkmux/         — project-local (preferred when present)
//!   2. ~/.darkmux/         — cross-project user state (fallback)
//!
//! Lab runs, sandboxes, profiles, teams, and notebooks all live under one
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
    pub runs: PathBuf,
    pub sandboxes: PathBuf,
    pub teams: PathBuf,
    pub notebook: PathBuf,
    pub profiles: PathBuf,
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

pub fn resolve(scope: ResolveScope) -> DarkmuxPaths {
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

    // The notebook dir can be overridden via DARKMUX_NOTEBOOK_DIR — useful
    // for pointing notebook entries at an iCloud-synced (or otherwise
    // shared) path so multiple machines write to the same notebook. When
    // unset, falls back to the standard `<root>/notebook` location.
    //
    // Tilde expansion is supported for ergonomics — most operators write
    // `~/Library/...` rather than the literal expanded path.
    let notebook = env::var("DARKMUX_NOTEBOOK_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
        .unwrap_or_else(|| chosen.join("notebook"));

    DarkmuxPaths {
        runs: chosen.join("runs"),
        sandboxes: chosen.join("sandboxes"),
        teams: chosen.join("teams"),
        notebook,
        profiles: chosen.join("profiles.yaml"),
        scope: chosen_scope,
        root: chosen,
    }
}

/// Expand a leading `~` to the user's home directory. Pass-through for
/// any other shape. Returns the original path unchanged if no home is
/// available (which is unusual; would mean a misconfigured environment).
fn expand_tilde(s: &str) -> PathBuf {
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
        &paths.teams,
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

    #[serial_test::serial]
    #[test]
    fn resolve_force_project_uses_cwd() {
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

    #[test]
    fn resolve_force_user_uses_home() {
        let paths = resolve(ResolveScope::ForceUser);
        assert_eq!(paths.scope, Scope::User);
        assert!(paths.root.ends_with(".darkmux"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_auto_prefers_project_when_present() {
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
            teams: tmp.path().join(".darkmux/teams"),
            notebook: tmp.path().join(".darkmux/notebook"),
            profiles: tmp.path().join(".darkmux/profiles.yaml"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        assert!(paths.root.exists());
        assert!(paths.runs.exists());
        assert!(paths.sandboxes.exists());
        assert!(paths.teams.exists());
        assert!(paths.notebook.exists());
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = DarkmuxPaths {
            root: tmp.path().join(".darkmux"),
            runs: tmp.path().join(".darkmux/runs"),
            sandboxes: tmp.path().join(".darkmux/sandboxes"),
            teams: tmp.path().join(".darkmux/teams"),
            notebook: tmp.path().join(".darkmux/notebook"),
            profiles: tmp.path().join(".darkmux/profiles.yaml"),
            scope: Scope::Project,
        };
        ensure(&paths).unwrap();
        ensure(&paths).unwrap(); // second call is a no-op
        assert!(paths.runs.exists());
    }
}
