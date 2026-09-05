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
//! absent, never read or written. `flock` is per open-file-description,
//! so threads in one process serialize against each other exactly as
//! separate processes do. Non-Unix targets get a no-op lock and are
//! documented as unsupported for concurrent materialization — the same
//! POSIX-only posture the audit flow sink takes.
//!
//! **What the lock actually covers, precisely.** Not "the call": the
//! guard is RETURNED, inside [`Materialized::lock`], and lives as long as
//! that value does. It has to. `materialize` hands back paths, and both
//! real callers (`crawl::plan_step`, `crawl::plan_sites_step` →
//! `crawl::plan`) then read every file in the trees it named. Releasing
//! at the return would leave a peer free to `remove_dir_all` those trees
//! mid-walk, and `crawl::plan` records a failed read as
//! `skipped: "stat error ..."` rather than failing — so the visible
//! symptom would be a crawl that silently covered less than it claimed.
//! The cost is stated in [`WorkspaceLock`]'s own doc: the lock is
//! exclusive, so steps sharing ONE workspace no longer overlap, and a
//! caller must drop the value rather than hold two of them.
//!
//! Work another holder already finished is not redone, in two places.
//! The mirror existence + health checks all run AFTER the lock is
//! acquired, and a `fetch` whose generation counter advanced while this
//! call sat blocked on the lock is skipped (see [`fetch_is_redundant`]).
//! The skip deliberately keys on that counter rather than on "the
//! resolved sha already matches the tree" — without a fetch the mirror
//! resolves the ref to the OLD sha by definition, so sha-equality would
//! leave a mirror permanently stale and break the documented "a second
//! materialize advances onto a new commit" contract. Separately, the
//! WORKTREE teardown + re-add is skipped when the tree is already this
//! mirror's worktree at the resolved sha with nothing modified or added
//! ([`tree_is_reusable`]) — pristine by inspection rather than by
//! rebuilding, so a peer's turn after the lock is cheap. A tree anyone
//! has written into is still torn down and rebuilt, exactly as before.

use super::{SourceSpec, WorkspaceSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::marker::PhantomData;
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

/// NOT `Clone`: it owns the workspace lock (`lock` below), and a lock
/// guard that can be duplicated is not a lock.
#[derive(Debug)]
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
    /// (#2399) The workspace lock, held for as long as this value is —
    /// because a caller that holds a `Materialized` goes on to READ the
    /// trees it names, and a peer `materialize` would otherwise tear those
    /// trees down mid-read. `None` only in tests that build the struct
    /// directly from already-resolved sources.
    ///
    /// Consequence worth stating: **dropping this value releases the
    /// workspace.** Keep it alive for exactly as long as you read the
    /// trees, and no longer.
    ///
    /// The field is `pub` only because `darkmux-lab`'s plan tests build a
    /// `Materialized` from already-resolved sources (with `lock: None`).
    /// `take()`-ing it out of a REAL one releases the workspace while the
    /// paths in `sources`/`files` are still held and still being read —
    /// which is precisely the failure this field exists to prevent. Drop
    /// the whole value instead.
    pub lock: Option<WorkspaceLock>,
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
    // from its `contained_child` path — a `?` rather than a silent
    // fallback (#2399 review), because a key that diverged from
    // `resolve_one`'s would cause a SPURIOUS fetch skip, not a loud error.
    let canon_mirror_root = mirror_root
        .canonicalize()
        .with_context(|| format!("canonicalizing mirror dir {}", mirror_root.display()))?;
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

    // Held for the whole call — clone/fetch, checkout, the walk below —
    // and then handed to the caller inside `Materialized`, which is what
    // keeps it held while the caller READS the trees (#2399 review).
    let canon_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace root {}", root.display()))?;
    let lock = WorkspaceLock::acquire(&canon_root)?;

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
        lock: Some(lock),
    })
}

/// The pull-request head namespace's own refspec — see
/// [`fetch_pull_heads_once`] for why this is NOT in [`ensure_fetch_refspecs`]'s
/// unconditional set.
const PULL_HEADS_REFSPEC: &str = "+refs/pull/*/head:refs/pull/*/head";

/// The refspecs a darkmux mirror fetches on EVERY clone/fetch, applied
/// idempotently (an `--add` of a spec already present would duplicate it,
/// and git then fetches it twice).
///
/// - `+refs/heads/*` — branches, force-updated so a rewritten branch
///   advances. `git clone --bare` does NOT configure this the way a normal
///   clone does, which is why it is set explicitly.
/// - `+refs/tags/*` — `+` force-update, same reason: git's default
///   auto-follow-tags won't move an EXISTING local tag ref that diverged.
///
/// `+refs/pull/*/head` (the fork-PR fix, #2310 P4d) is deliberately NOT
/// here any more — see [`fetch_pull_heads_once`]'s own doc for why #2404
/// P4d round 3 moved it off the unconditional path.
fn ensure_fetch_refspecs(mirror_path: &Path, source_id: &str) -> Result<()> {
    add_fetch_refspecs(mirror_path, source_id, &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"])
}

/// Idempotently `config --add remote.origin.fetch <spec>` for every spec
/// not already present. Used ONLY by [`ensure_fetch_refspecs`] (the
/// unconditional heads+tags set) — [`fetch_pull_heads_once`] deliberately
/// does NOT call this (#2404 P4d round 4): the pull-heads namespace is a
/// miss-recovery fetch, never a standing refspec, so it must never be
/// persisted into `remote.origin.fetch`.
fn add_fetch_refspecs(mirror_path: &Path, source_id: &str, specs: &[&str]) -> Result<()> {
    let existing = Command::new("git")
        .current_dir(mirror_path)
        .args(["config", "--get-all", "remote.origin.fetch"])
        .output()
        .with_context(|| format!("reading fetch refspecs for source '{source_id}'"))?;
    // A mirror with NO refspec configured yet exits non-zero here — an
    // empty set, not an error.
    let have: Vec<String> = if existing.status.success() {
        String::from_utf8_lossy(&existing.stdout).lines().map(|l| l.trim().to_string()).collect()
    } else {
        Vec::new()
    };
    for spec in specs {
        if have.iter().any(|h| h == spec) {
            continue;
        }
        run_git(
            Some(mirror_path),
            &["config", "--add", "remote.origin.fetch", spec],
            &format!("configuring fetch refspec {spec} for source '{source_id}'"),
        )?;
    }
    Ok(())
}

/// **Miss-recovery only — the fork-PR fix, re-scoped (#2404 P4d round 3),
/// and re-fixed to leave the mirror's config alone (#2404 P4d round 4).**
/// `+refs/pull/*/head` used to be part of every mirror's UNCONDITIONAL
/// fetch, on the doc-comment claim that "GitHub serves the namespace
/// read-only... this costs nothing elsewhere". Measured against a real
/// clone of kstrat2001/darkmux (2026-09): a warm (second, no-op) `git
/// fetch --prune` was ~0.3s slower with the pull refspec configured than
/// without it — inside the ~3s budget — but the pull namespace itself
/// (1331 refs vs. 374 heads+tags on this repo) pulled the mirror's ON-DISK
/// size from 39MB to 778MB, roughly 739MB over the ~50MB budget. That is
/// not "costs nothing" for an operator's disk on every single source this
/// crawl/review pipeline ever touches, the overwhelming majority of which
/// are same-repo branches that never need the pull namespace at all.
///
/// Round 3 re-scoped WHEN this fetches (miss-recovery only, not every
/// clone/fetch) but still called [`add_fetch_refspecs`] first, which
/// `config --add`s the refspec onto `remote.origin.fetch` — permanently.
/// That is exactly the cost this function exists to avoid: once added,
/// EVERY ordinary `git fetch --prune --prune-tags origin` afterward (no
/// explicit refspec — [`resolve_one`]'s unconditional fetch) re-fetches
/// the full pull-heads namespace on top of heads+tags, reproducing the
/// measured 739MB bloat on every single fetch of that mirror from then on,
/// not just the one recovery this was meant for.
///
/// Fixed here: this function no longer touches `remote.origin.fetch` at
/// all. An explicit refspec passed as a `git fetch` ARGUMENT (rather than
/// something configured via `config --add`) is one-shot — git fetches
/// exactly what is named and persists nothing — so the mirror's standing
/// refspec set stays heads+tags forever, exactly as [`ensure_fetch_
/// refspecs`] left it. Narrower still when the caller's `git_ref` is
/// already a real ref name (`refs/pull/<n>/head`): fetch only that one
/// ref, `<ref>:<ref>`, cheaper than the whole namespace. `derive_
/// workspace_spec` (the real production caller, `crates/darkmux-lab/src/
/// crawl/plan_sites_step.rs`) never actually produces that shape though —
/// it pins a BARE sha on purpose (a fixed point across force-pushes), and
/// a bare sha can't be named as a fetch destination without knowing which
/// PR it belongs to, so production always falls to the full pull-heads
/// wildcard below — still a one-shot argument, so still leaves the config
/// alone.
fn fetch_pull_heads_once(mirror_path: &Path, source_id: &str, git_ref: &str) -> Result<()> {
    let refspec =
        if git_ref.starts_with("refs/") { format!("+{git_ref}:{git_ref}") } else { PULL_HEADS_REFSPEC.to_string() };
    run_git(
        Some(mirror_path),
        &["fetch", "--prune", "origin", &refspec],
        &format!("fetching ref '{git_ref}' for source '{source_id}' (not found in heads/tags)"),
    )?;
    Ok(())
}

/// Whether a mirror clone should pass `--no-hardlinks` — see the call
/// site's own comment for the measured cost. `&[]` (no flag, hardlinking
/// allowed) for a LOCAL `path` origin; `&["--no-hardlinks"]` (forced copy)
/// for a `git` (remote) origin, where the flag is a no-op anyway (git
/// never hardlinks across a network clone) but costs nothing to keep as
/// the explicit, doc-carrying default.
fn no_hardlinks_flag(source: &SourceSpec) -> &'static [&'static str] {
    if source.path.is_some() { &[] } else { &["--no-hardlinks"] }
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
    if mirror_path.exists() {
        if let Some(defect) = mirror_defect(&mirror_path, origin) {
            // (#2399 review) Quarantine ONLY when a re-clone can follow it.
            // A `git`-origin mirror under --no-fetch cannot be rebuilt
            // offline, so moving it aside there would strand the operator
            // with neither a mirror nor a way to make one. Refuse instead,
            // and say plainly that nothing was moved.
            if !fetch && source.git.is_some() {
                bail!(
                    "mirror for source '{}' at {} failed its self-check ({defect}); it was left in place \
                     because re-cloning it needs the network — run without --no-fetch once to quarantine \
                     and rebuild it",
                    source.id,
                    mirror_path.display()
                );
            }
            quarantine_mirror(&mirror_path, &source.id, &defect)?;
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
            bail!(
                "mirror for source '{}' does not exist; run without --no-fetch once",
                source.id
            );
        }
        // (#2404 P4d round 3) `--no-hardlinks` forces a real byte-for-byte
        // object COPY even when the origin is a local directory git could
        // otherwise hardlink into instead. Measured against a 769MB local
        // mirror: with `--no-hardlinks` the clone duplicated the full
        // ~766MB of objects (0.76s); with hardlinking allowed, the clone's
        // OWN genuinely-new disk footprint was ~108KB (0.05s) — objects
        // shared with the source via hardlink rather than copied. That
        // cost is only worth paying for a REMOTE (`git`) origin, where
        // git's own clone protocol never hardlinks in the first place (the
        // flag is a no-op there) and where a mid-clone crash leaving a
        // hardlinked-into-nowhere mirror isn't a risk at all — the origin
        // isn't even on this filesystem. For a LOCAL `path` origin, the
        // flag turns every crawl/review source into a full duplicate of
        // its own already-on-disk repository for no benefit.
        let mirror_path_str = mirror_path.to_string_lossy();
        let mut clone_args: Vec<&str> = vec!["clone", "--bare"];
        clone_args.extend_from_slice(no_hardlinks_flag(source));
        clone_args.extend_from_slice(&["--", origin, &mirror_path_str]);
        run_git(
            None,
            &clone_args,
            &format!("cloning source '{}' ({origin})", source.id),
        )?;
        // See `ensure_fetch_refspecs` for what is configured and why.
        ensure_fetch_refspecs(&mirror_path, &source.id)?;
        bump_fetch_generation(&generation_key);
    } else if fetch && !fetch_is_redundant(gen_before, fetch_generation(&generation_key)) {
        // (#2310 P4d) An EXISTING mirror is brought up to the current
        // refspec set before the fetch, not only at clone time — a mirror
        // cloned before a refspec was added would otherwise never learn it,
        // and the ref it cannot see fails as "not found" at rev-parse with
        // no hint that a refspec was the cause.
        ensure_fetch_refspecs(&mirror_path, &source.id)?;
        run_git(
            Some(&mirror_path),
            &["fetch", "--prune", "--prune-tags", "origin"],
            &format!("fetching source '{}'", source.id),
        )?;
        bump_fetch_generation(&generation_key);
    }

    let git_ref = source.resolved_ref();
    let rev_parse_ref = |mirror_path: &Path| -> Result<std::process::Output> {
        Command::new("git")
            .current_dir(mirror_path)
            // `--end-of-options` is rev-parse's "stop parsing options" marker
            // that still treats what follows as a revision — plain `--` before
            // a rev-parse positional means "everything after is a path" and
            // breaks every valid ref.
            .args(["rev-parse", "--verify", "--end-of-options", &format!("{git_ref}^{{commit}}")])
            .output()
            .with_context(|| format!("running git rev-parse for source '{}'", source.id))
    };
    let mut rev_out = rev_parse_ref(&mirror_path)?;
    // (#2404 P4d round 3) The pull-heads namespace is no longer fetched
    // unconditionally — see `fetch_pull_heads_once`'s own doc for the
    // measured cost that moved it here. A miss against the ordinary
    // heads+tags mirror is retried EXACTLY ONCE against that namespace —
    // gated on `fetch` (mirrors both `git` and `path` origins: a `--no-
    // fetch` run promised fully-offline behavior against whatever's
    // already mirrored, so it must not reach out for a recovery fetch
    // either) — which is what makes a fork PR's head resolve without
    // paying the pull namespace's cost for every same-repo source that
    // never needed it.
    if !rev_out.status.success() && fetch {
        fetch_pull_heads_once(&mirror_path, &source.id, git_ref)?;
        rev_out = rev_parse_ref(&mirror_path)?;
    }
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
        // (#2399 review) A tree that is already this mirror's worktree, at
        // this exact sha, with nothing modified or added, is byte-identical
        // to what the teardown + re-add below would produce — so skip them.
        // That is what makes a peer's re-entry after the lock cheap, and it
        // never costs the pristine-tree guarantee: `tree_is_reusable`
        // refuses anything dirty (see the test that scribbles in a tree).
        if tree_is_reusable(&tree_path, &mirror_path, &sha) {
            if opts.read_only {
                make_tree_read_only(&tree_path)?;
            } else {
                make_tree_writable(&tree_path)?;
            }
            return Ok(MaterializedSource {
                id: source.id.clone(),
                sha,
                git_ref: git_ref.to_string(),
                tree: tree_path,
            });
        }
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
/// `<root>/.materialize.lock`, released when the LAST guard for that root
/// drops (including on an early `?` return, and on process exit when the
/// fd is closed).
///
/// `flock` locks the open file DESCRIPTION, not the process, so two
/// threads in one process that each open the file conflict with each other
/// exactly as two processes do — which is the case #2397's 8-wide NoModel
/// track actually produces.
///
/// **Re-entrant within one thread.** Since `Materialized` carries this
/// guard (#2399 review), a plain `flock` would turn the ordinary sequence
/// `let a = materialize(spec); let b = materialize(spec);` into a silent,
/// permanent hang — the same thread blocking on a lock it already holds
/// through a second file description. So a per-root registry records the
/// owning thread and a depth: a re-entry from the OWNING thread takes a
/// depth ticket and no new `flock`, while any other thread (or process)
/// still blocks. The `File` lives in the registry rather than in the
/// guard, so the `flock` is released exactly when the depth reaches zero,
/// whatever order the guards happen to drop in.
///
/// **It is `!Send`, deliberately (#2399 review).** The re-entrancy
/// registry keys on the ACQUIRING thread, so a guard that MOVED to
/// another thread would still be re-enterable by the thread that made it
/// — a probe measured exactly that: thread P materialized, handed the
/// value to thread H, materialized again in 216 ms while H was still
/// reading, and tore the tree down under it. No caller does this today
/// (both create and drop within one function), so rather than pay for a
/// cross-thread ownership handoff, the type refuses to travel:
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<darkmux_crew::workspace_spec::WorkspaceLock>();
/// ```
///
/// `Materialized` inherits this — it owns one — so the whole value stays
/// on the thread that materialized it.
///
/// **The consequence, stated plainly.** The lock is EXCLUSIVE for the
/// whole life of the `Materialized`, so N steps materializing ONE
/// workspace now run one after another, reads included — #2397's 8-wide
/// `plan.sites` track no longer overlaps ON A SINGLE WORKSPACE (different
/// workspaces are unaffected). That is the price of a read that cannot be
/// torn down mid-walk. Holding TWO guards for one workspace on two
/// threads at once therefore deadlocks; a caller takes what it needs and
/// drops the value. A shared read lock (downgrade to `LOCK_SH` once the
/// git work is done, with a fast path for "already at this sha") would
/// restore the overlap and is the obvious follow-up — it is deliberately
/// not in this packet, because it needs its own concurrency tests.
#[cfg(unix)]
#[derive(Debug)]
pub struct WorkspaceLock {
    /// The canonical workspace root this guard holds a ticket on.
    key: PathBuf,
    /// Makes the guard — and every `Materialized` that owns one — `!Send`.
    /// See the type doc: the re-entrancy registry keys on the ACQUIRING
    /// thread, so a guard that traveled would let its birth thread re-enter
    /// past a holder on another thread. A raw pointer is the standard way
    /// to spell this; nothing is ever read through it.
    not_send: PhantomData<*const ()>,
}

#[cfg(unix)]
struct HeldWorkspace {
    owner: std::thread::ThreadId,
    depth: usize,
    /// Dropping this closes the fd, which releases the `flock`.
    file: fs::File,
}

#[cfg(unix)]
fn held_workspaces() -> &'static Mutex<HashMap<PathBuf, HeldWorkspace>> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, HeldWorkspace>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
impl WorkspaceLock {
    /// `root` must already be canonical — the registry keys on it, and two
    /// spellings of one directory must not look like two workspaces.
    fn acquire(root: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let key = root.to_path_buf();

        // Re-entry from the owning thread: a depth ticket, no new flock.
        {
            let mut held = held_workspaces().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = held.get_mut(&key) {
                if entry.owner == std::thread::current().id() {
                    entry.depth += 1;
                    return Ok(Self { key, not_send: PhantomData });
                }
            }
        }

        // Anyone else waits. The registry mutex is NOT held across the
        // blocking flock — holding it there would stall every other
        // workspace in the process behind this one.
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

        let mut held = held_workspaces().lock().unwrap_or_else(|e| e.into_inner());
        held.insert(key.clone(), HeldWorkspace { owner: std::thread::current().id(), depth: 1, file });
        Ok(Self { key, not_send: PhantomData })
    }
}

#[cfg(unix)]
impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let mut held = held_workspaces().lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = held.get_mut(&self.key) else { return };
        entry.depth -= 1;
        if entry.depth > 0 {
            return;
        }
        // Last ticket: unlock explicitly, then drop the entry (whose
        // `File` close would release it anyway).
        unsafe {
            libc::flock(entry.file.as_raw_fd(), libc::LOCK_UN);
        }
        held.remove(&self.key);
    }
}

/// Non-Unix targets get no lock: `flock(2)` is POSIX, and darkmux is
/// POSIX-only elsewhere too (the audit flow sink, `bounded_command`'s
/// process groups). Concurrent materialization of ONE workspace is
/// therefore unsupported off Unix; a single materialize is unaffected.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct WorkspaceLock {
    /// Same `!Send` contract as the Unix guard, so the type behaves
    /// identically on every target even where it locks nothing.
    not_send: PhantomData<*const ()>,
}

#[cfg(not(unix))]
impl WorkspaceLock {
    fn acquire(_root: &Path) -> Result<Self> {
        Ok(Self { not_send: PhantomData })
    }
}

/// True when `tree_path` is ALREADY this mirror's worktree, checked out at
/// `sha`, with nothing modified, deleted or added — the only state in
/// which skipping the teardown + re-add produces the same bytes as doing
/// it (#2399 review). Anything it cannot prove (a git that errors, a
/// worktree belonging to a different mirror, a dirty tree) answers `false`
/// and falls back to the rebuild, so the safe direction is the default.
fn tree_is_reusable(tree_path: &Path, mirror_path: &Path, sha: &str) -> bool {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").current_dir(tree_path).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if git(&["rev-parse", "HEAD"]).as_deref() != Some(sha) {
        return false;
    }
    // It must be a worktree of THIS mirror, not a stale directory left by
    // some other repository that happens to sit at the same sha.
    let (Some(git_dir), Ok(canon_mirror)) = (git(&["rev-parse", "--absolute-git-dir"]), mirror_path.canonicalize())
    else {
        return false;
    };
    match Path::new(&git_dir).canonicalize() {
        Ok(canon_git_dir) if canon_git_dir.starts_with(&canon_mirror) => {}
        _ => return false,
    }
    // `--porcelain` prints one line per changed path and nothing at all
    // for a pristine tree; `--untracked-files=all` makes an added file
    // count, which is what the teardown used to remove.
    matches!(git(&["status", "--porcelain", "--untracked-files=all"]).as_deref(), Some(""))
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
/// answers almost every case. Two fallbacks behind it, in order:
///
/// 1. **Both sides are real local paths** — canonicalize and compare. This
///    covers a symlinked temp root (`/var` vs `/private/var` on macOS) and
///    is DECISIVE: two directories that canonicalize differently are two
///    different origins, and `/a/repo` is not `/a/repo.git` just because
///    one name ends in `.git`. The url normalization below never runs on
///    that pair.
/// 2. **Otherwise, remote-url normalization** — [`normalize_remote_url`].
fn same_origin(configured: &str, origin: &str) -> bool {
    if configured == origin {
        return true;
    }
    if configured.is_empty() {
        return false;
    }
    if let (Ok(a), Ok(b)) = (Path::new(configured).canonicalize(), Path::new(origin).canonicalize()) {
        return a == b;
    }
    normalize_remote_url(configured) == normalize_remote_url(origin)
}

/// Trim the two decorations git itself treats as noise on a remote url: a
/// trailing `/`, and a trailing `.git`. TRANSPORT IS NEVER NORMALIZED —
/// `git@host:o/r.git` and `https://host/o/r.git` name the same upstream
/// but are different access paths with different credentials, and a mirror
/// configured for one is not silently the other (#2399 review).
fn normalize_remote_url(url: &str) -> &str {
    let trimmed = url.trim_end_matches('/');
    match trimmed.strip_suffix(".git") {
        Some(base) => base.trim_end_matches('/'),
        None => trimmed,
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

    /// (#2404 P4d round 3) `--no-hardlinks` is the right default for a
    /// REMOTE `git` origin (git never hardlinks across a network clone
    /// anyway) but forces a full byte-for-byte object copy for a LOCAL
    /// `path` origin that could otherwise share objects with the source
    /// via hardlink — measured against a 769MB local mirror: ~766MB
    /// duplicated with the flag vs. ~108KB of genuinely new disk without
    /// it. See `no_hardlinks_flag`'s own doc.
    #[test]
    fn no_hardlinks_flag_is_absent_for_a_local_path_origin_and_present_for_a_git_origin() {
        let path_source = SourceSpec {
            id: "app".to_string(),
            git: None,
            path: Some("/tmp/some/local/repo".to_string()),
            git_ref: None,
            extras: Default::default(),
        };
        assert!(
            no_hardlinks_flag(&path_source).is_empty(),
            "a local path origin must allow hardlinking, not force a copy"
        );

        let git_source = SourceSpec {
            id: "app".to_string(),
            git: Some("https://github.com/kstrat2001/darkmux".to_string()),
            path: None,
            git_ref: None,
            extras: Default::default(),
        };
        assert_eq!(
            no_hardlinks_flag(&git_source),
            &["--no-hardlinks"],
            "a remote git origin keeps the explicit --no-hardlinks default"
        );
    }


    /// (#2310 P4d — RED before `ensure_fetch_refspecs` added the pull
    /// namespace: this failed at rev-parse with git's opaque "Needed a
    /// single revision", the exact failure the reviewer reproduced against
    /// a FORK pull request.) A fork PR's head lives in NO branch of the
    /// reviewed repository — only `refs/pull/<n>/head`. The mirror's fetch
    /// refspec has to name that namespace, and an EXISTING mirror (cloned
    /// before the ref was created, which is every real CI run) has to pick
    /// it up on its next fetch, not only at clone time.
    #[test]
    fn a_commit_reachable_only_from_a_pull_ref_resolves_after_a_fetch() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // First materialize clones the mirror while only `main` exists.
        let spec = spec_for("pullref-test", workdir.path(), source.path(), "main");
        materialize(&spec, RO).expect("the initial clone at main materializes");

        // NOW create a commit reachable only from refs/pull/7/head — the
        // shape of a fork PR: the branch it came from does not exist here.
        run(&["checkout", "-q", "-b", "tmp-fork-head"]);
        fs::write(source.path().join("fork.txt"), "from a fork\n").unwrap();
        run(&["add", "fork.txt"]);
        run(&["commit", "-q", "-m", "fork head"]);
        let fork_sha = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/pull/7/head", &fork_sha]);
        run(&["checkout", "-q", "main"]);
        run(&["branch", "-q", "-D", "tmp-fork-head"]);

        let pull_spec = spec_for("pullref-test", workdir.path(), source.path(), "refs/pull/7/head");
        let out = materialize(&pull_spec, RO).expect("a pull-ref source materializes");
        assert_eq!(out.sources[0].sha, fork_sha, "the checkout must be the pull ref's own commit");
        assert!(
            out.sources[0].tree.join("fork.txt").exists(),
            "the materialized tree must carry the fork commit's file"
        );
    }

    /// (#2404 P4d round 4) Red-prove the mirror-config leak round 3
    /// shipped: `fetch_pull_heads_once` used to call `add_fetch_refspecs`,
    /// which `config --add`s `+refs/pull/*/head:refs/pull/*/head` onto
    /// `remote.origin.fetch` PERMANENTLY. Once added, every ordinary
    /// `resolve_one` fetch afterward (no explicit refspec argument — it
    /// fetches whatever `remote.origin.fetch` names) re-pulls the entire
    /// pull-heads namespace on top of heads+tags, forever — the measured
    /// 739MB bloat, paid again on every single subsequent fetch of that
    /// mirror, not just the one recovery it was meant for. This test
    /// reuses the fork-PR miss-recovery scenario above and inspects the
    /// mirror's OWN git config afterward: `remote.origin.fetch` must carry
    /// only the unconditional heads+tags set, never a `refs/pull` entry —
    /// restoring the old `add_fetch_refspecs` call inside
    /// `fetch_pull_heads_once` turns this red.
    #[test]
    fn miss_recovery_never_persists_the_pull_heads_refspec_into_mirror_config() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let spec = spec_for("pullref-config-test", workdir.path(), source.path(), "main");
        materialize(&spec, RO).expect("the initial clone at main materializes");

        run(&["checkout", "-q", "-b", "tmp-fork-head-2"]);
        fs::write(source.path().join("fork2.txt"), "from a fork\n").unwrap();
        run(&["add", "fork2.txt"]);
        run(&["commit", "-q", "-m", "fork head 2"]);
        let fork_sha = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/pull/9/head", &fork_sha]);
        run(&["checkout", "-q", "main"]);
        run(&["branch", "-q", "-D", "tmp-fork-head-2"]);

        let pull_spec = spec_for("pullref-config-test", workdir.path(), source.path(), "refs/pull/9/head");
        materialize(&pull_spec, RO).expect("the miss-recovery fetch materializes the fork commit");

        let mirror_path = workdir.path().join("mirror").join("app.git");
        let out = Command::new("git")
            .current_dir(&mirror_path)
            .args(["config", "--get-all", "remote.origin.fetch"])
            .output()
            .expect("reading remote.origin.fetch from the mirror");
        let configured: Vec<String> =
            String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect();
        assert!(
            !configured.iter().any(|spec| spec.contains("refs/pull")),
            "remote.origin.fetch must never carry a pull-heads entry after miss-recovery, only \
             heads+tags — got {configured:?}"
        );
    }

    /// (#2404 P4d round 4) The shape production actually produces:
    /// `derive_workspace_spec` (`crates/darkmux-lab/src/crawl/
    /// plan_sites_step.rs`) pins a BARE sha, never a `refs/pull/<n>/head`
    /// literal — the fixed-point-across-force-pushes rationale in that
    /// function's own doc. The existing pull-ref test above pins
    /// `refs/pull/7/head` as the spec's `ref`, a shape production never
    /// takes; this test proves the miss-recovery path also resolves a
    /// bare sha reachable only from the pull-heads namespace, which must
    /// fall to `fetch_pull_heads_once`'s wildcard branch since a bare sha
    /// cannot be named as a narrow fetch destination.
    #[test]
    fn a_bare_sha_reachable_only_from_a_pull_ref_resolves_after_a_fetch() {
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let spec = spec_for("pullref-bare-sha-test", workdir.path(), source.path(), "main");
        materialize(&spec, RO).expect("the initial clone at main materializes");

        run(&["checkout", "-q", "-b", "tmp-fork-head-3"]);
        fs::write(source.path().join("fork3.txt"), "from a fork, bare sha\n").unwrap();
        run(&["add", "fork3.txt"]);
        run(&["commit", "-q", "-m", "fork head 3"]);
        let fork_sha = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/pull/11/head", &fork_sha]);
        run(&["checkout", "-q", "main"]);
        run(&["branch", "-q", "-D", "tmp-fork-head-3"]);

        // The spec names the BARE sha, not the ref — the shape
        // `derive_workspace_spec` actually mints.
        let bare_sha_spec = spec_for("pullref-bare-sha-test", workdir.path(), source.path(), &fork_sha);
        let out = materialize(&bare_sha_spec, RO).expect("a bare-sha fork commit materializes via miss-recovery");
        assert_eq!(out.sources[0].sha, fork_sha, "the checkout must be the pinned bare sha");
        assert!(
            out.sources[0].tree.join("fork3.txt").exists(),
            "the materialized tree must carry the fork commit's file"
        );
    }

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
                    // Take the sha and DROP the value inside the thread:
                    // `Materialized` owns the workspace lock (#2399 review),
                    // so holding six of them for one workspace at once would
                    // deadlock by construction — which is the contract, not
                    // an accident. See `WorkspaceLock`'s doc.
                    materialize(&spec, RW).map(|m| m.sources[0].sha.clone())
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut shas = std::collections::BTreeSet::new();
        for r in &results {
            let sha = r.as_ref().unwrap_or_else(|e| panic!("every concurrent materialize must succeed: {e:#}"));
            shas.insert(sha.clone());
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



    // ── #2399 second round: the guard's lifetime, tree reuse, and the
    //    --no-fetch/quarantine ordering ──

    /// (#2399 review MUST FIX) The lock has to outlive `materialize`'s
    /// RETURN, because both real callers read the tree afterwards
    /// (`crawl/plan_step.rs`, `crawl/plan_sites_step.rs` → `plan.rs`'s
    /// per-file `fs::metadata`/`fs::read`). Red before `Materialized`
    /// carried the guard: a peer's `materialize` tears `<root>/tree/<id>`
    /// down under the reader, and `plan.rs` swallows the resulting errors
    /// into `skipped: "stat error ..."` — a SILENT under-report of the
    /// crawl's coverage, not a crash.
    ///
    /// The peer here materializes the SAME workspace at a DIFFERENT ref,
    /// which is what forces a real teardown every round — a source that
    /// moves, or two steps pinned differently, under one workspace root.
    /// (An identical-ref peer would hit `tree_is_reusable` and never touch
    /// the tree at all, so it could not red-prove anything about the lock.)
    #[test]
    fn a_reader_holding_the_materialized_value_keeps_the_workspace_locked() {
        let source = init_source_repo();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(source.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        // `v0` holds ONLY a.txt; `main` advances to carry twenty more, so a
        // checkout swap genuinely removes the files the reader is reading.
        run(&["tag", "v0"]);
        for i in 0..20 {
            fs::write(source.path().join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "many"]);

        let workdir = TempDir::new().unwrap();
        let head_spec = std::sync::Arc::new(spec_for("t-reader", workdir.path(), source.path(), "main"));
        let pinned_spec = std::sync::Arc::new(spec_for("t-reader", workdir.path(), source.path(), "v0"));
        // Clone once up front so both threads race on an already-populated
        // workspace — the shape a second `plan.sites` step arrives in.
        drop(materialize(&head_spec, RW).unwrap());

        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let reader = {
            let spec = std::sync::Arc::clone(&head_spec);
            let gate = std::sync::Arc::clone(&gate);
            let reader_done = std::sync::Arc::clone(&reader_done);
            std::thread::spawn(move || {
                let m = materialize(&spec, RW).unwrap();
                // Only release the peer once we hold the value.
                gate.wait();
                let tree = m.sources[0].tree.clone();
                let files = m.files["app"].clone();
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
                let mut errors: Vec<String> = Vec::new();
                while std::time::Instant::now() < deadline {
                    for rel in &files {
                        if let Err(e) = fs::read(tree.join(rel)) {
                            errors.push(format!("{rel}: {e}"));
                        }
                    }
                }
                reader_done.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(m);
                errors
            })
        };

        let rebuilder = {
            let spec = std::sync::Arc::clone(&pinned_spec);
            let gate = std::sync::Arc::clone(&gate);
            let reader_done = std::sync::Arc::clone(&reader_done);
            std::thread::spawn(move || {
                gate.wait();
                let mut rounds = 0;
                while !reader_done.load(std::sync::atomic::Ordering::SeqCst) && rounds < 200 {
                    materialize(&spec, RW).unwrap();
                    rounds += 1;
                }
            })
        };

        let errors = reader.join().unwrap();
        rebuilder.join().unwrap();
        assert!(
            errors.is_empty(),
            "a reader holding its Materialized must never see the tree vanish under it ({} errors, first: {:?})",
            errors.len(),
            errors.first()
        );
    }

    /// (#2399 review) A tree already checked out at the resolved sha, with
    /// nothing modified, is REUSED — the inode of `<root>/tree/<id>`
    /// survives. Red before the reuse check: `resolve_one` unconditionally
    /// `remove_dir_all`s and re-`worktree add`s, so the directory is a new
    /// one every call.
    #[test]
    fn an_unchanged_tree_at_the_resolved_sha_is_reused_not_torn_down() {
        use std::os::unix::fs::MetadataExt;
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t-reuse", workdir.path(), source.path(), "main");

        let m1 = materialize(&spec, RW).unwrap();
        let tree = m1.sources[0].tree.clone();
        let ino1 = fs::metadata(&tree).unwrap().ino();
        drop(m1);

        let m2 = materialize(&spec, RW).unwrap();
        let ino2 = fs::metadata(&m2.sources[0].tree).unwrap().ino();
        assert_eq!(
            ino1, ino2,
            "an unchanged tree already at the resolved sha must be reused, not torn down and re-added"
        );
        assert!(m2.sources[0].tree.join("a.txt").exists());
    }

    /// (#2399 review) …but reuse never costs the pristine-tree guarantee:
    /// a tree someone has written into is torn down and rebuilt, exactly
    /// as before. This is the guard on the reuse check above.
    #[test]
    fn a_dirty_tree_is_still_torn_down_and_rebuilt_pristine() {
        use std::os::unix::fs::MetadataExt;
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t-dirty", workdir.path(), source.path(), "main");

        let m1 = materialize(&spec, RW).unwrap();
        let tree = m1.sources[0].tree.clone();
        let ino1 = fs::metadata(&tree).unwrap().ino();
        fs::write(tree.join("scribble.txt"), "someone wrote here\n").unwrap();
        fs::write(tree.join("a.txt"), "and edited a tracked file\n").unwrap();
        drop(m1);

        let m2 = materialize(&spec, RW).unwrap();
        let tree2 = m2.sources[0].tree.clone();
        assert!(!tree2.join("scribble.txt").exists(), "an untracked file must not survive");
        assert_eq!(fs::read_to_string(tree2.join("a.txt")).unwrap(), "hello\n", "a tracked edit must not survive");
        assert_ne!(ino1, fs::metadata(&tree2).unwrap().ino(), "a dirty tree is rebuilt, not reused");
    }

    /// (#2399 review) Quarantine only when a re-clone can actually follow.
    /// A `git`-origin mirror under `--no-fetch` cannot be re-cloned, so a
    /// defective one is REFUSED with the mirror left exactly where it is —
    /// moving it aside there would strand the operator with neither a
    /// mirror nor a way to rebuild one offline.
    #[test]
    fn no_fetch_refuses_a_defective_git_origin_mirror_without_moving_it_aside() {
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

        let json = serde_json::json!({
            "name": "t-nofetch-defect",
            "root": workdir.path().to_string_lossy(),
            "sources": [{"id": "app", "git": "https://example.invalid/never-cloned.git", "ref": "main"}]
        });
        let spec: WorkspaceSpec = serde_json::from_value(json).unwrap();

        let err = materialize(&spec, MaterializeOptions { fetch: false, read_only: false }).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--no-fetch"), "{msg}");
        assert!(msg.contains("not a bare repository"), "the refusal names the defect: {msg}");
        assert!(msg.contains("left in place"), "the refusal says the mirror was NOT moved: {msg}");
        assert!(bad.join("junk.txt").exists(), "the mirror must be left exactly where it was");
        assert!(
            corrupt_siblings(workdir.path()).is_empty(),
            "nothing may be quarantined when no re-clone can follow: {:?}",
            corrupt_siblings(workdir.path())
        );
    }

    /// (#2399 review) `remote.origin.url` equality is compared modulo the
    /// two decorations git itself treats as noise on a REMOTE url — a
    /// trailing `.git` and a trailing `/`. Transport is never normalized:
    /// ssh and https forms of one repo stay unequal, because they are
    /// different access paths with different credentials.
    #[test]
    fn same_origin_normalizes_the_git_suffix_and_trailing_slash_but_never_the_transport() {
        assert!(same_origin("https://example.com/o/r.git", "https://example.com/o/r"));
        assert!(same_origin("https://example.com/o/r/", "https://example.com/o/r"));
        assert!(same_origin("https://example.com/o/r.git/", "https://example.com/o/r"));
        assert!(!same_origin("git@example.com:o/r.git", "https://example.com/o/r.git"));
        assert!(!same_origin("https://example.com/o/other", "https://example.com/o/r"));
    }


    /// (#2399 second review) The dirty-tree guard above writes BOTH an
    /// untracked file and a tracked edit, so it stays green with
    /// `--untracked-files=no` — it cannot pin the untracked clause. This
    /// one dirties the tree ONLY with an untracked file, which is the case
    /// `git status` ignores by default and the teardown used to remove.
    #[test]
    fn an_untracked_file_alone_forces_a_rebuild() {
        use std::os::unix::fs::MetadataExt;
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t-untracked", workdir.path(), source.path(), "main");

        let m1 = materialize(&spec, RW).unwrap();
        let tree = m1.sources[0].tree.clone();
        let ino1 = fs::metadata(&tree).unwrap().ino();
        fs::write(tree.join("scribble.txt"), "untracked, nothing else\n").unwrap();
        drop(m1);

        let m2 = materialize(&spec, RW).unwrap();
        let tree2 = m2.sources[0].tree.clone();
        assert!(!tree2.join("scribble.txt").exists(), "an untracked file alone must not survive a materialize");
        assert_ne!(ino1, fs::metadata(&tree2).unwrap().ino(), "an untracked-only dirty tree is rebuilt, not reused");
    }

    /// (#2399 second review) A tree sitting at the right sha but belonging
    /// to a DIFFERENT mirror must be rebuilt from ours — otherwise reuse
    /// would adopt a worktree whose git dir, and therefore whose future
    /// checkouts, darkmux does not own.
    #[test]
    fn a_tree_at_the_right_sha_from_a_foreign_mirror_is_rebuilt() {
        use std::os::unix::fs::MetadataExt;
        let source = init_source_repo();
        let workdir = TempDir::new().unwrap();
        let spec = spec_for("t-foreign-tree", workdir.path(), source.path(), "main");

        let m1 = materialize(&spec, RW).unwrap();
        let tree = m1.sources[0].tree.clone();
        let sha = m1.sources[0].sha.clone();
        let ours = workdir.path().join("mirror").join("app.git");
        let ino1 = fs::metadata(&tree).unwrap().ino();
        drop(m1);

        // Hand the tree path over to a second, unrelated bare mirror of the
        // same source, checked out at the SAME sha — so only the mirror
        // identity distinguishes it from a reusable tree.
        let out = Command::new("git")
            .current_dir(&ours)
            .args(["worktree", "remove", "--force", "--", &tree.to_string_lossy()])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let elsewhere = TempDir::new().unwrap();
        let foreign = elsewhere.path().join("other.git");
        let out = Command::new("git")
            .args([
                "clone",
                "--bare",
                "--no-hardlinks",
                "--",
                &source.path().to_string_lossy(),
                &foreign.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = Command::new("git")
            .current_dir(&foreign)
            .args(["worktree", "add", "--detach", "--force", "--", &tree.to_string_lossy(), &sha])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let planted = Command::new("git")
            .current_dir(&tree)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&planted.stdout).trim(), sha, "the planted tree is at the same sha");

        let m2 = materialize(&spec, RW).unwrap();
        let tree2 = m2.sources[0].tree.clone();
        assert_ne!(ino1, fs::metadata(&tree2).unwrap().ino(), "a foreign worktree is never adopted");
        let git_dir = Command::new("git")
            .current_dir(&tree2)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .unwrap();
        let git_dir = String::from_utf8_lossy(&git_dir.stdout).trim().to_string();
        assert!(
            Path::new(&git_dir).canonicalize().unwrap().starts_with(ours.canonicalize().unwrap()),
            "the rebuilt tree must belong to THIS mirror, not {}: {git_dir}",
            foreign.display()
        );
    }

    /// (#2399 second review) The re-entrancy registry, named and BOUNDED.
    /// Materializing one workspace twice on ONE thread while the first
    /// value is still held must not block — without the owner check the
    /// second call waits on a `flock` its own thread holds, forever. The
    /// `recv_timeout` is what makes a regression fail red in CI instead of
    /// hanging the suite; the incidental coverage this replaces (the
    /// foreign-origin test) hangs for 90 s and reports nothing useful.
    #[test]
    fn materializing_one_workspace_twice_on_one_thread_does_not_block() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let source = init_source_repo();
            let workdir = TempDir::new().unwrap();
            let spec = spec_for("t-reentrant", workdir.path(), source.path(), "main");
            let first = materialize(&spec, RW).unwrap();
            // Still holding `first`: the guard's owner thread re-enters.
            let second = materialize(&spec, RW).unwrap();
            let same = first.sources[0].sha == second.sources[0].sha;
            let _ = tx.send(same);
            drop(second);
            drop(first);
        });

        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(true) => worker.join().unwrap(),
            Ok(false) => panic!("the re-entrant materialize resolved a different sha"),
            // Deliberately do NOT join here — the worker is wedged on a
            // lock it will never get, and joining would hang the suite
            // instead of reporting the regression.
            Err(_) => panic!(
                "materializing one workspace twice on one thread blocked for 15s — the \
                 re-entrancy registry regressed and every caller that holds a Materialized \
                 across a second materialize now deadlocks"
            ),
        }
    }

}
