//! (#1129) Capture a build tag at build time so the observability viewer
//! header, `darkmux doctor`, and `--version` can show WHICH build is running.
//! The package version (`CARGO_PKG_VERSION`) doesn't change between releases,
//! so it alone can't tell an operator whether a daemon has their latest code
//! — or whether it's a packaged release at all.
//!
//! Precedence for the tag (`darkmux_types::build_version()` reads it):
//!   1. **release** — `DARKMUX_RELEASE` is set. The Homebrew stable formula
//!      stamps this (it builds from a release tarball with no `.git`, so the
//!      git fallback below would otherwise leave it indistinguishable from a
//!      bare source build). Shows `<version> (release)`.
//!   2. **git SHA** — a git checkout (dev build, or `brew install --HEAD`):
//!      short SHA with a `✱` suffix when the tree is dirty. Shows
//!      `<version> (a1b2c3d✱)`.
//!   3. **empty** — no release flag, no git: source tarball build. Shows the
//!      version alone.
//!
//! Dep-free by design (CLAUDE.md: "don't add dependencies casually") — it
//! shells to `git`. Lives in the foundation crate so serve + doctor + the
//! binary all read one source of truth.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let tag = if std::env::var_os("DARKMUX_RELEASE").is_some() {
        "release".to_string()
    } else {
        git_short_sha().unwrap_or_default()
    };
    println!("cargo:rustc-env=DARKMUX_BUILD_TAG={tag}");

    // Re-run when the release flag flips or HEAD moves, so the baked tag can't
    // go stale across rebuilds.
    println!("cargo:rerun-if-env-changed=DARKMUX_RELEASE");
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
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
    let mut paths = vec![git_dir.join("HEAD"), git_dir.join("index")];
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

/// Short `HEAD` SHA, with a `✱` suffix when the working tree is dirty. `None`
/// when git is unavailable (no `.git`, no `git` binary, detached/empty repo).
fn git_short_sha() -> Option<String> {
    let sha = git_stdout(&["rev-parse", "--short", "HEAD"])?;
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    Some(if dirty {
        format!("{sha}\u{2731}")
    } else {
        sha
    })
}
