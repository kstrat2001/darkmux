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
use std::path::{Component, Path, PathBuf};

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
///
/// (#2302) The messages below name the WORKDIR, never a CLI flag: a
/// workdir reaches this validator from a `--workdir` flag, from a Task,
/// and — since the crawl's follow-on template — from a mission step's own
/// `config.workdir`. An operator reading a failed step should not be sent
/// looking for a flag they never typed.
/// - `fleet::handle_claimed_job` (runner side; validates `WorkJob.workdir`
///   before invoking the dispatch path that consumes it)
pub fn validate_workdir(path: &Path) -> Result<PathBuf> {
    if let Some(offending) = first_user_symlink_in(path)
        .with_context(|| format!("checking the workdir for symlinks: {}", path.display()))?
    {
        bail!(
            "workdir traverses an operator-named symlink at {} — refusing to follow.\n  \
             Use the real directory path directly to prevent unintended r/w. \
             (macOS firmlinks /tmp, /var, /etc are tolerated; user-named symlinks anywhere \
             in the path are not.)",
            offending.display()
        );
    }
    let resolved = path.canonicalize().with_context(|| {
        format!(
            "workdir path does not exist or cannot be resolved: {} \
             (from the step's `config.workdir`, the task, or `--workdir`)",
            path.display()
        )
    })?;
    if !resolved.is_dir() {
        return Err(anyhow!(
            "workdir path is not a directory: {} \
             (from the step's `config.workdir`, the task, or `--workdir`)",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// Parse `workdir/.git` when it is a POINTER FILE — a file holding
/// `gitdir: <path>` — rather than the ordinary `.git` directory a normal
/// clone or the main checkout has.
///
/// Git writes that shape for three different things, and they do NOT
/// share a remedy, so this function only PARSES; [`classify_gitdir_target`]
/// decides which one it is and [`find_split_gitdirs`] bundles them:
/// - a `git worktree add` checkout, whose target is
///   `<main repo>/.git/worktrees/<name>` — a sibling tree, absolute by
///   default and RELATIVE under git 2.48's `worktree.useRelativePaths`;
/// - a submodule checkout, whose target is
///   `<superproject>/.git/modules/<path>`, usually relative;
/// - a `--separate-git-dir` checkout, whose target is an absolute
///   operator-chosen path under no `.git/` at all.
///
/// Either way the real git directory lives outside the workdir's own tree
/// and is therefore never bind-mounted alongside it (only `workdir` itself
/// gets mounted at `/workspace`, per `apply_volume_mounts`), so any git
/// command run inside the dispatch container fails with `fatal: not a git
/// repository: <that path>` (#2294) — a confusing failure to debug from
/// inside a sandbox, since nothing about it names the real cause.
///
/// Returns the parsed `gitdir:` target **verbatim** (not canonicalized —
/// this is a diagnostic, not a security check) when `workdir/.git` is a
/// pointer file of any of those kinds (see [`classify_gitdir_target`]).
/// Returns `None` for a plain directory, an
/// ordinary repo (`.git` is a directory), a workdir with no `.git` at
/// all, or a malformed/unreadable pointer file — this detector is purely
/// advisory (it powers a preflight warning), so any I/O hiccup reading it
/// folds to `None` rather than propagating an error that would block a
/// dispatch over a diagnostic side check.
///
/// **`symlink_metadata`, deliberately** (a `.git` SYMLINK therefore reads
/// as "not a pointer file" and returns `None`). Two reasons, both
/// intentional: `validate_workdir` above already refuses any workdir
/// whose own path components are operator-named symlinks, so the shape
/// barely arises; and a `.git` symlink points at a real `.git`
/// DIRECTORY (that is what the symlink form is for) rather than carrying
/// a `gitdir:` line, so following it would find no pointer to parse
/// anyway. If a real case ever shows up where a `.git` symlink hides a
/// pointer file, switch this one call to `metadata` — the rest of the
/// function needs no change.
pub fn worktree_gitdir_target(workdir: &Path) -> Option<PathBuf> {
    let git_path = workdir.join(".git");
    let meta = std::fs::symlink_metadata(&git_path).ok()?;
    if !meta.is_file() {
        // Directory (ordinary repo/main checkout) or absent — not a pointer.
        return None;
    }
    let contents = std::fs::read_to_string(&git_path).ok()?;
    let target = contents.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    Some(PathBuf::from(target))
}

/// What kind of checkout a `.git` POINTER FILE describes. Every kind
/// breaks git inside the dispatch container for the same underlying
/// reason (the real git directory lives outside what gets bind-mounted),
/// but they have DIFFERENT remedies, so the preflight warning has to tell
/// them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitdirPointerKind {
    /// A `git worktree add` checkout. Its `gitdir:` names
    /// `<main repo>/.git/worktrees/<name>` — a sibling tree entirely.
    /// Remedy: dispatch against a plain clone or the main checkout.
    Worktree,
    /// A submodule checkout. Its `gitdir:` names
    /// `<superproject>/.git/modules/<path>`, usually as a RELATIVE path
    /// (`../../.git/modules/vendor/sub`) that resolves inside the
    /// superproject's own tree. Remedy: dispatch against the
    /// SUPERPROJECT ROOT — the relative pointer then resolves inside the
    /// mounted workspace and git works normally.
    Submodule,
    /// A pointer file carrying NEITHER structural marker and naming an
    /// absolute path — the shape `git init --separate-git-dir <dir>` and
    /// `git clone --separate-git-dir <dir>` write, where the git
    /// directory sits at an operator-chosen path that is under no `.git/`
    /// at all. There is no main checkout to fall back to and no
    /// superproject to point at, so the remedy has to stay NEUTRAL:
    /// naming either of the other two sends the reader after a directory
    /// that does not exist.
    Separate,
}

/// A checkout whose `.git` is a pointer FILE rather than the ordinary
/// `.git` directory, found at or under an operator-supplied workdir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitGitdir {
    /// The directory whose `.git` is the pointer file. Equal to the
    /// workdir itself in the common case; an immediate CHILD of it for
    /// the crawl shape (see [`find_split_gitdirs`]).
    pub checkout: PathBuf,
    /// The parsed `gitdir:` target, verbatim (absolute or relative,
    /// exactly as git wrote it).
    pub target: PathBuf,
    pub kind: GitdirPointerKind,
    /// For [`GitdirPointerKind::Submodule`], the superproject root the
    /// pointer resolves against — the directory an operator should point
    /// the workdir at instead. `None` for every other kind, and `None`
    /// when the target's shape doesn't let one be derived.
    pub superproject: Option<PathBuf>,
}

/// True when `target` contains the component pair `.git/<name>` — the
/// structural marker that distinguishes a worktree pointer
/// (`.git/worktrees/…`) from a submodule pointer (`.git/modules/…`).
/// Component-wise, never a substring match, so a directory merely NAMED
/// `modules` somewhere else in the path can't be mistaken for the marker.
fn names_git_subdir(target: &Path, name: &str) -> bool {
    let mut comps = target.components().peekable();
    while let Some(c) = comps.next() {
        if c.as_os_str() == ".git" && comps.peek().is_some_and(|n| n.as_os_str() == name) {
            return true;
        }
    }
    false
}

/// Collapse `.` and `..` LEXICALLY (no filesystem access, no symlink
/// resolution). Used only to derive the submodule remedy path for a
/// diagnostic message, so a symlinked superproject producing a slightly
/// different-looking-but-equivalent path is acceptable; what matters is
/// that the operator gets a path they recognize.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Classify a parsed `gitdir:` target.
///
/// Keys on the STRUCTURAL marker first, never on absolute-vs-relative:
/// git 2.48+ writes a RELATIVE worktree pointer when
/// `worktree.useRelativePaths=true` (`gitdir: ../main-repo/.git/worktrees/wt`),
/// so treating "relative" as "submodule" would misclassify — and hand the
/// operator the wrong remedy — on any machine with that config set.
///
/// An ABSOLUTE target carrying neither marker is
/// [`GitdirPointerKind::Separate`], not a worktree: that is what
/// `--separate-git-dir` writes, and there is no main checkout for the
/// worktree remedy to point at.
pub fn classify_gitdir_target(target: &Path) -> GitdirPointerKind {
    if names_git_subdir(target, "worktrees") {
        GitdirPointerKind::Worktree
    } else if names_git_subdir(target, "modules") || target.is_relative() {
        // A relative pointer with neither marker still resolves INSIDE
        // some parent tree, which is the submodule shape, not the
        // sibling-tree shape a worktree has.
        GitdirPointerKind::Submodule
    } else {
        GitdirPointerKind::Separate
    }
}

/// The superproject root a submodule's `gitdir:` resolves against: the
/// parent of the `.git` directory the target names. `None` when the
/// target names no `.git` component (a shape we can't reason about).
fn submodule_superproject(checkout: &Path, target: &Path) -> Option<PathBuf> {
    let resolved = if target.is_absolute() {
        lexical_normalize(target)
    } else {
        lexical_normalize(&checkout.join(target))
    };
    let mut acc = PathBuf::new();
    for c in resolved.components() {
        if c.as_os_str() == ".git" {
            return Some(acc);
        }
        acc.push(c.as_os_str());
    }
    None
}

/// Find EVERY split-gitdir checkout at, or one level under, `workdir`.
///
/// Checks `workdir` itself first — a hit there is the whole answer, and
/// the returned vector holds exactly that one. When `workdir` has no
/// `.git` entry at all, scans its immediate subdirectories in sorted
/// order and returns ALL of them that hit.
///
/// **Why every hit and not the first.** Returning the first sorted child
/// is deterministic but carries no meaning: on the crawl shape below the
/// siblings under one root are DIFFERENT sources, and an `EdgeSpec` names
/// two of them in a single spec, so a two-source crawl always has at
/// least two sibling worktrees — while `crawl::unit_step` passes the
/// SHARED tree root as the workdir for every unit regardless of which
/// source that unit works on. "First sorted hit" would then have every
/// consumer (warning, model-facing note, flow record) describe `alpha`
/// while the dispatch works on `zeta`.
///
/// **Why one level down.** There are two producers of split-gitdir
/// workdirs in this repo, and only one of them puts the checkout AT the
/// workdir. `coder_phase` creates a git worktree and dispatches with
/// `--workdir <that worktree>`; the crawl's unit dispatch
/// (`darkmux_lab::crawl::unit_step`) mounts the workspace's `tree/`
/// PARENT so the container's `/workspace/<source>/…` paths resolve, and
/// each `tree/<source>` under it is a detached worktree of a bare mirror
/// that lives outside the mount. Inspecting only the mount root is blind
/// to the second one — and the crawler role is explicitly instructed to
/// run `git log` and `git show`, so it is the case that hurts most.
///
/// The child scan is skipped entirely when `workdir` has its own `.git`
/// (pointer or directory): a repository's own identity is the answer, and
/// a repo containing submodules must not report its submodules' pointers.
/// Symlinked children are skipped, matching this module's posture
/// everywhere else.
pub fn find_split_gitdirs(workdir: &Path) -> Vec<SplitGitdir> {
    if let Some(found) = split_gitdir_at(workdir) {
        return vec![found];
    }
    if std::fs::symlink_metadata(workdir.join(".git")).is_ok() {
        // An ordinary repo (or a `.git` we declined to parse) — its own
        // identity is the answer; don't go looking at its children.
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(workdir) else {
        return Vec::new();
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    children.sort();
    children.iter().filter_map(|c| split_gitdir_at(c)).collect()
}

/// [`find_split_gitdirs`] for exactly one directory, no child scan.
fn split_gitdir_at(dir: &Path) -> Option<SplitGitdir> {
    let target = worktree_gitdir_target(dir)?;
    let kind = classify_gitdir_target(&target);
    let superproject = match kind {
        GitdirPointerKind::Submodule => submodule_superproject(dir, &target),
        GitdirPointerKind::Worktree | GitdirPointerKind::Separate => None,
    };
    Some(SplitGitdir {
        checkout: dir.to_path_buf(),
        target,
        kind,
        superproject,
    })
}

/// Resolve the per-machine darkmux worktrees base directory:
/// `<darkmux root>/worktrees`.
///
/// (#2450) Routed through `paths::resolve` rather than straight at
/// `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped install keeps its worktrees
/// inside its own root — the same bug class #1585 fixed for `lab_dir`, #2093
/// for `hooks_outbox_dir`, #2363 for `flows_dir` and #2450 for `fleet_file`.
/// Probed and confirmed broken (a `DARKMUX_HOME` tempdir still resolved to the
/// real `~/.darkmux/worktrees`) before this fix.
///
/// **`ForceUser`, deliberately — NOT the `Auto` its sibling defaults use.**
/// `Auto` would prefer a project-local `./.darkmux` when the process happens
/// to be standing in one, making this base CWD-DEPENDENT. That is unacceptable
/// here specifically because this value feeds a SECURITY check: the daemon's
/// `worktree_contained` / `validate_remote_workdir` containment test (#840)
/// compares a tailnet-supplied workdir against this base, and `darkmux serve`
/// (whose cwd is wherever the operator launched it) must agree with
/// `coder_phase` (whose cwd is the repo) about what "the worktrees base" IS.
/// Under `Auto` those two disagree exactly when one of them stands in a repo
/// carrying a `./.darkmux` — which `lessons.rs` creates in every repo a coder
/// dispatch has recorded a lesson in. `ForceUser` still honors `DARKMUX_HOME`
/// (that branch short-circuits ahead of the scope match), which is the whole
/// bug being fixed, while keeping the base a single per-machine constant.
///
/// The single canonical implementation — previously triplicated across
/// `coder_phase.rs`, this module, and `darkmux-serve/src/lib.rs`, two of
/// which resolved HOME via `std::env::var("HOME")` while the third used
/// `dirs::home_dir()`. The two can disagree (e.g. HOME set but malformed
/// UTF-8, or a platform where `dirs::home_dir()` consults an API beyond
/// the env var), and the serve copy feeds the worktree containment check
/// (security-adjacent — see `validate_remote_workdir` below and
/// `worktree_contained` in darkmux-serve). Unified on `dirs::home_dir()`
/// semantics here; both other call sites now re-point at this function.
pub fn worktrees_base_dir() -> PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::ForceUser).root.join("worktrees")
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
            "workdir traverses an operator-named symlink at {} — refusing to follow. \
             Queue-originated dispatches must use real directory paths under the \
             worktrees base.",
            offending.display()
        );
    }

    // 2. Must exist and be a directory.
    let resolved = path.canonicalize().with_context(|| {
        format!(
            "workdir path does not exist or cannot be resolved: {} \
             (from the queued job's `workdir`)",
            path.display()
        )
    })?;
    if !resolved.is_dir() {
        return Err(anyhow!(
            "workdir path is not a directory: {} (from the queued job's `workdir`)",
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

    // ─── worktree_gitdir_target (#2294) ────────────────────────────────
    //
    // Uses a REAL `git worktree add` fixture (via the `git` binary), not a
    // hand-written `.git` file — the exact on-disk shape git writes is the
    // thing under test, and a hand-rolled stand-in would prove nothing
    // about the real case.

    /// The `-c` flags EVERY fixture git invocation carries, plus the
    /// config-file isolation around them.
    ///
    /// The suite must assert BEHAVIOR, not the ambient git config of
    /// whatever machine runs it:
    /// - `worktree.useRelativePaths` decides whether git 2.48+ writes an
    ///   absolute or a relative `gitdir:` pointer. Left ambient, the
    ///   absolute-pointer fixture below silently becomes a relative one on
    ///   an operator who set it (this machine's git does write relative
    ///   pointers when it's on), and the `is_absolute()` assertion becomes
    ///   a statement about their `~/.gitconfig`.
    /// - `commit.gpgsign=true` in a global config makes the fixture's own
    ///   `git commit` fail — a red test that has nothing to do with the
    ///   code under test.
    ///
    /// `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` pointed at `/dev/null` cut
    /// out every OTHER ambient knob too (`init.defaultBranch`, hooks,
    /// templates, aliases); the explicit `-c` flags stay anyway so the two
    /// knobs this file actually depends on are pinned visibly rather than
    /// only by the absence of a config file.
    fn git_cmd(cwd: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .args(["-c", "commit.gpgsign=false"]);
        cmd
    }

    fn run_git_with(relative_worktree_paths: bool, args: &[&str], cwd: &Path) {
        let mut cmd = git_cmd(cwd);
        cmd.args([
            "-c",
            if relative_worktree_paths {
                "worktree.useRelativePaths=true"
            } else {
                "worktree.useRelativePaths=false"
            },
        ]);
        let out = cmd
            .args(args)
            .output()
            .expect("git must be on PATH to run this test");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run_git(args: &[&str], cwd: &Path) {
        run_git_with(false, args, cwd);
    }

    /// Build a real repo with one commit + a real worktree off it. Returns
    /// (tempdir-guard, main-repo-path, worktree-path).
    /// `relative_worktree_paths` selects git's absolute (default) vs
    /// relative (`worktree.useRelativePaths=true`, git 2.48+) pointer form.
    fn real_worktree_fixture_with(relative_worktree_paths: bool) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("main-repo");
        std::fs::create_dir(&main_repo).unwrap();
        run_git_with(relative_worktree_paths, &["init", "-q", "-b", "main"], &main_repo);
        std::fs::write(main_repo.join("f.txt"), "hello\n").unwrap();
        run_git_with(relative_worktree_paths, &["add", "f.txt"], &main_repo);
        run_git_with(relative_worktree_paths, &["commit", "-q", "-m", "init"], &main_repo);
        let worktree = tmp.path().join("the-worktree");
        run_git_with(
            relative_worktree_paths,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
            &main_repo,
        );
        (tmp, main_repo, worktree)
    }

    fn real_worktree_fixture() -> (TempDir, PathBuf, PathBuf) {
        real_worktree_fixture_with(false)
    }

    /// `find_split_gitdirs` for the tests that are about ONE checkout, with
    /// the "exactly one" part asserted rather than assumed — a helper that
    /// silently took the first element would hide the very multi-hit
    /// behavior the plural return exists for.
    fn one_split_gitdir(workdir: &Path) -> Option<SplitGitdir> {
        let found = find_split_gitdirs(workdir);
        assert!(found.len() <= 1, "expected at most one hit, got {found:?}");
        found.into_iter().next()
    }

    #[test]
    fn worktree_gitdir_target_detects_a_real_git_worktree() {
        let (_tmp, main_repo, worktree) = real_worktree_fixture();
        let target = worktree_gitdir_target(&worktree)
            .expect("a real `git worktree add` checkout must be detected");
        // git writes an absolute path into `.git/worktrees/<name>` under the
        // MAIN repo's own `.git` — outside the worktree's own tree entirely.
        // Canonicalize `main_repo` before comparing: on macOS, `TempDir`
        // paths live under `/var/...` while `git` itself resolves and
        // writes the firmlink-collapsed `/private/var/...` form, so a
        // literal-string `starts_with` on the uncanonicalized tempdir path
        // would false-fail on exactly this platform.
        let canon_main_repo = main_repo.canonicalize().unwrap();
        // Absolute is a property of THIS fixture, which pins
        // `worktree.useRelativePaths=false` — not of git in general. The
        // relative form git 2.48+ writes under that knob is real, and
        // covered by its own tests below.
        assert!(target.is_absolute(), "gitdir target must be absolute: {target:?}");
        assert!(
            target.starts_with(canon_main_repo.join(".git").join("worktrees")),
            "gitdir target {target:?} must point into the main repo's .git/worktrees/ \
             (canonical main repo: {canon_main_repo:?})"
        );
        assert!(
            !target.starts_with(&worktree),
            "gitdir target {target:?} must NOT be inside the worktree tree itself \
             (that's the whole bug: it's outside what gets bind-mounted)"
        );
    }

    #[test]
    fn worktree_gitdir_target_is_none_for_the_main_checkout() {
        let (_tmp, main_repo, _worktree) = real_worktree_fixture();
        // The main repo's own `.git` is an ordinary directory, not a
        // worktree pointer file.
        assert!(worktree_gitdir_target(&main_repo).is_none());
    }

    #[test]
    fn worktree_gitdir_target_is_none_for_a_plain_directory() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("not-a-repo-at-all");
        std::fs::create_dir(&plain).unwrap();
        assert!(worktree_gitdir_target(&plain).is_none());
    }

    #[test]
    fn worktree_gitdir_target_is_none_when_git_missing_entirely() {
        let tmp = TempDir::new().unwrap();
        assert!(worktree_gitdir_target(tmp.path()).is_none());
    }

    #[test]
    fn worktree_gitdir_target_is_none_for_malformed_pointer_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "not a gitdir line at all\n").unwrap();
        assert!(worktree_gitdir_target(tmp.path()).is_none());
    }

    // ─── relative `gitdir:` pointers (git 2.48+ useRelativePaths) ──────

    /// Production parses the relative form correctly and MUST NOT read it
    /// as a submodule — the two have different remedies. Hand-written
    /// rather than git-produced so the coverage holds on every git
    /// version, including ones predating `worktree.useRelativePaths`; the
    /// real-git counterpart is the next test.
    #[test]
    fn a_relative_worktree_pointer_is_not_mistaken_for_a_submodule() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().join("the-worktree");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: ../main-repo/.git/worktrees/the-worktree\n",
        )
        .unwrap();
        let target = worktree_gitdir_target(&wt).expect("a relative pointer is still a pointer");
        assert!(target.is_relative(), "{target:?}");
        assert_eq!(
            classify_gitdir_target(&target),
            GitdirPointerKind::Worktree,
            "relative-vs-absolute must not decide the kind; the `.git/worktrees` marker does"
        );
        let found = one_split_gitdir(&wt).expect("must be found");
        assert_eq!(found.kind, GitdirPointerKind::Worktree);
        assert_eq!(found.superproject, None, "a worktree has no superproject remedy");
    }

    /// The same shape, produced by REAL git under
    /// `worktree.useRelativePaths=true`. On git 2.48+ (this machine) the
    /// pointer really is relative; on older git the knob is ignored and
    /// the pointer is absolute — either way the detector must find it and
    /// call it a worktree, which is what this asserts unconditionally.
    #[test]
    fn real_git_relative_worktree_pointer_is_detected_as_a_worktree() {
        let (_tmp, _main_repo, worktree) = real_worktree_fixture_with(true);
        let raw = std::fs::read_to_string(worktree.join(".git")).unwrap();
        let found = one_split_gitdir(&worktree).expect("a real worktree must be detected");
        assert_eq!(
            found.kind,
            GitdirPointerKind::Worktree,
            "raw pointer file was: {raw:?}"
        );
        if raw.contains("gitdir: ..") {
            assert!(
                found.target.is_relative(),
                "git honored useRelativePaths, so the parsed target must be relative: {raw:?}"
            );
        }
    }

    // ─── submodules (a pointer file that is NOT a worktree) ────────────

    /// Build a real superproject with a real submodule at `vendor/sub`.
    /// Returns (tempdir-guard, superproject-path, submodule-checkout-path).
    fn real_submodule_fixture() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let inner = tmp.path().join("inner-repo");
        std::fs::create_dir(&inner).unwrap();
        run_git(&["init", "-q", "-b", "main"], &inner);
        std::fs::write(inner.join("lib.txt"), "lib\n").unwrap();
        run_git(&["add", "lib.txt"], &inner);
        run_git(&["commit", "-q", "-m", "inner"], &inner);

        let super_repo = tmp.path().join("super-repo");
        std::fs::create_dir(&super_repo).unwrap();
        run_git(&["init", "-q", "-b", "main"], &super_repo);
        std::fs::write(super_repo.join("top.txt"), "top\n").unwrap();
        run_git(&["add", "top.txt"], &super_repo);
        run_git(&["commit", "-q", "-m", "super"], &super_repo);
        // Modern git refuses the local `file://` transport for submodules
        // unless this is set — nothing to do with the code under test.
        run_git(
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                inner.to_str().unwrap(),
                "vendor/sub",
            ],
            &super_repo,
        );
        let sub = super_repo.join("vendor").join("sub");
        (tmp, super_repo, sub)
    }

    /// A submodule checkout has a `.git` POINTER FILE just like a
    /// worktree, so the old code diagnosed it as a worktree and told the
    /// operator to point at "a plain clone or the main checkout" — which
    /// is not the fix. The fix is the SUPERPROJECT ROOT: the pointer is
    /// relative, so once the superproject is what's mounted, it resolves
    /// inside the mount and git works normally.
    #[test]
    fn a_submodule_checkout_is_classified_as_a_submodule_naming_the_superproject() {
        let (_tmp, super_repo, sub) = real_submodule_fixture();
        let found = one_split_gitdir(&sub).expect("a submodule checkout is a split gitdir");
        assert_eq!(
            found.kind,
            GitdirPointerKind::Submodule,
            "target was {:?}",
            found.target
        );
        let superproject = found
            .superproject
            .expect("a submodule must name its superproject as the remedy");
        assert_eq!(
            superproject.canonicalize().unwrap(),
            super_repo.canonicalize().unwrap(),
            "the remedy must be the superproject ROOT (parsed target: {:?})",
            found.target
        );
    }

    /// The superproject itself is an ordinary repo — its `.git` is a
    /// directory — so it must not trip the detector, AND the child scan
    /// must not reach down into `vendor/sub` and report the submodule's
    /// pointer as if it were the workdir's own problem.
    #[test]
    fn a_superproject_is_not_reported_via_its_own_submodule() {
        let (_tmp, super_repo, _sub) = real_submodule_fixture();
        assert_eq!(find_split_gitdirs(&super_repo), Vec::new());
    }

    // ─── the crawl shape: a worktree ONE LEVEL under the workdir ───────

    /// `darkmux_lab::crawl::unit_step` dispatches with
    /// `workdir: <workspace>/tree` — the PARENT of each source's checkout,
    /// so the container's `/workspace/<source>/…` paths resolve — and each
    /// `tree/<source>` is a detached worktree of a bare mirror outside the
    /// mount. The mount root itself has no `.git` at all, so a detector
    /// that only inspects the workdir is structurally blind to it, while
    /// the crawler role is explicitly told to run `git log` and `git show`.
    #[test]
    fn find_split_gitdirs_sees_a_worktree_one_level_under_the_workdir() {
        let (tmp, main_repo, _wt) = real_worktree_fixture();
        // Rebuild the crawl layout: <root>/tree/<source> is the worktree,
        // <root>/tree is what gets mounted.
        let tree_root = tmp.path().join("tree");
        std::fs::create_dir(&tree_root).unwrap();
        let source_tree = tree_root.join("the-source");
        run_git(
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                source_tree.to_str().unwrap(),
            ],
            &main_repo,
        );

        // The exact blindness the old detector had:
        assert_eq!(
            worktree_gitdir_target(&tree_root),
            None,
            "the mounted root has no .git of its own — this is why the workdir-only check missed it"
        );
        assert!(worktree_gitdir_target(&source_tree).is_some());

        let found = one_split_gitdir(&tree_root).expect("the child worktree must be found");
        assert_eq!(found.checkout, source_tree);
        assert_eq!(found.kind, GitdirPointerKind::Worktree);
    }

    #[test]
    fn find_split_gitdirs_prefers_the_workdir_itself_over_any_child() {
        let (tmp, _main_repo, worktree) = real_worktree_fixture();
        // A child that is ALSO a pointer file — the workdir's own identity
        // must win.
        let decoy = worktree.join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        std::fs::write(decoy.join(".git"), "gitdir: /nowhere/.git/worktrees/x\n").unwrap();
        let found = one_split_gitdir(&worktree).expect("must find the workdir's own pointer");
        assert_eq!(found.checkout, worktree);
        drop(tmp);
    }

    #[test]
    fn find_split_gitdirs_is_empty_for_a_plain_tree_of_plain_children() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("b").join("f.txt"), "x").unwrap();
        assert_eq!(find_split_gitdirs(&root), Vec::new());
    }

    /// A multi-source crawl is the ordinary case, not an edge one: every
    /// source materializes at `<workspace>/tree/<source id>` (siblings
    /// under one root), an `EdgeSpec` names TWO sources in a single spec,
    /// and `crawl::unit_step` passes the SHARED `tree/` root as the
    /// workdir for every unit regardless of which source that unit works
    /// on. Returning only the first sorted child would then describe
    /// `alpha` to a dispatch that is working on `zeta` — deterministic,
    /// and meaningless.
    #[test]
    fn find_split_gitdirs_returns_every_sibling_checkout_not_just_the_first() {
        let (tmp, main_repo, _wt) = real_worktree_fixture();
        let tree_root = tmp.path().join("tree");
        std::fs::create_dir(&tree_root).unwrap();
        for source in ["zeta", "alpha"] {
            run_git(
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    tree_root.join(source).to_str().unwrap(),
                ],
                &main_repo,
            );
        }
        // A plain sibling directory that is NOT a checkout must not appear.
        std::fs::create_dir(tree_root.join("notes")).unwrap();

        let found = find_split_gitdirs(&tree_root);
        let names: Vec<String> = found
            .iter()
            .map(|f| f.checkout.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["alpha".to_string(), "zeta".to_string()],
            "every sibling checkout, in sorted order"
        );
        assert!(found.iter().all(|f| f.kind == GitdirPointerKind::Worktree));
    }

    /// `git init --separate-git-dir` / `git clone --separate-git-dir`
    /// write an ABSOLUTE pointer carrying neither `.git/worktrees` nor
    /// `.git/modules`. Classifying that as a worktree hands the reader the
    /// worktree remedy ("point at the main checkout") when there is no
    /// main checkout to point at.
    #[test]
    fn a_separate_git_dir_pointer_is_its_own_kind_not_a_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("the-repo");
        let gitdir = tmp.path().join("elsewhere").join("the-repo.git");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir_all(gitdir.parent().unwrap()).unwrap();
        run_git(
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                gitdir.to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
            tmp.path(),
        );
        let target = worktree_gitdir_target(&repo).expect("--separate-git-dir writes a pointer");
        assert!(target.is_absolute(), "{target:?}");
        assert_eq!(classify_gitdir_target(&target), GitdirPointerKind::Separate);
        let found = one_split_gitdir(&repo).expect("must be detected");
        assert_eq!(found.kind, GitdirPointerKind::Separate);
        assert_eq!(
            found.superproject, None,
            "there is no superproject and no main checkout to name"
        );
    }

    /// The scan is ONE level, not recursive — a pointer two levels down
    /// is not the crawl shape and must not be reported (an unbounded walk
    /// on an arbitrary `--workdir` would be both slow and noisy).
    #[test]
    fn find_split_gitdirs_does_not_recurse_past_one_level() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        assert_eq!(find_split_gitdirs(&root), Vec::new());
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

    /// (#2302) The refusal names the WORKDIR, not a flag. A mission step
    /// sets `config.workdir` (the crawl's follow-on template does), and an
    /// operator reading that step's failure must not be sent looking for a
    /// `--workdir` they never typed.
    #[test]
    fn a_bad_workdir_names_the_field_not_a_cli_flag() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let err = format!("{:#}", validate_workdir(&missing).unwrap_err());
        assert!(err.contains("workdir path does not exist"), "{err}");
        assert!(err.contains("config.workdir"), "it names where a workdir comes from: {err}");
        assert!(!err.contains("--workdir path"), "no CLI-flag vocabulary: {err}");

        let file = tmp.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        let err = format!("{:#}", validate_workdir(&file).unwrap_err());
        assert!(err.contains("workdir path is not a directory"), "{err}");
        assert!(!err.contains("--workdir path"), "no CLI-flag vocabulary: {err}");
    }

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

    /// (#2450) The worktrees base must scope under `DARKMUX_HOME`. Probed
    /// before the fix and confirmed broken: with `DARKMUX_HOME` pointed at a
    /// throwaway root, `worktrees_base_dir()` still returned the operator's
    /// REAL `~/.darkmux/worktrees` — and this base is both a WRITE target
    /// (`coder_phase` creates git worktrees under it) and the daemon's
    /// remote-workdir containment base (#840).
    #[test]
    #[serial_test::serial]
    fn worktrees_base_dir_honors_darkmux_home() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let base = worktrees_base_dir();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(
            base,
            tmp.path().join("worktrees"),
            "must scope under DARKMUX_HOME, not the real user home"
        );
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
