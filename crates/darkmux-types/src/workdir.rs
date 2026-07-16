//! Shared `--workdir` validation — symlink-escape guard + canonical-path
//! resolution. Hoisted from `crew/dispatch_internal.rs` so the internal
//! runtime path AND the runner side (`fleet.rs::handle_claimed_job`) share
//! one implementation and can validate queued `WorkJob.workdir` values
//! before invoking dispatch.
//!
//! Flagged in PR-C.1, PR-C.2, AND PR-C.3 security reviews — without this
//! hoist, a remote publisher could target a weaker symlink-following
//! surface via the work queue, bypassing the protection that's been in
//! `dispatch_internal.rs` since #232. (The legacy openclaw shell-out
//! path this originally guarded against was removed in #1405.)
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
///    symlink-following path on a runner machine.
/// 2. Canonicalize — surfaces "doesn't exist" as the canonical error.
/// 3. Confirm it's a directory.
///
/// Returns the canonical `PathBuf` (always absolute, symlink-free).
/// Callers should use the returned canonical path for any subsequent
/// filesystem operation (docker volume mount, etc.) so the actual target
/// is the explicit one validated here.
///
/// Call sites:
/// - `crew::dispatch_internal::dispatch` (internal runtime path)
/// - `fleet::handle_claimed_job` (runner side; validates `WorkJob.workdir`
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

/// Resolve the per-machine darkmux worktrees base directory:
/// `~/.darkmux/worktrees` (HOME-less fallback `/tmp/darkmux/worktrees`).
/// Mirrors `worktrees_base_dir()` from darkmux-serve and `worktrees_base()`
/// from mission_run — kept here so the queue-boundary validator can use it
/// without pulling in darkmux-serve as a dependency.
fn worktrees_base_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".darkmux").join("worktrees"))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/worktrees"))
}

/// Validate a workdir path for **queue-originated (remote) dispatches**.
///
/// This is a stricter variant of `validate_workdir()` that adds a
/// base-containment check: the resolved workdir must be under the
/// per-machine darkmux worktrees base (`~/.darkmux/worktrees`).
/// This prevents a tailnet publisher from bind-mounting an arbitrary
/// runner directory as `/workspace` (security #840).
///
/// Rules:
/// 1. Symlink guard — same as `validate_workdir()` (reject operator-named
///    symlinks; macOS firmlinks tolerated).
/// 2. Must exist and be a directory.
/// 3. **Must be contained under the worktrees base.**
///
/// (Absence is handled by the caller: when `WorkJob.workdir` is `None`
/// there's no override and this is never called.)
///
/// The worktrees base is darkmux-owned (`~/.darkmux/worktrees`). We
/// `create_dir_all` it before canonicalizing so containment compares two
/// canonical paths: a non-existent base canonicalizes to nothing, and a
/// non-canonical fallback would false-reject every valid workdir whose
/// parents cross a symlink (e.g. macOS firmlinks, where `resolved` is
/// always firmlink-resolved but the literal base path is not). Creating
/// the base is harmless and idempotent — the runner needs it to exist to
/// dispatch into it anyway.
///
/// Returns the canonical `PathBuf` on success.
pub fn validate_remote_workdir(path: &Path) -> Result<PathBuf> {
    let base = worktrees_base_dir();
    std::fs::create_dir_all(&base).with_context(|| {
        format!(
            "creating worktrees base for containment check: {}",
            base.display()
        )
    })?;
    let canon_base = base.canonicalize().with_context(|| {
        format!(
            "canonicalizing worktrees base for containment check: {}",
            base.display()
        )
    })?;
    validate_remote_workdir_in(path, &canon_base)
}

/// Containment validator with an injectable, **already-canonical** base.
///
/// Pure of any env/`HOME` read so the containment rules are unit-testable
/// without env-var mutation. `validate_remote_workdir` is the production
/// entry point that resolves + canonicalizes the real worktrees base and
/// delegates here. `canon_base` MUST be canonical (the public wrapper
/// guarantees this) — `resolved` is always canonical, and `starts_with`
/// is a lexical prefix test, so a non-canonical base would not match.
pub fn validate_remote_workdir_in(path: &Path, canon_base: &Path) -> Result<PathBuf> {
    // 1. Symlink guard (same as validate_workdir).
    if let Some(offending) = first_user_symlink_in(path)
        .with_context(|| format!("checking --workdir for symlinks: {}", path.display()))?
    {
        bail!(
            "--workdir traverses an operator-named symlink at {} — refusing to follow. \
             Queue-originated dispatches must use real directory paths under the \
             worktrees base.",
            offending.display()
        );
    }

    // 2. Must exist and be a directory.
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

    // 3. Must be contained under the worktrees base. Both paths are
    // canonical (resolved by `canonicalize`, canon_base by the caller),
    // so this also rejects `..`-escapes — canonicalize collapses them
    // before the prefix test runs.
    if !resolved.starts_with(canon_base) {
        bail!(
            "--workdir {} is outside the allowed worktrees base {}; \
             queue-originated dispatches must use paths under {}",
            resolved.display(),
            canon_base.display(),
            canon_base.display()
        );
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

    // ─── validate_remote_workdir_in (queue-originated dispatch guard) ───
    //
    // These exercise the containment rules through the pure inner function
    // with an injectable canonical base — no `HOME` mutation, so they run
    // in parallel without env-var races. One `#[serial]` test below covers
    // the public wrapper's base-resolution wiring.

    /// Create a temp worktrees-base + a real sub-dir under it, returning
    /// (tempdir-guard, canonical-base, sub-under-base). The base is
    /// canonicalized because containment compares against the always-
    /// canonical `resolved` (the wrapper guarantees the same in prod).
    fn base_and_sub() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("worktrees_base");
        std::fs::create_dir(&base).unwrap();
        let canon_base = base.canonicalize().unwrap();
        let sub = canon_base.join("my_worktree");
        std::fs::create_dir(&sub).unwrap();
        (tmp, canon_base, sub)
    }

    #[test]
    fn validate_remote_workdir_in_accepts_path_under_base() {
        let (_tmp, base, sub) = base_and_sub();
        let resolved = validate_remote_workdir_in(&sub, &base).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.is_dir());
        assert!(resolved.starts_with(&base));
    }

    #[test]
    fn validate_remote_workdir_in_rejects_path_outside_base() {
        let (_tmp, base, _sub) = base_and_sub();
        // An independent dir that is NOT under the base.
        let outside_tmp = TempDir::new().unwrap();
        let outside = outside_tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let err = validate_remote_workdir_in(&outside, &base)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("outside the allowed worktrees base"),
            "expected outside-base rejection; got: {err}"
        );
    }

    #[test]
    fn validate_remote_workdir_in_rejects_prefix_sibling() {
        // The single most security-load-bearing line is the `starts_with`
        // containment test. `Path::starts_with` is COMPONENT-WISE, not a
        // raw string prefix — so a sibling that shares a string prefix with
        // the base (base=`…/worktrees`, path=`…/worktrees-evil`) must be
        // rejected. A string-prefix bug here would silently let it through.
        let (tmp, base, _sub) = base_and_sub();
        // Sibling of the base whose name string-prefixes the base's name.
        let evil = tmp
            .path()
            .join(format!("{}-evil", base.file_name().unwrap().to_str().unwrap()));
        std::fs::create_dir(&evil).unwrap();
        let err = validate_remote_workdir_in(&evil, &base)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("outside the allowed worktrees base"),
            "prefix-sibling must be rejected (component-wise starts_with); got: {err}"
        );
    }

    #[test]
    fn validate_remote_workdir_in_rejects_dotdot_escape() {
        // base/../<sibling> canonicalizes to the sibling, which is outside
        // the base — confirm `..` can't escape containment.
        let (tmp, base, _sub) = base_and_sub();
        let sibling = tmp.path().join("sibling_outside");
        std::fs::create_dir(&sibling).unwrap();
        // Construct an escape path that lexically starts under the base but
        // resolves outside it: <base>/../sibling_outside.
        let escape = base.join("..").join("sibling_outside");
        let err = validate_remote_workdir_in(&escape, &base)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("outside the allowed worktrees base"),
            "expected `..`-escape rejection; got: {err}"
        );
    }

    #[test]
    fn validate_remote_workdir_in_rejects_missing_path() {
        let (_tmp, base, _sub) = base_and_sub();
        let missing = base.join("does-not-exist");
        let err = validate_remote_workdir_in(&missing, &base)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not exist") || err.contains("cannot be resolved"),
            "expected does-not-exist error; got: {err}"
        );
    }

    #[test]
    fn validate_remote_workdir_in_rejects_symlink() {
        let (_tmp, base, sub) = base_and_sub();
        // A symlink under the base pointing elsewhere — rejected by the
        // symlink guard before containment is even considered.
        let target = sub.join("real_target");
        std::fs::create_dir(&target).unwrap();
        let sym = sub.join("evil_sym");
        symlink(&target, &sym).unwrap();
        let err = validate_remote_workdir_in(&sym, &base)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("symlink") || err.contains("refusing"),
            "expected symlink rejection; got: {err}"
        );
    }

    #[test]
    fn validate_remote_workdir_in_rejects_non_directory() {
        let (_tmp, base, sub) = base_and_sub();
        let f = sub.join("not_a_dir");
        std::fs::write(&f, b"hi").unwrap();
        let err = validate_remote_workdir_in(&f, &base)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    /// The public wrapper resolves the real worktrees base from `HOME`,
    /// creates it if missing, canonicalizes it, then delegates. This is the
    /// one test that touches `HOME`, so it's `#[serial]` to avoid racing
    /// other env-mutating tests.
    #[test]
    #[serial_test::serial]
    fn validate_remote_workdir_wrapper_resolves_base_from_home() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path().to_str().unwrap());
        // Base does NOT exist yet — wrapper must create_dir_all + canonicalize
        // it (this is the regression: a non-canonical fallback base would
        // false-reject a valid workdir whose parents cross a firmlink).
        let base = worktrees_base_dir();
        let workdir = base.join("my_worktree");
        std::fs::create_dir_all(&workdir).unwrap();
        let result = validate_remote_workdir(&workdir);
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let resolved = result.expect("valid workdir under freshly-created base must pass");
        assert!(resolved.is_dir());
    }
}
