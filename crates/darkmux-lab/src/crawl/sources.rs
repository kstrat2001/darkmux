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
    // #1959 second-round finding: `assert_direct_child` below only ran
    // inside the `tree_path.exists()` branch, so a manifest-supplied
    // escaping id (`../../victim`) sailed straight through on a FIRST-time
    // resolve — past the bare clone AND `git worktree add` — with nothing
    // to catch it until a stale-cleanup pass happened to run later, if
    // ever. Compute both paths through the containment guard ONCE, up
    // front, so neither the clone nor the worktree checkout below can run
    // against an escaping path in the first place.
    let mirror_path = contained_child(mirror_root, &format!("{}.git", source.id))
        .with_context(|| format!("resolving mirror path for source '{}'", source.id))?;
    let tree_path = contained_child(tree_root, &source.id)
        .with_context(|| format!("resolving tree path for source '{}'", source.id))?;

    if !mirror_path.exists() {
        // #1959 finding 15: --no-fetch promises "fully offline against
        // whatever's already mirrored". A `git`-origin mirror that doesn't
        // exist yet can only be populated over the network, which breaks
        // that promise silently if we clone anyway. A `path`-origin clone
        // is local-filesystem-only regardless of `fetch` — no network
        // activity either way — so it stays consistent with the promise
        // and is allowed through unconditionally.
        if !fetch && source.git.is_some() {
            bail!(
                "mirror for source '{}' does not exist; run without --no-fetch once",
                source.id
            );
        }
        run_git(
            None,
            &["clone", "--bare", "--no-hardlinks", "--", origin, &mirror_path.to_string_lossy()],
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
        // A second, `--add`ed refspec for tags — same `+` force-update
        // prefix as the branches refspec above, and for the same reason:
        // without it, a tag that already exists locally (from an earlier
        // resolve) and gets force-moved on the remote is left stale.
        // `git`'s default auto-follow-tags behavior fetches a NEW tag
        // reachable from a fetched commit, but refuses to move an
        // EXISTING local tag ref that has diverged — it treats tags as
        // immutable by default and silently skips the update rather than
        // erroring (#1959 second-round CONSIDER 3).
        run_git(
            Some(&mirror_path),
            &["config", "--add", "remote.origin.fetch", "+refs/tags/*:refs/tags/*"],
            &format!("configuring tag fetch refspec for source '{}'", source.id),
        )?;
    } else if fetch {
        run_git(
            Some(&mirror_path),
            &["fetch", "--prune", "--prune-tags", "origin"],
            &format!("fetching source '{}'", source.id),
        )?;
    }

    let git_ref = source.resolved_ref();
    let rev_out = Command::new("git")
        .current_dir(&mirror_path)
        // `--` before a rev-parse positional means "everything after this is
        // a path", not "end of options for a revision" — it broke every
        // valid ref (`git rev-parse --verify -- main^{commit}` fails with
        // "Needed a single revision", confirmed against git 2.50.1).
        // `--end-of-options` is rev-parse's actual "stop parsing options"
        // marker that still treats what follows as a revision (#1959
        // finding 14).
        .args(["rev-parse", "--verify", "--end-of-options", &format!("{git_ref}^{{commit}}")])
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

    if tree_path.exists() {
        // #1959 finding 1: a manifest-supplied id feeds straight into
        // `tree_root.join(id)`/`mirror_root.join(id)` above.
        // `CorpusManifest::validate` rejects a shape that could escape
        // (`../../victim`) before a manifest ever reaches here, and
        // `contained_child` above is now the guard for the CREATE path —
        // this canonicalized check is the belt-and-suspenders backstop
        // right before the destructive ops that follow (`worktree remove`
        // / `remove_dir_all`), independent of either.
        assert_direct_child(tree_root, &tree_path, &format!("tree path for source '{}'", source.id))?;
        assert_direct_child(mirror_root, &mirror_path, &format!("mirror path for source '{}'", source.id))?;
        // The prior checkout may have been made read-only — restore write
        // access before `git worktree remove`/`remove_dir_all` need it.
        make_tree_writable(&tree_path)?;
        // Best-effort: unregister with the mirror first so a stale
        // registration doesn't fight the next `worktree add`. `--force`
        // below is the real safety net if this fails.
        let _ = Command::new("git")
            .current_dir(&mirror_path)
            .args(["worktree", "remove", "--force", "--", &tree_path.to_string_lossy()])
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
            "--",
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

/// Compute `root/name`, refusing unless `name` is a single, non-escaping
/// path component — the containment guard for the CREATE path (#1959
/// second-round finding: `assert_direct_child` below only fires once a
/// stale tree/mirror already exists, which skips a manifest-supplied
/// escaping id entirely on a first-time resolve). `root.canonicalize()`
/// requires `root` already exist (`resolve()` `fs::create_dir_all`s both
/// `mirror_root`/`tree_root` before any source is resolved); `name` itself
/// may not exist yet (it's what's ABOUT to be created), so it's validated
/// lexically — rejecting an absolute path, any `..`/`.` component, and any
/// path with more than one component — rather than canonicalized.
fn contained_child(root: &Path, name: &str) -> Result<PathBuf> {
    let canon_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing root {}", root.display()))?;
    let mut components = Path::new(name).components();
    let single_normal_component =
        matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none();
    if !single_normal_component {
        bail!(
            "refusing to resolve '{name}' under {} — not a single non-escaping path component (possible path traversal via source id)",
            canon_root.display()
        );
    }
    Ok(canon_root.join(name))
}

/// Bail unless `child`'s canonical form is an IMMEDIATE child of `parent`'s
/// canonical form — the containment guard that stands between a
/// manifest-supplied source id and any destructive filesystem operation
/// (`make_tree_writable`, `git worktree remove`, `fs::remove_dir_all`).
/// Independent of `CorpusManifest::validate`'s id-shape check: a caller
/// that constructs a `SourceSpec` directly (a test, a future entry point)
/// still can't smuggle `../../victim` past a delete (#1959 finding 1).
fn assert_direct_child(parent: &Path, child: &Path, what: &str) -> Result<()> {
    let canon_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalizing {what} root {}", parent.display()))?;
    let canon_child = child
        .canonicalize()
        .with_context(|| format!("canonicalizing {what} {}", child.display()))?;
    if canon_child.parent() != Some(canon_parent.as_path()) {
        bail!(
            "refusing to touch {what} {} — it resolves outside {} (possible path traversal via source id)",
            canon_child.display(),
            canon_parent.display()
        );
    }
    Ok(())
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

/// Recursively list every directory AT OR UNDER `dir` (including `dir`
/// itself), skipping `.git`. `top_down` controls whether `dir` is emitted
/// before or after its descendants — `make_tree_writable` wants top-down
/// (parent restored before its children), `make_tree_read_only` wants
/// bottom-up (a directory's own write bit is cleared only after every file
/// and subdirectory under it has already been touched, #1959 finding 12).
/// Never descends into a symlinked directory (`entry.file_type()` reports
/// a symlink's own type without following it, same as `walk_files` below)
/// — a symlink is neither read-only-locked nor writable-restored, matching
/// the "never follow, never touch the target" contract `plan.rs`'s walker
/// keeps for the same reason (#1959 finding 6).
fn walk_dirs(dir: &Path, out: &mut Vec<PathBuf>, top_down: bool) -> Result<()> {
    if top_down {
        out.push(dir.to_path_buf());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
            continue;
        }
        if entry.file_type()?.is_dir() {
            walk_dirs(&path, out, top_down)?;
        }
    }
    if !top_down {
        out.push(dir.to_path_buf());
    }
    Ok(())
}

/// `chmod -R a-w` on every FILE, then every DIRECTORY (bottom-up, including
/// `tree` itself), under `tree` — the read-only guarantee is "the model
/// can't edit what it reads AND can't create/rename/delete anything in the
/// tree either" (#1959 finding 12: locking files alone left every
/// directory's write bit — including the tree's own top level — untouched,
/// so a new file could still be created inside a "read-only" tree).
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

    let mut dirs = Vec::new();
    walk_dirs(tree, &mut dirs, false)?; // bottom-up: tree's own top level last
    for d in dirs {
        let meta = fs::metadata(&d)?;
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() & !0o222);
        fs::set_permissions(&d, perm)
            .with_context(|| format!("setting {} read-only", d.display()))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn make_tree_read_only(_tree: &Path) -> Result<()> {
    Ok(())
}

/// Undo `make_tree_read_only` before deleting/re-checking-out a stale tree
/// — directories top-down (the tree's own top level restored first) before
/// any file, mirroring `make_tree_read_only`'s reverse order.
#[cfg(unix)]
fn make_tree_writable(tree: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs = Vec::new();
    walk_dirs(tree, &mut dirs, true)?; // top-down: tree's own top level first
    for d in dirs {
        let meta = fs::metadata(&d)?;
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() | 0o200);
        fs::set_permissions(&d, perm)
            .with_context(|| format!("restoring write access to {}", d.display()))?;
    }

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

    // ── #1959 finding 1: containment guard ──

    /// Direct call to the guard (not routed through `resolve_one`): an
    /// escaping path must bail WITHOUT touching the target — proven by
    /// creating a "victim" dir with a file and confirming it's still there.
    #[test]
    fn containment_guard_rejects_escaping_path_without_touching_it() {
        let root = TempDir::new().unwrap();
        let parent = root.path().join("tree_root");
        fs::create_dir_all(&parent).unwrap();
        let victim = root.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("canary.txt"), "still here").unwrap();

        let err = assert_direct_child(&parent, &victim, "tree path for source 'x'").unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");

        // Untouched: still exists, still readable, content unchanged.
        let canary = victim.join("canary.txt");
        assert!(canary.exists());
        assert_eq!(fs::read_to_string(&canary).unwrap(), "still here");
    }

    #[test]
    fn containment_guard_accepts_a_direct_child() {
        let root = TempDir::new().unwrap();
        let parent = root.path().join("tree_root");
        let child = parent.join("app");
        fs::create_dir_all(&child).unwrap();
        assert_direct_child(&parent, &child, "tree path for source 'app'").unwrap();
    }

    // ── #1959 second-round finding: the containment guard must cover the
    // CREATE path too, not just the exists()-already branch ──

    /// Before this fix `assert_direct_child` only ran inside `if
    /// tree_path.exists()` — a FIRST-time resolve of a manifest-supplied
    /// escaping id skipped it entirely and went straight to `git clone
    /// --bare` / `git worktree add`. `resolve()` must refuse BEFORE either
    /// runs, proven here by a manifest with no prior mirror/tree state.
    #[test]
    fn create_path_containment_guard_rejects_escaping_id_before_any_clone() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let mut manifest = manifest_for("t6", workdir.path(), source.path(), "main");
        manifest.sources[0].id = "../../victim3".to_string();

        let err = resolve(&manifest, true).unwrap_err();
        assert!(err.to_string().contains("victim3"), "{err}");

        // No clone escaped: neither of the corpus root's own mirror/tree
        // dirs picked up anything at all — the guard fired before any
        // `git` process ran, not just before the escaping path
        // specifically. (A naive `mirror_root.join(id)` would have landed
        // the clone at `<system temp root>/victim3.git`, two levels above
        // `workdir` — proven by hand while red-proving this fix: before
        // the guard existed, that exact clone showed up there.)
        assert!(
            fs::read_dir(workdir.path().join("mirror")).unwrap().next().is_none(),
            "mirror dir should stay empty"
        );
        assert!(
            fs::read_dir(workdir.path().join("tree")).unwrap().next().is_none(),
            "tree dir should stay empty"
        );
    }

    #[test]
    fn create_path_containment_guard_accepts_a_normal_id() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t7", workdir.path(), source.path(), "main");

        let resolved = resolve(&manifest, true).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].tree.join("a.txt").exists());
    }

    // ── #1959 second-round CONSIDER 3: tag refs advance on fetch ──

    #[test]
    fn resolve_advances_a_moved_tag_on_fetch() {
        let source = init_source_repo();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["tag", "v1"]);

        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t8", workdir.path(), source.path(), "v1");

        let first = resolve(&manifest, true).unwrap();
        let first_sha = first[0].sha.clone();

        // Advance the source past the tag, then force-move the tag onto
        // the new commit — the mirror already has a local `v1` from the
        // first resolve, so this exercises "advance an EXISTING tag ref",
        // not just "fetch a tag for the first time".
        fs::write(source.path().join("moved.txt"), "moved\n").unwrap();
        run(&["add", "moved.txt"]);
        run(&["commit", "-q", "-m", "advance"]);
        run(&["tag", "-f", "v1"]);

        let second = resolve(&manifest, true).unwrap();
        assert_ne!(second[0].sha, first_sha, "a moved tag must advance on the next fetch");
        assert!(second[0].tree.join("moved.txt").exists());
    }

    // ── #1959 finding 15: --no-fetch with an absent mirror ──

    #[test]
    fn no_fetch_with_absent_mirror_bails_for_a_git_origin_instead_of_cloning() {
        let workdir = TempDir::new().unwrap();
        let json = serde_json::json!({
            "name": "t4",
            "root": workdir.path().to_string_lossy(),
            "sources": [
                {"id": "app", "git": "https://example.invalid/never-cloned.git", "ref": "main"}
            ]
        });
        let manifest: CorpusManifest = serde_json::from_value(json).unwrap();

        let err = resolve(&manifest, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(msg.contains("--no-fetch"), "{msg}");
        assert!(msg.contains("app"), "{msg}");
    }

    #[test]
    fn no_fetch_with_absent_mirror_still_clones_a_local_path_origin() {
        // A `path` origin's first clone is local-filesystem-only — no
        // network activity — so it stays consistent with --no-fetch's
        // "fully offline" promise and is allowed through.
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t5", workdir.path(), source.path(), "main");

        let resolved = resolve(&manifest, false).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    // ── #1959 finding 12: read-only tree locks directories too ──

    #[test]
    fn make_tree_read_only_locks_the_top_level_directory() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let manifest = manifest_for("t6", workdir.path(), source.path(), "main");

        let resolved = resolve(&manifest, true).unwrap();
        let tree = &resolved[0].tree;

        // Before this fix, only FILES lost their write bit — the tree's own
        // top-level directory kept it, so a brand new file could still be
        // created directly inside the "read-only" tree.
        let result = fs::write(tree.join("new-file.txt"), "should not be allowed");
        assert!(
            result.is_err(),
            "creating a new file inside the read-only tree must fail"
        );
    }

    // ── #1959 finding 6: the chmod walker never follows a symlink ──

    #[test]
    fn make_tree_read_only_never_chmods_through_a_symlink() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("real.txt"), "in tree").unwrap();

        // A file OUTSIDE the tree, symlinked from inside it.
        let outside_dir = dir.path().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("external.txt");
        fs::write(&outside_file, "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, tree.join("link-to-external.txt")).unwrap();

        make_tree_read_only(&tree).unwrap();

        // The tree's own file lost its write bit...
        use std::os::unix::fs::PermissionsExt;
        let real_mode = fs::metadata(tree.join("real.txt")).unwrap().permissions().mode();
        assert_eq!(real_mode & 0o222, 0, "{real_mode:o}");

        // ...but the symlink target OUTSIDE the tree was never touched.
        let outside_mode = fs::metadata(&outside_file).unwrap().permissions().mode();
        assert_ne!(outside_mode & 0o222, 0, "symlink target must be untouched: {outside_mode:o}");
    }
}
