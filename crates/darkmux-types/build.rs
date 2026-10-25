//! (#1129) Capture a build tag at build time so the observability viewer
//! header, `darkmux doctor`, and `--version` can show WHICH build is running.
//! The package version (`CARGO_PKG_VERSION`) doesn't change between releases,
//! so it alone can't tell an operator whether a daemon has their latest code
//! — or whether it's a packaged release at all.
//!
//! Precedence for the tag (`darkmux_types::build_version()` reads it). A git
//! checkout with changes is never a release, flag or not: it shows as in 3.
//! Cargo's own `.cargo-ok` marker in a `cargo install --git` checkout is not
//! a change (#3380). That checkout carries no tags, so a tagged install is a
//! release through `DARKMUX_RELEASE` (1), with git still naming the commit.
//!   1. **release** — `DARKMUX_RELEASE` is set. The Homebrew stable formula
//!      stamps this (it builds from a release tarball with no `.git`, so the
//!      git fallback below would otherwise leave it indistinguishable from a
//!      bare source build) and, alongside it, `DARKMUX_COMMIT` with the full
//!      commit of the tag the formula's `url` points at — a tarball has no
//!      `.git`, so without the env the commit would stay empty (#3373). Shows
//!      `<version> (release)`, or `<version> (release a1b2c3d)` when the
//!      commit is known. A `DARKMUX_COMMIT` that is not 40 hex characters is
//!      ignored, the same as unset.
//!   2. **release, from git** — a clean checkout whose `HEAD` carries the tag
//!      `v<version>`. Shows `<version> (release a1b2c3d)`.
//!   3. **git SHA** — any other git checkout (dev build, or `brew install
//!      --HEAD`): short SHA with a `✱` suffix when the tree is dirty. Shows
//!      `<version> (a1b2c3d✱)`.
//!   4. **empty** — no release flag, no git: source tarball build. Shows the
//!      version alone.
//!
//! Beside the tag it bakes the full commit (`DARKMUX_BUILD_COMMIT`) and the
//! kind (`DARKMUX_BUILD_KIND`: `release`, `dev`, or empty when neither is
//! known), which `darkmux_types::build_info()` reads, so a run records what
//! built the darkmux that ran it. When git is unavailable (a release tarball)
//! the full commit rides in the `DARKMUX_COMMIT` env var instead (#3373).
//!
//! Dep-free by design (CLAUDE.md: "don't add dependencies casually") — it
//! shells to `git`. Lives in the foundation crate so serve + doctor + the
//! binary all read one source of truth.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let git = git_state();
    let flagged = std::env::var_os("DARKMUX_RELEASE").is_some();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // (#3373) A release tarball has no `.git`, so git can't name the commit;
    // the formula stamps it here. Only a full 40-char hex SHA is accepted.
    // Git still wins when it can speak.
    let env_commit_raw = std::env::var("DARKMUX_COMMIT").ok();
    let env_commit = env_commit_raw
        .as_deref()
        .and_then(accepted_commit)
        .map(str::to_string);
    let (tag, kind) = build_tag(flagged, git.as_ref(), &version, env_commit.as_deref());
    println!("cargo:rustc-env=DARKMUX_BUILD_TAG={tag}");
    println!("cargo:rustc-env=DARKMUX_BUILD_KIND={kind}");
    println!("cargo:rustc-env=DARKMUX_BUILD_COMMIT={}", git.as_ref().map(|g| g.commit.as_str()).or(env_commit.as_deref()).unwrap_or(""));

    // Re-run when the release flag, the commit env, or HEAD moves, so the
    // baked tag can't go stale across rebuilds.
    println!("cargo:rerun-if-env-changed=DARKMUX_RELEASE");
    println!("cargo:rerun-if-env-changed=DARKMUX_COMMIT");
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // A release claim must not outlive an edit: HEAD and the index do not
    // move when a tracked file is changed and left unstaged, so watch the
    // tracked tree itself. Only for a release, so a dev rebuild stays cheap.
    if kind == "release" && git.is_some() {
        for path in tracked_top_level() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// The repo's tracked top-level entries (folders and files), absolute.
fn tracked_top_level() -> Vec<PathBuf> {
    let (Some(root), Some(files)) = (git_stdout(&["rev-parse", "--show-toplevel"]), git_stdout(&["ls-files", "--full-name", ":/"]))
    else {
        return Vec::new();
    };
    let mut tops: Vec<&str> = files.lines().filter_map(|f| f.split('/').next()).collect();
    tops.sort_unstable();
    tops.dedup();
    tops.into_iter().map(|t| Path::new(&root).join(t)).filter(|p| p.exists()).collect()
}

/// The git files whose change means the tag may have moved: `HEAD` and the
/// index in THIS checkout's git dir, plus the branch ref `HEAD` points at.
///
/// (#2976) Asked of git rather than hardcoded as `../../.git/…`. In a git
/// worktree `.git` is a FILE pointing elsewhere, so those paths never existed,
/// and cargo re-runs a build script whose watched path is missing on EVERY
/// build: `darkmux-types` rebuilt each time, relinking every binary above
/// it. Only existing paths are emitted, for the same reason. No git (a
/// release tarball) emits nothing, and the env watch alone keeps the script
/// from re-running.
fn git_watch_paths() -> Vec<PathBuf> {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let Some(out) = git_stdout(&["rev-parse", "--git-dir", "--git-common-dir"]) else {
        return Vec::new();
    };
    let mut dirs = out.lines().map(|l| manifest_dir.join(l.trim()));
    let (Some(git_dir), Some(common_dir)) = (dirs.next(), dirs.next()) else {
        return Vec::new();
    };
    // Tags too: tagging the release commit makes the build a release one.
    let mut paths = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common_dir.join("refs").join("tags"),
        common_dir.join("packed-refs"),
    ];
    // A commit moves the branch ref, not `HEAD` itself (which stays
    // `ref: refs/heads/<branch>`). Detached HEAD has no ref to add.
    if let Some(head_ref) = git_stdout(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(common_dir.join(head_ref.trim()));
    }
    paths.retain(|p: &PathBuf| Path::exists(p));
    paths
}

/// `git <args>`'s trimmed stdout, or `None` when git is missing or fails.
fn git_stdout(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// What git says about the checkout being built.
struct GitState {
    commit: String,
    short: String,
    dirty: bool,
    tags: Vec<String>,
}

/// `None` when git is unavailable (no `.git`, no `git` binary, empty repo).
fn git_state() -> Option<GitState> {
    let commit = git_stdout(&["rev-parse", "HEAD"])?;
    let short = git_stdout(&["rev-parse", "--short", "HEAD"])?;
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| porcelain_is_dirty(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false);
    let tags = git_stdout(&["tag", "--points-at", "HEAD"])
        .map(|t| t.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    Some(GitState { commit, short, dirty, tags })
}

/// The marker cargo writes at the root of its own checkout for
/// `cargo install --git` (#3380): untracked, and not a change to the source.
const CARGO_CHECKOUT_MARKER: &str = "?? .cargo-ok";

/// Whether `git status --porcelain` output reports a change. Cargo's
/// checkout marker alone is not one; anything else, tracked or not, is.
fn porcelain_is_dirty(porcelain: &str) -> bool {
    porcelain.lines().any(|l| l != CARGO_CHECKOUT_MARKER)
}

/// The tag and the kind for a build: see the module doc for the precedence.
fn build_tag(
    flagged: bool,
    git: Option<&GitState>,
    version: &str,
    env_commit: Option<&str>,
) -> (String, &'static str) {
    match git {
        // A release tarball has no `.git`, so the commit can only ride in the
        // env the formula stamps (#3373). A missing or non-SHA value stays a
        // bare `(release)` — still a release, just one the lab can't name.
        None if flagged => tarball_release(env_commit),
        None => (String::new(), ""),
        Some(g) if g.dirty => (format!("{}\u{2731}", g.short), "dev"),
        Some(g) if flagged || g.tags.iter().any(|t| *t == format!("v{version}")) => {
            (format!("release {}", g.short), "release")
        }
        Some(g) => (g.short.clone(), "dev"),
    }
}

/// No `.git`: `release <7>` from a full SHA, otherwise a bare `release`.
fn tarball_release(env_commit: Option<&str>) -> (String, &'static str) {
    match env_commit.and_then(accepted_commit) {
        Some(c) => (format!("release {}", &c[..7]), "release"),
        None => ("release".to_string(), "release"),
    }
}

/// A full git SHA (40 hex characters). Surrounding whitespace is ignored.
/// A short SHA, a tag name, or other text is refused so it is not baked.
/// This does not distinguish a commit from an annotated-tag object id —
/// both are 40 hex chars; the formula comment says to peel with `^{commit}`.
fn accepted_commit(raw: &str) -> Option<&str> {
    let c = raw.trim();
    (c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit())).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v5.6.0's commit (`git rev-parse v5.6.0^{commit}`), not the tag object.
    const SHA: &str = "69b8fc5032ecd872885780be0bf5b4f7f24844a8";

    #[test]
    fn no_git_flagged_with_env_commit_is_release_short() {
        let (tag, kind) = build_tag(true, None, "5.6.0", Some(SHA));
        assert_eq!(tag, "release 69b8fc5");
        assert_eq!(kind, "release");
    }

    #[test]
    fn no_git_flagged_without_env_commit_is_bare_release() {
        let (tag, kind) = build_tag(true, None, "5.6.0", None);
        assert_eq!((tag.as_str(), kind), ("release", "release"));
    }

    #[test]
    fn no_git_not_flagged_is_empty() {
        // An env commit does not make a non-release tarball a release.
        let (tag, kind) = build_tag(false, None, "5.6.0", Some(SHA));
        assert_eq!((tag.as_str(), kind), ("", ""));
        let (tag, kind) = build_tag(false, None, "5.6.0", None);
        assert_eq!((tag.as_str(), kind), ("", ""));
    }

    /// (#3380) `cargo install --git` builds from cargo's own checkout, which
    /// holds an untracked `.cargo-ok` marker cargo writes there. That marker is
    /// not a change to darkmux's source, so it must not make the build `dev`.
    #[test]
    fn cargo_checkout_marker_is_not_a_change() {
        assert!(!porcelain_is_dirty(""));
        assert!(!porcelain_is_dirty("?? .cargo-ok\n"));
        assert!(!porcelain_is_dirty("?? .cargo-ok"));
        // Any real change still marks the build dirty, marker or not.
        assert!(porcelain_is_dirty("?? .cargo-ok\n M src/lib.rs\n"));
        assert!(porcelain_is_dirty(" M src/lib.rs\n?? .cargo-ok\n"));
        assert!(porcelain_is_dirty("?? notes.txt\n"));
        assert!(porcelain_is_dirty("?? crates/x/.cargo-ok\n"));
    }

    /// (#3380) A clean cargo checkout of a tag has no tags of its own, so the
    /// release claim comes from `DARKMUX_RELEASE`; git still names the commit.
    #[test]
    fn flagged_clean_checkout_without_tags_is_release() {
        let git = GitState { commit: SHA.to_string(), short: SHA[..7].to_string(), dirty: false, tags: Vec::new() };
        assert_eq!(build_tag(true, Some(&git), "5.6.0", None), ("release 69b8fc5".to_string(), "release"));
        assert_eq!(build_tag(false, Some(&git), "5.6.0", None), ("69b8fc5".to_string(), "dev"));
    }

    #[test]
    fn env_commit_must_be_40_hex_chars() {
        for bad in [
            "",
            "   ",
            "69b8fc5",
            "not-a-commit",
            "69b8fc5032ecd872885780be0bf5b4f7f24844a8extra",
            "gggggggggggggggggggggggggggggggggggggggg",
            "fabfdc84603ed07115b8f971345c3a2645733ad",
        ] {
            let (tag, kind) = build_tag(true, None, "5.6.0", Some(bad));
            assert_eq!((tag.as_str(), kind), ("release", "release"), "{bad:?}");
        }
        let padded = "  69b8fc5032ecd872885780be0bf5b4f7f24844a8\n";
        let (tag, kind) = build_tag(true, None, "5.6.0", Some(padded));
        assert_eq!((tag.as_str(), kind), ("release 69b8fc5", "release"));
    }
}
