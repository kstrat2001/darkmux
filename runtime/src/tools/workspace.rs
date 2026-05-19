//! Workspace-root path enforcement.
//!
//! All Read / Write / Bash tool paths must resolve to somewhere inside
//! the dispatch's workspace root (default: `/workspace` inside the
//! container). This module is the security-critical piece — every
//! tool that touches the filesystem routes through here first.
//!
//! Three attack vectors the validator defends against:
//!
//! 1. **Absolute paths outside workspace** — `/etc/passwd`, `/home/...`,
//!    etc. Rejected by the post-canonicalization prefix check.
//! 2. **Relative paths with `..` escapes** — `../../etc/passwd` resolves
//!    out of workspace. Rejected after canonicalization.
//! 3. **Symlink escapes** — `ln -s /etc/passwd inside-workspace/leak`
//!    followed by `read inside-workspace/leak` would otherwise follow
//!    the link. `canonicalize()` resolves symlinks before the prefix
//!    check, so the target's real path is what we test.
//!
//! Note on the prefix check: `Path::starts_with` does **component-wise**
//! prefix matching, not substring matching. So `/workspace2/foo`
//! correctly fails the `starts_with("/workspace")` test — the components
//! are `[workspace2, foo]` and `[workspace]` which don't share a prefix.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Default workspace root inside the dispatch container.
pub const DEFAULT_WORKSPACE: &str = "/workspace";

/// Resolve and validate a path that the agent wants to **read**. The
/// path must already exist on disk so that `canonicalize` can resolve
/// it; the canonical resolution is then checked against the workspace
/// root.
///
/// `input` may be absolute (`/workspace/foo.txt`) or relative
/// (`foo.txt`, `subdir/bar.txt`, `subdir/../bar.txt`). Relative paths
/// resolve against `workspace_root`.
pub fn resolve_read(input: &str, workspace_root: &Path) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let initial = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        workspace_root.join(input)
    };

    let canonical = initial
        .canonicalize()
        .with_context(|| format!("resolving path: {input}"))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {workspace_root:?}"))?;

    if !canonical.starts_with(&canonical_root) {
        return Err(anyhow!(
            "path escapes workspace: {input} → {canonical:?}"
        ));
    }

    Ok(canonical)
}

/// Resolve and validate a path that the agent wants to **write**. The
/// file itself may not exist yet, so we canonicalize the **parent** dir
/// (which must exist) and require that the parent is inside the
/// workspace root. The returned path is `canonical_parent / filename`.
///
/// Note: this rejects writes where the parent dir doesn't exist. Agents
/// that need to create deep paths should call Bash with `mkdir -p`
/// first, or future iterations can add an explicit auto-mkdir mode.
pub fn resolve_write(input: &str, workspace_root: &Path) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let initial = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        workspace_root.join(input)
    };

    let filename = initial
        .file_name()
        .ok_or_else(|| anyhow!("path has no filename component: {input}"))?
        .to_owned();

    let parent = initial
        .parent()
        .ok_or_else(|| anyhow!("path has no parent component: {input}"))?;

    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("resolving parent of {input}: {parent:?}"))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {workspace_root:?}"))?;

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(anyhow!(
            "write parent escapes workspace: {input} → {canonical_parent:?}"
        ));
    }

    Ok(canonical_parent.join(filename))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Create a fresh tempdir to serve as the workspace root for one
    /// test. Returns the dir; drop = cleanup.
    fn fresh_workspace() -> tempdir::TempDir {
        tempdir::TempDir::new("darkmux-runtime-workspace-test")
            .expect("create tempdir")
    }

    #[test]
    fn read_accepts_existing_file_inside_workspace() {
        let ws = fresh_workspace();
        let file = ws.path().join("hello.txt");
        fs::write(&file, b"hi").unwrap();

        let result = resolve_read("hello.txt", ws.path()).unwrap();
        assert_eq!(result, file.canonicalize().unwrap());
    }

    #[test]
    fn read_accepts_nested_path() {
        let ws = fresh_workspace();
        let sub = ws.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("nested.txt");
        fs::write(&file, b"nested").unwrap();

        let result = resolve_read("sub/nested.txt", ws.path()).unwrap();
        assert_eq!(result, file.canonicalize().unwrap());
    }

    #[test]
    fn read_rejects_dotdot_escape() {
        let ws = fresh_workspace();
        // Need a file at /tmp/foo to test against
        let outside = ws.path().parent().unwrap().join("escape-target.txt");
        let _ = fs::write(&outside, b"shouldnt see this");

        // Relative escape via ..
        let escape_attempt = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = resolve_read(&escape_attempt, ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn read_rejects_absolute_path_outside_workspace() {
        let ws = fresh_workspace();
        // /etc/hosts is present on macOS, Linux, and Alpine — more
        // reliable cross-platform than /etc/hostname (which is
        // missing on stock macOS).
        let err = resolve_read("/etc/hosts", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn read_rejects_symlink_target_outside_workspace() {
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("link-target.txt");
        fs::write(&outside, b"secret").unwrap();
        let link = ws.path().join("leak");
        symlink(&outside, &link).unwrap();

        let err = resolve_read("leak", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn read_rejects_empty_path() {
        let ws = fresh_workspace();
        let err = resolve_read("", ws.path()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn read_rejects_nonexistent_file() {
        let ws = fresh_workspace();
        let err = resolve_read("nope.txt", ws.path()).unwrap_err();
        // canonicalize() returns IO error for nonexistent paths;
        // we wrap it with context so the message includes "resolving path"
        assert!(err.to_string().contains("resolving path"));
    }

    #[test]
    fn read_rejects_workspace2_lookalike_prefix() {
        // Build a workspace dir, and a sibling whose name STARTS with
        // the workspace dir's name as a substring. Path::starts_with
        // does component-wise matching, so the lookalike should fail
        // even though string-prefix would pass.
        let tmp = tempdir::TempDir::new("darkmux-lookalike").unwrap();
        let workspace = tmp.path().join("workspace");
        let workspace2 = tmp.path().join("workspace2");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&workspace2).unwrap();
        let target = workspace2.join("foo.txt");
        fs::write(&target, b"x").unwrap();

        let absolute = target.to_string_lossy().into_owned();
        let err = resolve_read(&absolute, &workspace).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_accepts_new_file_inside_workspace() {
        let ws = fresh_workspace();
        let result = resolve_write("new-file.txt", ws.path()).unwrap();
        // file doesn't exist yet but parent is workspace
        assert!(result.starts_with(ws.path().canonicalize().unwrap()));
        assert_eq!(result.file_name().unwrap(), "new-file.txt");
    }

    #[test]
    fn write_rejects_dotdot_escape() {
        let ws = fresh_workspace();
        let err = resolve_write("../escape.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_rejects_absolute_outside() {
        let ws = fresh_workspace();
        let err = resolve_write("/tmp/escape.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_rejects_path_through_symlink_escape() {
        // Create a symlink IN the workspace pointing to a directory outside
        // the workspace, then try to write through it.
        let ws = fresh_workspace();
        let outside_dir = ws.path().parent().unwrap().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();
        let leak = ws.path().join("leak");
        symlink(&outside_dir, &leak).unwrap();

        let err = resolve_write("leak/sneak.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_dir(&outside_dir);
    }

    #[test]
    fn write_rejects_when_parent_dir_doesnt_exist() {
        let ws = fresh_workspace();
        let err = resolve_write("does-not-exist/file.txt", ws.path()).unwrap_err();
        assert!(err.to_string().contains("resolving parent"));
    }

    #[test]
    fn write_accepts_nested_path_when_parent_exists() {
        let ws = fresh_workspace();
        fs::create_dir(ws.path().join("subdir")).unwrap();
        let result = resolve_write("subdir/file.txt", ws.path()).unwrap();
        assert!(result.starts_with(ws.path().canonicalize().unwrap()));
        assert_eq!(result.file_name().unwrap(), "file.txt");
    }
}
