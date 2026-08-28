//! Source resolution (#1959 packet 1) — turns each corpus manifest source
//! into a read-only worktree pinned at a resolved sha.
//!
//! Mechanics: a bare mirror per source at `<root>/mirror/<id>.git` (cloned
//! once, `git fetch --prune`d on later resolves when `fetch` is requested),
//! then a DETACHED worktree at `<root>/tree/<id>` checked out at the ref's
//! resolved sha and made read-only. Shells out to `git` via
//! `std::process::Command`, mirroring the existing pattern in
//! `src/coder_phase.rs` (`git worktree add`/`remove`) rather than adding a
//! git-wrapper crate. Never touches a source's ORIGINAL clone beyond the
//! read-only `git clone --bare` / `git fetch` a mirror needs.

use crate::crawl::manifest::{CorpusManifest, SourceSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSource {
    pub id: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub tree: PathBuf,
}

/// Resolve every source in the manifest into a `ResolvedSource`. When
/// `fetch` is true, an already-mirrored source is `git fetch --prune`d
/// before the worktree checkout so a later resolve can advance past a new
/// commit; `--no-fetch` (the CLI flag) passes `false` to work fully
/// offline against whatever the mirror already has.
pub fn resolve(manifest: &CorpusManifest, fetch: bool) -> Result<Vec<ResolvedSource>> {
    let root = manifest.resolved_root();
    let mirror_root = root.join("mirror");
    let tree_root = root.join("tree");
    fs::create_dir_all(&mirror_root)
        .with_context(|| format!("creating mirror dir {}", mirror_root.display()))?;
    fs::create_dir_all(&tree_root)
        .with_context(|| format!("creating tree dir {}", tree_root.display()))?;

    manifest
        .sources
        .iter()
        .map(|s| resolve_one(s, &mirror_root, &tree_root, fetch))
        .collect()
}

fn resolve_one(
    source: &SourceSpec,
    mirror_root: &Path,
    tree_root: &Path,
    fetch: bool,
) -> Result<ResolvedSource> {
    let origin = source
        .origin()
        .ok_or_else(|| anyhow::anyhow!("source '{}' names neither `git` nor `path`", source.id))?;
    let mirror_path = mirror_root.join(format!("{}.git", source.id));

    if !mirror_path.exists() {
        run_git(
            None,
            &["clone", "--bare", origin, &mirror_path.to_string_lossy()],
            &format!("cloning source '{}' ({origin})", source.id),
        )?;
        // `git clone --bare` does NOT configure `remote.origin.fetch` the
        // way a normal clone does — a later `git fetch --prune origin`
        // would update FETCH_HEAD only and never advance the mirror's own
        // `refs/heads/*`, so a second `resolve()` could never see a new
        // commit. Configure the refspec explicitly, once, right after the
        // initial clone.
        run_git(
            Some(&mirror_path),
            &["config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*"],
            &format!("configuring fetch refspec for source '{}'", source.id),
        )?;
    } else if fetch {
        run_git(
            Some(&mirror_path),
            &["fetch", "--prune", "origin"],
            &format!("fetching source '{}'", source.id),
        )?;
    }

    let git_ref = source.resolved_ref();
    let rev_out = Command::new("git")
        .current_dir(&mirror_path)
        .args(["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")])
        .output()
        .with_context(|| format!("running git rev-parse for source '{}'", source.id))?;
    if !rev_out.status.success() {
        bail!(
            "ref '{git_ref}' not found for source '{}' (git rev-parse failed): {}",
            source.id,
            stderr_of(&rev_out)
        );
    }
    let sha = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();

    let tree_path = tree_root.join(&source.id);
    if tree_path.exists() {
        // The prior checkout may have been made read-only — restore write
        // access before `git worktree remove`/`remove_dir_all` need it.
        make_tree_writable(&tree_path)?;
        // Best-effort: unregister with the mirror first so a stale
        // registration doesn't fight the next `worktree add`. `--force`
        // below is the real safety net if this fails.
        let _ = Command::new("git")
            .current_dir(&mirror_path)
            .args(["worktree", "remove", "--force", &tree_path.to_string_lossy()])
            .output();
        if tree_path.exists() {
            fs::remove_dir_all(&tree_path)
                .with_context(|| format!("removing stale worktree {}", tree_path.display()))?;
        }
    }

    run_git(
        Some(&mirror_path),
        &[
            "worktree",
            "add",
            "--detach",
            "--force",
            &tree_path.to_string_lossy(),
            &sha,
        ],
        &format!("checking out source '{}' at {sha}", source.id),
    )?;

    make_tree_read_only(&tree_path)?;

    Ok(ResolvedSource {
        id: source.id.clone(),
        sha,
        git_ref: git_ref.to_string(),
        tree: tree_path,
    })
}

fn run_git(cwd: Option<&Path>, args: &[&str], context: &str) -> Result<Output> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .args(args)
        .output()
        .with_context(|| format!("running `git {}` ({context})", args.join(" ")))?;
    if !out.status.success() {
        bail!("{context} failed: {}", stderr_of(&out));
    }
    Ok(out)
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

/// Recursively list every regular file (not directory, not symlink) under
/// `dir`, skipping `.git` (the worktree's own metadata pointer/dir).
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_files(&path, out)?;
        } else if ft.is_file() {
            out.push(path);
        }
        // Symlinks are left untouched — chmod on a symlink follows the
        // target on most platforms, which isn't a file inside this tree.
    }
    Ok(())
}

/// `chmod -R a-w` on every FILE under `tree` — directories keep their
/// permissions (and stay traversable) so the read-only guarantee is
/// "the model can't edit what it reads" without breaking directory listing.
/// POSIX-only (mirrors the existing POSIX-only carve-out for
/// `DARKMUX_AUDIT_DIR` — Windows is unsupported for darkmux generally); a
/// no-op elsewhere.
#[cfg(unix)]
fn make_tree_read_only(tree: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut files = Vec::new();
    walk_files(tree, &mut files)?;
    for f in files {
        let meta = fs::metadata(&f)?;
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() & !0o222);
        fs::set_permissions(&f, perm)
            .with_context(|| format!("setting {} read-only", f.display()))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn make_tree_read_only(_tree: &Path) -> Result<()> {
    Ok(())
}

/// Undo `make_tree_read_only` before deleting/re-checking-out a stale tree.
#[cfg(unix)]
fn make_tree_writable(tree: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut files = Vec::new();
    walk_files(tree, &mut files)?;
    for f in files {
        let meta = fs::metadata(&f)?;
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() | 0o200);
        fs::set_permissions(&f, perm)
            .with_context(|| format!("restoring write access to {}", f.display()))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn make_tree_writable(_tree: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::manifest::CorpusManifest;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Build a throwaway git repo with two commits, returning (tempdir, dir
    /// path). Commit 1 has `a.txt`; commit 2 adds `b.txt`.
    fn init_source_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "first"]);
        dir
    }

    fn manifest_for(name: &str, root: &Path, source_path: &Path, git_ref: &str) -> CorpusManifest {
        let json = serde_json::json!({
            "name": name,
            "root": root.to_string_lossy(),
            "sources": [
                {"id": "app", "path": source_path.to_string_lossy(), "ref": git_ref}
            ]
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn resolve_checks_out_at_correct_sha_read_only_and_advances_on_new_commit() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t1", workdir.path(), source.path(), "main");

        let resolved = resolve(&manifest, true).unwrap();
        assert_eq!(resolved.len(), 1);
        let r0 = &resolved[0];

        // sha matches `git rev-parse` of the source at the time of resolve.
        let expect_sha = Command::new("git")
            .current_dir(source.path())
            .args(["rev-parse", "main"])
            .output()
            .unwrap();
        let expect_sha = String::from_utf8_lossy(&expect_sha.stdout).trim().to_string();
        assert_eq!(r0.sha, expect_sha);

        // Tree contains the checked-out file.
        let a_path = r0.tree.join("a.txt");
        assert!(a_path.exists(), "{:?}", r0.tree);

        // Files are not writable.
        let mode = fs::metadata(&a_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "file should have no write bits: {mode:o}");

        // A new commit in the source advances the sha on the next resolve.
        fs::write(source.path().join("b.txt"), "world\n").unwrap();
        Command::new("git")
            .current_dir(source.path())
            .args(["add", "b.txt"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(source.path())
            .args(["commit", "-q", "-m", "second"])
            .output()
            .unwrap();

        let resolved2 = resolve(&manifest, true).unwrap();
        let r1 = &resolved2[0];
        assert_ne!(r1.sha, r0.sha);
        assert!(r1.tree.join("b.txt").exists());
    }

    #[test]
    fn resolve_no_fetch_does_not_advance_past_a_new_commit() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t2", workdir.path(), source.path(), "main");

        let first = resolve(&manifest, true).unwrap();
        let first_sha = first[0].sha.clone();

        fs::write(source.path().join("c.txt"), "new\n").unwrap();
        Command::new("git")
            .current_dir(source.path())
            .args(["add", "c.txt"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(source.path())
            .args(["commit", "-q", "-m", "third"])
            .output()
            .unwrap();

        let second = resolve(&manifest, false).unwrap();
        assert_eq!(second[0].sha, first_sha, "no-fetch must not pick up the new commit");
    }

    #[test]
    fn resolve_unknown_ref_fails_loudly_naming_the_ref() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t3", workdir.path(), source.path(), "does-not-exist");

        let err = resolve(&manifest, true).unwrap_err();
        assert!(err.to_string().contains("does-not-exist"), "{err}");
    }
}
