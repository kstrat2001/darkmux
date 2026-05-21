//! Shared `--workdir` validation — symlink-escape guard + canonical-path
//! resolution. Hoisted from `crew/dispatch_internal.rs` so the openclaw
//! path (`crew/dispatch.rs::apply_workdir_override`) and the internal
//! runtime path (`crew/dispatch_internal.rs`) share one implementation,
//! and the worker side (`fleet.rs::handle_claimed_job`) can validate
//! queued `WorkJob.workdir` values before invoking dispatch.
//!
//! Flagged in PR-C.1, PR-C.2, AND PR-C.3 security reviews — without
//! this hoist, a remote publisher can target the (weaker) openclaw
//! path's symlink-following surface via the work queue, bypassing the
//! protection that's been in `dispatch_internal.rs` since #232.
//!
//! ## Algorithm
//!
//! For each component of the operator's `--workdir` path, check via
//! `symlink_metadata()` whether it's a symbolic link. Bail when ANY
//! operator-typed segment is a symlink — except for known macOS system
//! firmlinks (`/tmp`, `/var`, `/etc`) which operators traverse
//! routinely without thinking.
//!
//! The two-call structure (`first_user_symlink_in` + the wrapping
//! `validate_workdir`) keeps the symlink-detection logic pure (no I/O
//! side effects) for unit-testing, while the wrapping function bundles
//! the symlink check + `canonicalize()` + `is_dir()` into the
//! operator-facing API call sites use.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// Walk each component of an operator-typed `--workdir` path and return
/// the first symlink encountered that ISN'T a known macOS system
/// firmlink. Returns `Ok(None)` when no operator-named symlink is
/// present along the path. Returns the offending accumulated path so
/// the caller can name it in the error message.
///
/// The walk stops short (with `Ok(None)`) when a component doesn't
/// exist — the subsequent `canonicalize()` will surface that as the
/// canonical "does not exist" error.
pub fn first_user_symlink_in(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut acc = PathBuf::new();
    for component in path.components() {
        acc.push(component);
        let meta = match std::fs::symlink_metadata(&acc) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() && !is_macos_firmlink(&acc) {
            return Ok(Some(acc));
        }
    }
    Ok(None)
}

/// True for the macOS top-level firmlinks operators routinely traverse
/// without thinking. Deliberately narrow: only the three that real
/// `--workdir` paths cross (`/tmp`, `/var`, `/etc`). Other macOS
/// firmlinks (`/Applications`, `/Library`, `/Users`, `/Volumes`, ...)
/// aren't typical workdir destinations; if an operator hits one, the
/// bail is correct behavior. On Linux those paths are real
/// directories, so this never trips.
pub fn is_macos_firmlink(p: &Path) -> bool {
    matches!(p.to_str(), Some("/tmp" | "/var" | "/etc"))
}

/// Validate an operator-supplied `--workdir` for cross-machine OR local
/// dispatch:
///
/// 1. Walk components; bail if any operator-typed segment is a symlink
///    that isn't an allowed macOS firmlink. Closes the original #227
///    threat (operator surprised by indirection) AND prevents a remote
///    publisher from using the work queue to target a weaker
///    symlink-following path on a worker machine.
/// 2. Canonicalize — surfaces "doesn't exist" as the canonical error.
/// 3. Confirm it's a directory.
///
/// Returns the canonical `PathBuf` (always absolute, symlink-free).
/// Callers should use the returned canonical path for any subsequent
/// filesystem operation (docker volume mount, openclaw workspace
/// symlink, etc.) so the actual target is the explicit one validated
/// here.
///
/// Call sites:
/// - `crew::dispatch::apply_workdir_override` (openclaw path)
/// - `crew::dispatch_internal::dispatch` (internal runtime path)
/// - `fleet::handle_claimed_job` (worker side; validates `WorkJob.workdir`
///   before invoking the dispatch path that consumes it)
pub fn validate_workdir(path: &Path) -> Result<PathBuf> {
    if let Some(offending) = first_user_symlink_in(path)
        .with_context(|| format!("checking --workdir for symlinks: {}", path.display()))?
    {
        bail!(
            "--workdir traverses an operator-named symlink at {} — refusing to follow.\n  \
             Use the real directory path directly to prevent unintended r/w. \
             (macOS firmlinks /tmp, /var, /etc are tolerated; user-named symlinks anywhere \
             in the path are not.)",
            offending.display()
        );
    }
    let resolved = path.canonicalize().with_context(|| {
        format!(
            "--workdir path does not exist or cannot be resolved: {}",
            path.display()
        )
    })?;
    if !resolved.is_dir() {
        return Err(anyhow!(
            "--workdir path is not a directory: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn first_user_symlink_in_returns_none_for_real_path() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        assert!(first_user_symlink_in(&real).unwrap().is_none());
    }

    #[test]
    fn first_user_symlink_in_detects_leaf_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let sym = tmp.path().join("evilsym");
        symlink(&target, &sym).unwrap();
        assert_eq!(first_user_symlink_in(&sym).unwrap(), Some(sym));
    }

    #[test]
    fn first_user_symlink_in_detects_middle_component_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("child")).unwrap();
        let sym = tmp.path().join("sym");
        symlink(&real, &sym).unwrap();
        let probe = sym.join("child");
        assert_eq!(first_user_symlink_in(&probe).unwrap(), Some(sym));
    }

    #[test]
    fn first_user_symlink_in_returns_none_when_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(first_user_symlink_in(&missing).unwrap().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_tolerates_macos_tmp_firmlink() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        let path = std::path::PathBuf::from(format!("/tmp/dm_firmlink_test_{unique}"));
        std::fs::create_dir(&path).unwrap();
        let result = first_user_symlink_in(&path);
        let _ = std::fs::remove_dir(&path);
        assert!(result.unwrap().is_none(), "/tmp/foo must not trip on firmlink");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_still_catches_user_symlink_under_tmp() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        let target = std::path::PathBuf::from(format!("/tmp/dm_target_{unique}"));
        let sym = std::path::PathBuf::from(format!("/tmp/dm_sym_{unique}"));
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &sym).unwrap();
        let result = first_user_symlink_in(&sym);
        let _ = std::fs::remove_file(&sym);
        let _ = std::fs::remove_dir(&target);
        let offending = result.unwrap().expect("user symlink under /tmp must still be caught");
        assert!(offending.to_string_lossy().contains(&format!("dm_sym_{unique}")));
    }

    #[test]
    fn is_macos_firmlink_allowlist_is_narrow() {
        assert!(is_macos_firmlink(Path::new("/tmp")));
        assert!(is_macos_firmlink(Path::new("/var")));
        assert!(is_macos_firmlink(Path::new("/etc")));
        assert!(!is_macos_firmlink(Path::new("/tmp/sub")));
        assert!(!is_macos_firmlink(Path::new("/")));
        assert!(!is_macos_firmlink(Path::new("/home")));
        assert!(!is_macos_firmlink(Path::new("/Users")));
    }

    // ─── validate_workdir (the operator-facing API) ───────────────────

    #[test]
    fn validate_workdir_accepts_real_directory() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let resolved = validate_workdir(&real).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.is_dir());
    }

    #[test]
    fn validate_workdir_rejects_leaf_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let sym = tmp.path().join("evilsym");
        symlink(&target, &sym).unwrap();
        let err = validate_workdir(&sym).unwrap_err().to_string();
        assert!(
            err.contains("symlink") || err.contains("refusing"),
            "expected symlink-reject error; got: {err}"
        );
    }

    #[test]
    fn validate_workdir_rejects_missing_path() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = validate_workdir(&missing).unwrap_err().to_string();
        assert!(
            err.contains("does not exist") || err.contains("cannot be resolved"),
            "expected does-not-exist error; got: {err}"
        );
    }

    #[test]
    fn validate_workdir_rejects_file() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("not-a-dir");
        std::fs::write(&f, b"hi").unwrap();
        let err = validate_workdir(&f).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }
}
