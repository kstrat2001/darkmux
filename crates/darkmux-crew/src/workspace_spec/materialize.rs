//! Workspace materialization (#1959) — turns each workspace spec source
//! into a worktree pinned at a resolved sha, then walks it into a
//! filtered, sorted relative-path file list per source.
//!
//! Git mechanics: a bare mirror per source at `<root>/mirror/<id>.git`
//! (cloned once, `git fetch --prune`d on later resolves when `fetch` is
//! requested), then a DETACHED worktree at `<root>/tree/<id>` checked out
//! at the ref's resolved sha, optionally made read-only. Moved UNCHANGED
//! from `darkmux_lab::crawl::sources` (#1959 packet 1's `sources::resolve`)
//! — same containment guards, same shell-out-to-`git` approach, same
//! every test. The one behavior change: `read_only` is now a caller
//! option (`MaterializeOptions.read_only`) rather than unconditional —
//! `sources::resolve` always locked the tree; a generic mission input
//! shouldn't assume every consumer wants that.
//!
//! **The mirror is darkmux-owned cache state, and it is checked before it
//! is trusted (#2399).** Existence used to be the only test: `if
//! !mirror_path.exists() { clone } else { fetch }`. On 2026-09-05 a live
//! bake-off found a NON-bare repo sitting at a mirror path (HEAD on
//! `main`, most likely an external `git` run inside the cache), and every
//! `plan.sites` step died on `fatal: refusing to fetch into branch
//! 'refs/heads/main' checked out at ...`. So a mirror that already exists
//! is now verified — `git rev-parse --is-bare-repository` must say `true`
//! and `remote.origin.url` must be the spec's own origin — and a mirror
//! that fails either check is MOVED ASIDE to
//! `<mirror>.corrupt-<unix-ts>` (moved, never deleted: it is evidence),
//! announced on stderr in one loud line naming the path and the defect,
//! and re-cloned from the origin the spec names. darkmux never fetches
//! into a repository that failed the check.
//!
//! **One workspace materializes at a time (#2399).** Since #2397 the
//! NoModel track runs its `plan.sites` steps 8-wide, so N `materialize`
//! calls now hit one workspace's mirror and tree at once; before, they
//! were only accidentally serialized. Every call takes an advisory
//! `flock(2)` (`LOCK_EX`) on `<root>/.materialize.lock` — created if
//! absent, never read or written, held for the whole call: the mirror
//! clone/fetch, the worktree checkout, AND the tree walk that follows
//! (walking a tree another holder is about to tear down and re-add is the
//! same race one step later). `flock` is per open-file-description, so
//! threads in one process serialize against each other exactly as
//! separate processes do. Non-Unix targets get a no-op lock and are
//! documented as unsupported for concurrent materialization — the same
//! POSIX-only posture the audit flow sink takes.
//!
//! Work another holder already finished is not redone: the mirror
//! existence + health checks all run AFTER the lock is acquired, and a
//! `fetch` whose generation counter advanced while this call sat blocked
//! on the lock is skipped (see [`fetch_is_redundant`]). The skip
//! deliberately keys on that counter rather than on "the resolved sha
//! already matches the tree" — without a fetch the mirror resolves the
//! ref to the OLD sha by definition, so sha-equality would leave a mirror
//! permanently stale and break the documented "a second materialize
//! advances onto a new commit" contract.

use super::{SourceSpec, WorkspaceSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};

/// The advisory lock file that serializes materialization of ONE
/// workspace root (#2399). Created on demand, never read or written —
/// only `flock`ed.
const LOCK_FILE_NAME: &str = ".materialize.lock";

#[derive(Debug, Clone, Copy)]
pub struct MaterializeOptions {
    /// `git fetch --prune`/clone over the network when true; when false,
    /// work fully offline against whatever a mirror already has (a
    /// `path`-origin source's first clone is local-filesystem-only either
    /// way — see `resolve_one`'s doc).
    pub fetch: bool,
    /// `chmod -R a-w` the checked-out tree (files, then every directory
    /// including the tree's own top level) when true.
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedSource {
    pub id: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub tree: PathBuf,
}

/// A file `walk_and_filter` found but didn't include — a symlink (never
/// followed, never counted) or a path that failed the spec's
/// `include`/`exclude`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub source_id: String,
    pub relative_path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Materialized {
    /// (#1959) The spec's own `effective_name()`, carried here so a
    /// consumer (the crawl planner) has everything it needs from ONE
    /// `Materialized` value without also holding a `&WorkspaceSpec`.
    pub name: String,
    pub root: PathBuf,
    pub sources: Vec<MaterializedSource>,
    /// (#1959) The spec's own `edges`, carried verbatim — same reasoning
    /// as `name`.
    pub edges: Vec<super::EdgeSpec>,
    /// Per source id: the relative paths that pass the spec's
    /// `include`/`exclude`, sorted.
    pub files: std::collections::BTreeMap<String, Vec<String>>,
    pub skipped: Vec<SkippedFile>,
    /// Per source: files the spec\'s include/exclude left out — out of scope,
    /// counted, never listed (#1959).
    pub out_of_scope: std::collections::BTreeMap<String, usize>,
}

/// Resolve every source in the spec, then walk + filter each tree.
pub fn materialize(spec: &WorkspaceSpec, opts: MaterializeOptions) -> Result<Materialized> {
    let root = spec.resolved_root();
    let mirror_root = root.join("mirror");
    let tree_root = root.join("tree");
    fs::create_dir_all(&mirror_root)
        .with_context(|| format!("creating mirror dir {}", mirror_root.display()))?;
    fs::create_dir_all(&tree_root)
        .with_context(|| format!("creating tree dir {}", tree_root.display()))?;

    // (#2399) Read each source's fetch generation BEFORE blocking on the
    // lock. If it has advanced by the time we hold the lock, a peer
    // completed a clone/fetch for that exact (mirror, ref) while we were
    // queued behind it and ours would be redundant. Keys are built off the
    // CANONICAL mirror root so they match the ones `resolve_one` computes
    // from its `contained_child` path.
    let canon_mirror_root = mirror_root.canonicalize().unwrap_or_else(|_| mirror_root.clone());
    let gens_before: Vec<u64> = spec
        .sources
        .iter()
        .map(|s| {
            fetch_generation(&fetch_key(
                &canon_mirror_root.join(format!("{}.git", s.id)),
                s.resolved_ref(),
            ))
        })
        .collect();

    // Held for the whole call — clone/fetch, checkout, AND the walk below.
    let _lock = WorkspaceLock::acquire(&root)?;

    let sources: Vec<MaterializedSource> = spec
        .sources
        .iter()
        .zip(gens_before)
        .map(|(s, gen_before)| resolve_one(s, &mirror_root, &tree_root, opts, gen_before))
        .collect::<Result<_>>()?;

    let include = spec.effective_include();
    let exclude = spec.effective_exclude();
    let mut files = std::collections::BTreeMap::new();
    let mut skipped = Vec::new();
    let mut out_of_scope = std::collections::BTreeMap::new();
    for s in &sources {
        let (kept, mut skip, oos) = walk_and_filter(&s.tree, &include, &exclude, &s.id)?;
        files.insert(s.id.clone(), kept);
        skipped.append(&mut skip);
        out_of_scope.insert(s.id.clone(), oos);
    }

    Ok(Materialized {
        name: spec.effective_name().to_string(),
        root,
        sources,
        edges: spec.edges.clone(),
        files,
        skipped,
        out_of_scope,
    })
}

fn resolve_one(
    source: &SourceSpec,
    mirror_root: &Path,
    tree_root: &Path,
    opts: MaterializeOptions,
    // This call's fetch generation for this source, read BEFORE it blocked
    // on the workspace lock (#2399).
    gen_before: u64,
) -> Result<MaterializedSource> {
    let fetch = opts.fetch;
    let origin = source
        .origin()
        .ok_or_else(|| anyhow::anyhow!("source '{}' names neither `git` nor `path`", source.id))?;
    // Compute both paths through the containment guard ONCE, up front, so
    // neither the clone nor the worktree checkout below can run against an
    // escaping path in the first place (#1959 second-round finding,
    // carried over from `crawl::sources`).
    let mirror_path = contained_child(mirror_root, &format!("{}.git", source.id))
        .with_context(|| format!("resolving mirror path for source '{}'", source.id))?;
    let tree_path = contained_child(tree_root, &source.id)
        .with_context(|| format!("resolving tree path for source '{}'", source.id))?;

    // (#2399) The mirror is darkmux-owned cache state, so an existing one
    // is VERIFIED before it is trusted — bare, and pointing at the origin
    // this spec names. A mirror that fails either check is quarantined and
    // re-cloned; darkmux never fetches into a repository that failed.
    let mut healed_defect: Option<String> = None;
    if mirror_path.exists() {
        if let Some(defect) = mirror_defect(&mirror_path, origin) {
            quarantine_mirror(&mirror_path, &source.id, &defect)?;
            healed_defect = Some(defect);
        }
    }

    let generation_key = fetch_key(&mirror_path, source.resolved_ref());

    if !mirror_path.exists() {
        // --no-fetch promises "fully offline against whatever's already
        // mirrored". A `git`-origin mirror that doesn't exist yet can only
        // be populated over the network. A `path`-origin clone is
        // local-filesystem-only regardless of `fetch` and is allowed
        // through unconditionally.
        if !fetch && source.git.is_some() {
            match &healed_defect {
                Some(defect) => bail!(
                    "mirror for source '{}' was quarantined ({defect}) and re-cloning it needs the network; run without --no-fetch once",
                    source.id
                ),
                None => bail!(
                    "mirror for source '{}' does not exist; run without --no-fetch once",
                    source.id
                ),
            }
        }
        run_git(
            None,
            &["clone", "--bare", "--no-hardlinks", "--", origin, &mirror_path.to_string_lossy()],
            &format!("cloning source '{}' ({origin})", source.id),
        )?;
        // `git clone --bare` does NOT configure `remote.origin.fetch` the
        // way a normal clone does — configure the refspec explicitly, once,
        // right after the initial clone, so a later `git fetch --prune
        // origin` actually advances the mirror's own `refs/heads/*`.
        run_git(
            Some(&mirror_path),
            &["config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*"],
            &format!("configuring fetch refspec for source '{}'", source.id),
        )?;
        // A second, `--add`ed refspec for tags — `+` force-update, same as
        // the branches refspec, so a moved tag advances too (git's default
        // auto-follow-tags won't move an EXISTING local tag ref that has
        // diverged).
        run_git(
            Some(&mirror_path),
            &["config", "--add", "remote.origin.fetch", "+refs/tags/*:refs/tags/*"],
            &format!("configuring tag fetch refspec for source '{}'", source.id),
        )?;
        bump_fetch_generation(&generation_key);
    } else if fetch && !fetch_is_redundant(gen_before, fetch_generation(&generation_key)) {
        run_git(
            Some(&mirror_path),
            &["fetch", "--prune", "--prune-tags", "origin"],
            &format!("fetching source '{}'", source.id),
        )?;
        bump_fetch_generation(&generation_key);
    }

    let git_ref = source.resolved_ref();
    let rev_out = Command::new("git")
        .current_dir(&mirror_path)
        // `--end-of-options` is rev-parse's "stop parsing options" marker
        // that still treats what follows as a revision — plain `--` before
        // a rev-parse positional means "everything after is a path" and
        // breaks every valid ref.
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
        // Belt-and-suspenders backstop right before the destructive ops
        // that follow, independent of `WorkspaceSpec::validate`'s id-shape
        // check and `contained_child` above.
        assert_direct_child(tree_root, &tree_path, &format!("tree path for source '{}'", source.id))?;
        assert_direct_child(mirror_root, &mirror_path, &format!("mirror path for source '{}'", source.id))?;
        // The prior checkout may have been made read-only — restore write
        // access before `git worktree remove`/`remove_dir_all` need it.
        make_tree_writable(&tree_path)?;
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

    if opts.read_only {
        make_tree_read_only(&tree_path)?;
    }

    Ok(MaterializedSource { id: source.id.clone(), sha, git_ref: git_ref.to_string(), tree: tree_path })
}

/// The advisory per-workspace lock (#2399): an exclusive `flock(2)` on
/// `<root>/.materialize.lock`, released when the guard drops (including
/// on an early `?` return, and on process exit when the fd is closed).
///
/// `flock` locks the open file DESCRIPTION, not the process, so two
/// threads in one process that each open the file conflict with each
/// other exactly as two processes do — which is the case #2397's 8-wide
/// NoModel track actually produces.
#[cfg(unix)]
struct WorkspaceLock {
    /// Held only for its lifetime: dropping it closes the fd, which is
    /// what releases the lock. Read by `Drop` below.
    file: fs::File,
}

#[cfg(unix)]
impl WorkspaceLock {
    fn acquire(root: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let path = root.join(LOCK_FILE_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening workspace lock {}", path.display()))?;
        loop {
            // SAFETY: `file` owns the fd for the whole call.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            // A signal can interrupt a blocking flock; that is not a
            // failure to lock, it is a reason to ask again.
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow::Error::new(err)
                .context(format!("locking workspace lock {}", path.display())));
        }
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Closing the fd would release the lock anyway; unlocking first is
        // explicit about the intent. Nothing useful to do with a failure
        // here — the close that follows releases it regardless.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Non-Unix targets get no lock: `flock(2)` is POSIX, and darkmux is
/// POSIX-only elsewhere too (the audit flow sink, `bounded_command`'s
/// process groups). Concurrent materialization of ONE workspace is
/// therefore unsupported off Unix; a single materialize is unaffected.
#[cfg(not(unix))]
struct WorkspaceLock;

#[cfg(not(unix))]
impl WorkspaceLock {
    fn acquire(_root: &Path) -> Result<Self> {
        Ok(Self)
    }
}

/// What a mirror self-check found wrong, phrased for the operator — or
/// `None` when the mirror is a bare clone of the origin this spec names
/// (#2399).
fn mirror_defect(mirror_path: &Path, origin: &str) -> Option<String> {
    match Command::new("git")
        .current_dir(mirror_path)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if answer != "true" {
                return Some(format!(
                    "it is not a bare repository (git rev-parse --is-bare-repository said '{answer}')"
                ));
            }
        }
        Ok(out) => {
            return Some(format!(
                "git cannot read it as a repository ({})",
                stderr_of(&out).replace('\n', " ")
            ))
        }
        Err(e) => return Some(format!("git rev-parse could not run against it ({e})")),
    }

    let configured = match Command::new("git")
        .current_dir(mirror_path)
        .args(["config", "--get", "remote.origin.url"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    };
    if !same_origin(&configured, origin) {
        let shown = if configured.is_empty() { "<unset>" } else { configured.as_str() };
        return Some(format!(
            "its remote.origin.url is '{shown}', not this spec's origin '{origin}'"
        ));
    }
    None
}

/// `git clone` records a `path` origin verbatim, so the string compare
/// answers almost every case; the canonical compare behind it covers the
/// one that matters in practice (a symlinked temp root — `/var` vs
/// `/private/var` on macOS) without ever calling a differing REMOTE url
/// equal.
fn same_origin(configured: &str, origin: &str) -> bool {
    if configured == origin {
        return true;
    }
    if configured.is_empty() {
        return false;
    }
    match (Path::new(configured).canonicalize(), Path::new(origin).canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Move a mirror that failed [`mirror_defect`] aside to
/// `<mirror>.corrupt-<unix-ts>` and say so, loudly, in one line naming the
/// path and what was wrong (#2399). MOVED, never deleted: a corrupted
/// mirror is the evidence for whatever wrote into it.
///
/// stderr is the honest floor here — `materialize` is handed a spec and
/// options, not a flow sink, and inventing a global one to carry a warning
/// would be a bigger change than the warning is worth.
fn quarantine_mirror(mirror_path: &Path, source_id: &str, defect: &str) -> Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut aside = PathBuf::from({
        let mut s = mirror_path.as_os_str().to_os_string();
        s.push(format!(".corrupt-{ts}"));
        s
    });
    // Two quarantines within one second must not collide.
    let mut n = 1u32;
    while aside.exists() {
        aside = PathBuf::from({
            let mut s = mirror_path.as_os_str().to_os_string();
            s.push(format!(".corrupt-{ts}-{n}"));
            s
        });
        n += 1;
    }
    fs::rename(mirror_path, &aside).with_context(|| {
        format!("moving corrupt mirror {} aside to {}", mirror_path.display(), aside.display())
    })?;
    eprintln!(
        "[darkmux] WARNING: the mirror for source '{source_id}' at {} failed its self-check — {defect}. Moved aside to {} and re-cloning from the spec's origin. (#2399)",
        mirror_path.display(),
        aside.display()
    );
    Ok(())
}

/// Per-(mirror, ref) counter of completed clones/fetches, used only to
/// recognize work a peer finished while this call was blocked on the
/// workspace lock. In-process by design: #2397's concurrency is threads in
/// one process, and a cross-process peer costs at worst one redundant
/// fetch, never a wrong answer.
fn fetch_generations() -> &'static Mutex<HashMap<String, u64>> {
    static GENERATIONS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fetch_key(mirror_path: &Path, git_ref: &str) -> String {
    format!("{}\u{0}{git_ref}", mirror_path.display())
}

fn fetch_generation(key: &str) -> u64 {
    fetch_generations().lock().map(|m| m.get(key).copied().unwrap_or(0)).unwrap_or(0)
}

fn bump_fetch_generation(key: &str) {
    if let Ok(mut m) = fetch_generations().lock() {
        *m.entry(key.to_string()).or_insert(0) += 1;
    }
}

/// Skip this call's fetch iff the generation advanced BETWEEN the read
/// taken before we queued on the lock and the read taken while holding it
/// — i.e. a peer clone/fetch for this exact (mirror, ref) landed while we
/// waited. A sequential second call never sees this (nothing runs between
/// its two reads), so "a second materialize advances onto a new commit"
/// still holds.
fn fetch_is_redundant(gen_before: u64, gen_now: u64) -> bool {
    gen_now > gen_before
}

/// Compute `root/name`, refusing unless `name` is a single, non-escaping
/// path component — moved unchanged from `crawl::sources::contained_child`.
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
/// canonical form — moved unchanged from `crawl::sources::assert_direct_child`.
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
/// `dir`, skipping `.git`. Moved unchanged from `crawl::sources::walk_files`.
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
        // Symlinks are left untouched here — `walk_and_filter` below
        // records them in `skipped` instead of following them.
    }
    Ok(())
}

/// Recursively list every regular file AND every symlink under `dir`
/// (skipping `.git`), tagging which is which — the walker
/// `walk_and_filter` uses so a symlink can be recorded in `skipped` with a
/// reason rather than silently vanishing from both `files` and `skipped`.
fn walk_relative(dir: &Path, out: &mut Vec<(PathBuf, bool)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            out.push((path, true));
        } else if ft.is_dir() {
            walk_relative(&path, out)?;
        } else if ft.is_file() {
            out.push((path, false));
        }
    }
    Ok(())
}

/// Walk `tree`, filter each relative path against `include`/`exclude`
/// (`super::glob::applies` — the one filter language), and split the
/// result into `(kept, skipped)`. Symlinks are NEVER followed and are
/// always recorded in `skipped` with a reason, regardless of whether they
/// would have matched `include` — a symlink can point outside the tree
/// (see `crawl::sources::make_tree_read_only`'s doc on the same
/// never-follow contract for the chmod walker).
fn walk_and_filter(
    tree: &Path,
    include: &[String],
    exclude: &[String],
    source_id: &str,
) -> Result<(Vec<String>, Vec<SkippedFile>, usize)> {
    let mut entries = Vec::new();
    walk_relative(tree, &mut entries)?;

    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    // A file the spec's include/exclude leaves out is OUT OF SCOPE, not
    // skipped: counted so the plan can say how much of the tree it covers,
    // never listed (a dry run on a real tree printed 541 such lines, #1959).
    let mut out_of_scope = 0usize;
    for (path, is_symlink) in entries {
        let rel = path
            .strip_prefix(tree)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if is_symlink {
            skipped.push(SkippedFile {
                source_id: source_id.to_string(),
                relative_path: rel,
                reason: "symlink — never followed".to_string(),
            });
            continue;
        }
        if super::glob::applies(include, exclude, &rel) {
            kept.push(rel);
        } else {
            out_of_scope += 1;
        }
    }
    kept.sort();
    Ok((kept, skipped, out_of_scope))
}

/// `chmod -R a-w` on every FILE, then every DIRECTORY (bottom-up), under
/// `tree`. POSIX-only; a no-op elsewhere. Moved unchanged from
/// `crawl::sources::make_tree_read_only`.
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

/// Recursively list every directory AT OR UNDER `dir` — moved unchanged
/// from `crawl::sources::walk_dirs`.
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

/// Undo `make_tree_read_only` — moved unchanged from
/// `crawl::sources::make_tree_writable`.
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
    use crate::workspace_spec::WorkspaceSpec;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Build a throwaway git repo with two commits, returning the tempdir.
    /// Commit 1 has `a.txt`; commit 2 adds `b.txt`.
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

    fn spec_for(name: &str, root: &Path, source_path: &Path, git_ref: &str) -> WorkspaceSpec {
        let json = serde_json::json!({
            "name": name,
            "root": root.to_string_lossy(),
            "sources": [
                {"id": "app", "path": source_path.to_string_lossy(), "ref": git_ref}
            ]
        });
        serde_json::from_value(json).unwrap()
    }

    const RW: MaterializeOptions = MaterializeOptions { fetch: true, read_only: false };
    const RO: MaterializeOptions = MaterializeOptions { fetch: true, read_only: true };

    /// (#1959) `Materialized.name`/`.edges` carry the spec's own
    /// `effective_name()`/`edges` verbatim — so a consumer (the crawl
    /// planner) has everything it needs from ONE `Materialized` value
    /// without also holding a reference to the original `WorkspaceSpec`.
    #[test]
    fn materialized_carries_the_spec_name_and_edges() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let mut spec = spec_for("edges-test", workdir.path(), source.path(), "main");
        spec.edges = vec![crate::workspace_spec::EdgeSpec {
            consumer: "app".to_string(),
            library: "app".to_string(),
            package: "self".to_string(),
            extras: Default::default(),
        }];

        let m = materialize(&spec, RW).unwrap();
        assert_eq!(m.name, "edges-test");
        assert_eq!(m.edges.len(), 1);
        assert_eq!(m.edges[0].package, "self");
    }

    #[test]
    fn materialize_checks_out_at_correct_sha_and_advances_on_new_commit() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t1", workdir.path(), source.path(), "main");

        let m = materialize(&spec, RW).unwrap();
        assert_eq!(m.sources.len(), 1);
        let r0 = &m.sources[0];

        let expect_sha = Command::new("git")
            .current_dir(source.path())
            .args(["rev-parse", "main"])
            .output()
            .unwrap();
        let expect_sha = String::from_utf8_lossy(&expect_sha.stdout).trim().to_string();
        assert_eq!(r0.sha, expect_sha);
        assert!(r0.tree.join("a.txt").exists());
        assert_eq!(m.files["app"], vec!["a.txt".to_string()]);

        fs::write(source.path().join("b.txt"), "world\n").unwrap();
        Command::new("git").current_dir(source.path()).args(["add", "b.txt"]).output().unwrap();
        Command::new("git").current_dir(source.path()).args(["commit", "-q", "-m", "second"]).output().unwrap();

        let m2 = materialize(&spec, RW).unwrap();
        let r1 = &m2.sources[0];
        assert_ne!(r1.sha, r0.sha);
        assert!(r1.tree.join("b.txt").exists());
    }

    #[test]
    fn materialize_no_fetch_does_not_advance_past_a_new_commit() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t2", workdir.path(), source.path(), "main");

        let first = materialize(&spec, RW).unwrap();
        let first_sha = first.sources[0].sha.clone();

        fs::write(source.path().join("c.txt"), "new\n").unwrap();
        Command::new("git").current_dir(source.path()).args(["add", "c.txt"]).output().unwrap();
        Command::new("git").current_dir(source.path()).args(["commit", "-q", "-m", "third"]).output().unwrap();

        let second = materialize(&spec, MaterializeOptions { fetch: false, read_only: false }).unwrap();
        assert_eq!(second.sources[0].sha, first_sha, "no-fetch must not pick up the new commit");
    }

    #[test]
    fn materialize_unknown_ref_fails_loudly_naming_the_ref() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t3", workdir.path(), source.path(), "does-not-exist");

        let err = materialize(&spec, RW).unwrap_err();
        assert!(err.to_string().contains("does-not-exist"), "{err}");
    }

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

    #[test]
    fn create_path_containment_guard_rejects_escaping_id_before_any_clone() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let mut spec = spec_for("t6", workdir.path(), source.path(), "main");
        spec.sources[0].id = "../../victim3".to_string();

        let err = materialize(&spec, RW).unwrap_err();
        assert!(err.to_string().contains("victim3"), "{err}");

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
        let spec = spec_for("t7", workdir.path(), source.path(), "main");

        let m = materialize(&spec, RW).unwrap();
        assert_eq!(m.sources.len(), 1);
        assert!(m.sources[0].tree.join("a.txt").exists());
    }

    #[test]
    fn resolve_advances_a_moved_tag_on_fetch() {
        let source = init_source_repo();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["tag", "v1"]);

        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t8", workdir.path(), source.path(), "v1");

        let first = materialize(&spec, RW).unwrap();
        let first_sha = first.sources[0].sha.clone();

        fs::write(source.path().join("moved.txt"), "moved\n").unwrap();
        run(&["add", "moved.txt"]);
        run(&["commit", "-q", "-m", "advance"]);
        run(&["tag", "-f", "v1"]);

        let second = materialize(&spec, RW).unwrap();
        assert_ne!(second.sources[0].sha, first_sha, "a moved tag must advance on the next fetch");
        assert!(second.sources[0].tree.join("moved.txt").exists());
    }

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
        let spec: WorkspaceSpec = serde_json::from_value(json).unwrap();

        let err = materialize(&spec, MaterializeOptions { fetch: false, read_only: false }).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(msg.contains("--no-fetch"), "{msg}");
        assert!(msg.contains("app"), "{msg}");
    }

    #[test]
    fn no_fetch_with_absent_mirror_still_clones_a_local_path_origin() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t5", workdir.path(), source.path(), "main");

        let m = materialize(&spec, MaterializeOptions { fetch: false, read_only: false }).unwrap();
        assert_eq!(m.sources.len(), 1);
    }

    #[test]
    fn read_only_true_locks_the_top_level_directory() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t6ro", workdir.path(), source.path(), "main");

        let m = materialize(&spec, RO).unwrap();
        let tree = &m.sources[0].tree;

        let result = fs::write(tree.join("new-file.txt"), "should not be allowed");
        assert!(result.is_err(), "creating a new file inside the read-only tree must fail");
    }

    /// The generalization this packet adds over `crawl::sources::resolve`:
    /// `read_only: false` (the default a generic consumer would reach for)
    /// leaves the tree writable. `crawl::sources::resolve` had no such
    /// knob — it always locked.
    #[test]
    fn read_only_false_leaves_the_tree_writable() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t6rw", workdir.path(), source.path(), "main");

        let m = materialize(&spec, RW).unwrap();
        let tree = &m.sources[0].tree;

        let result = fs::write(tree.join("new-file.txt"), "should be allowed");
        assert!(result.is_ok(), "read_only:false must leave the tree writable: {result:?}");
    }

    #[test]
    fn make_tree_read_only_never_chmods_through_a_symlink() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("real.txt"), "in tree").unwrap();

        let outside_dir = dir.path().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("external.txt");
        fs::write(&outside_file, "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, tree.join("link-to-external.txt")).unwrap();

        make_tree_read_only(&tree).unwrap();

        let real_mode = fs::metadata(tree.join("real.txt")).unwrap().permissions().mode();
        assert_eq!(real_mode & 0o222, 0, "{real_mode:o}");

        let outside_mode = fs::metadata(&outside_file).unwrap().permissions().mode();
        assert_ne!(outside_mode & 0o222, 0, "symlink target must be untouched: {outside_mode:o}");
    }

    // ── walk_and_filter: the new capability over `crawl::sources` ──

    #[test]
    fn walk_and_filter_applies_include_exclude_and_sorts() {
        let source = init_source_repo();
        fs::create_dir_all(source.path().join("src")).unwrap();
        fs::write(source.path().join("src/z.ts"), "z").unwrap();
        fs::write(source.path().join("src/a.ts"), "a").unwrap();
        fs::write(source.path().join("README.md"), "r").unwrap();
        Command::new("git").current_dir(source.path()).args(["add", "-A"]).output().unwrap();
        Command::new("git").current_dir(source.path()).args(["commit", "-q", "-m", "more"]).output().unwrap();

        let workdir = TempDir::new().unwrap();
        let mut spec = spec_for("t9", workdir.path(), source.path(), "main");
        spec.include = Some(vec!["**/*.ts".to_string()]);

        let m = materialize(&spec, RW).unwrap();
        assert_eq!(m.files["app"], vec!["src/a.ts".to_string(), "src/z.ts".to_string()]);
        // Out-of-scope files are COUNTED, never listed as skipped (#1959: a
        // dry run on a real tree printed 541 "excluded" lines as if they were
        // failures). README.md and a.txt fall outside `**/*.ts`.
        assert!(m.out_of_scope["app"] >= 2, "{:?}", m.out_of_scope);
        assert!(
            !m.skipped.iter().any(|s| s.reason.contains("include/exclude")),
            "out-of-scope files must not appear in skipped: {:?}",
            m.skipped
        );
    }

    #[test]
    fn walk_and_filter_records_a_symlink_as_skipped_with_reason() {
        let source = init_source_repo();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), source.path().join("link.txt")).unwrap();
        Command::new("git").current_dir(source.path()).args(["add", "-A"]).output().unwrap();
        Command::new("git").current_dir(source.path()).args(["commit", "-q", "-m", "symlink"]).output().unwrap();

        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t10", workdir.path(), source.path(), "main");

        let m = materialize(&spec, RW).unwrap();
        assert!(!m.files["app"].contains(&"link.txt".to_string()));
        let skip = m.skipped.iter().find(|s| s.relative_path == "link.txt").expect("symlink recorded as skipped");
        assert!(skip.reason.contains("symlink"), "{skip:?}");
    }

    // ── #2399: the per-workspace lock + the mirror self-check ──

    /// Every `<root>/mirror/*.corrupt-*` sibling the healing path left
    /// behind — the observable proof a mirror was quarantined.
    fn corrupt_siblings(root: &Path) -> Vec<String> {
        let mirror = root.join("mirror");
        let Ok(rd) = fs::read_dir(&mirror) else { return Vec::new() };
        rd.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect()
    }

    /// (#2399) Six concurrent `materialize` calls on ONE spec — the shape
    /// `plan.sites` takes since #2397 made the NoModel track 8-wide. Red
    /// before the lock: every thread sees `!mirror_path.exists()` at the
    /// same instant and six `git clone --bare` runs collide on one
    /// destination.
    #[test]
    fn concurrent_materialize_of_one_spec_serializes_on_the_workspace_lock() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = std::sync::Arc::new(spec_for("t-race", workdir.path(), source.path(), "main"));

        let start = std::sync::Arc::new(std::sync::Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let spec = std::sync::Arc::clone(&spec);
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    materialize(&spec, RW)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut shas = std::collections::BTreeSet::new();
        for r in &results {
            let m = r.as_ref().unwrap_or_else(|e| panic!("every concurrent materialize must succeed: {e:#}"));
            shas.insert(m.sources[0].sha.clone());
        }
        assert_eq!(shas.len(), 1, "the tree must end pinned at exactly one sha: {shas:?}");

        let mirrors: Vec<_> = fs::read_dir(workdir.path().join("mirror"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(mirrors, vec!["app.git".to_string()], "exactly one mirror: {mirrors:?}");
        assert!(
            corrupt_siblings(workdir.path()).is_empty(),
            "a clean concurrent run must never quarantine a mirror: {:?}",
            corrupt_siblings(workdir.path())
        );
    }

    /// (#2399) The live failure: a NON-bare repo sitting at the mirror
    /// path. Red before the self-check — `git fetch` inside it dies with
    /// "refusing to fetch into branch 'refs/heads/main' checked out at
    /// ...", every plan step, forever.
    #[test]
    fn a_non_bare_repo_at_the_mirror_path_is_quarantined_and_recloned() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let mirror_dir = workdir.path().join("mirror");
        fs::create_dir_all(&mirror_dir).unwrap();
        let bad = mirror_dir.join("app.git");
        fs::create_dir_all(&bad).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(&bad).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        fs::write(bad.join("junk.txt"), "junk\n").unwrap();
        run(&["add", "junk.txt"]);
        run(&["commit", "-q", "-m", "not a mirror"]);
        // Wired to the real origin with the mirror's own refspec, so with
        // the bareness check mutated away this fixture reproduces the LIVE
        // 2026-09-05 failure verbatim: `fatal: refusing to fetch into
        // branch 'refs/heads/main' checked out at ...`.
        run(&["remote", "add", "origin", &source.path().to_string_lossy()]);
        run(&["config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*"]);

        let spec = spec_for("t-nonbare", workdir.path(), source.path(), "main");
        let m = materialize(&spec, RW).unwrap_or_else(|e| panic!("materialize must heal a non-bare mirror: {e:#}"));
        assert!(m.sources[0].tree.join("a.txt").exists(), "the re-cloned mirror must carry the real source");
        assert!(!m.sources[0].tree.join("junk.txt").exists(), "the quarantined repo must not be the source");

        let quarantined = corrupt_siblings(workdir.path());
        assert_eq!(quarantined.len(), 1, "the bad mirror must be moved aside: {quarantined:?}");
        assert!(quarantined[0].starts_with("app.git.corrupt-"), "{quarantined:?}");
        assert!(
            workdir.path().join("mirror").join(&quarantined[0]).join("junk.txt").exists(),
            "quarantine MOVES the directory aside — it never deletes it"
        );

        let bare = Command::new("git")
            .current_dir(workdir.path().join("mirror").join("app.git"))
            .args(["rev-parse", "--is-bare-repository"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bare.stdout).trim(), "true");
    }

    /// (#2399) A mirror whose `remote.origin.url` points somewhere else —
    /// the same quarantine-and-reclone path. Red before the self-check:
    /// `git fetch origin` reaches for the wrong (here unreachable) origin.
    #[test]
    fn a_mirror_with_a_foreign_origin_is_quarantined_and_recloned() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t-foreign", workdir.path(), source.path(), "main");

        let first = materialize(&spec, RW).unwrap();
        let sha = first.sources[0].sha.clone();

        let mirror = workdir.path().join("mirror").join("app.git");
        let out = Command::new("git")
            .current_dir(&mirror)
            .args(["config", "remote.origin.url", "https://example.invalid/not-ours.git"])
            .output()
            .unwrap();
        assert!(out.status.success());

        let m = materialize(&spec, RW).unwrap_or_else(|e| panic!("materialize must heal a foreign-origin mirror: {e:#}"));
        assert_eq!(m.sources[0].sha, sha, "the re-clone resolves the same ref to the same sha");

        let quarantined = corrupt_siblings(workdir.path());
        assert_eq!(quarantined.len(), 1, "{quarantined:?}");
        let url = Command::new("git")
            .current_dir(&mirror)
            .args(["config", "--get", "remote.origin.url"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&url.stdout).trim(),
            source.path().to_string_lossy(),
            "the healed mirror points at the spec's own origin"
        );
    }

    /// (#2399) The fetch-coalescing predicate, unit-tested directly — the
    /// concurrency test above can't pin down WHICH thread skipped, but the
    /// rule itself is a two-line decision and deserves its own red.
    #[test]
    fn fetch_is_skipped_only_when_a_peer_fetched_while_we_waited() {
        assert!(!fetch_is_redundant(0, 0), "nobody fetched while we waited — fetch");
        assert!(fetch_is_redundant(0, 1), "a peer completed a clone/fetch while we were blocked — skip");
        assert!(fetch_is_redundant(7, 9), "two peers went ahead of us — skip");
        assert!(!fetch_is_redundant(3, 3), "a sequential second call must still fetch (a new commit may exist)");
    }


}
