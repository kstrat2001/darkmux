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
use std::process::Command;

fn main() {
    let tag = if std::env::var_os("DARKMUX_RELEASE").is_some() {
        "release".to_string()
    } else {
        git_short_sha().unwrap_or_default()
    };
    println!("cargo:rustc-env=DARKMUX_BUILD_TAG={tag}");

    // Re-run when the release flag flips or HEAD moves, so the baked tag can't
    // go stale across rebuilds. Best-effort: missing `.git` paths (tarball
    // build) just make cargo always re-run this cheap script.
    println!("cargo:rerun-if-env-changed=DARKMUX_RELEASE");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

/// Short `HEAD` SHA, with a `✱` suffix when the working tree is dirty. `None`
/// when git is unavailable (no `.git`, no `git` binary, detached/empty repo).
fn git_short_sha() -> Option<String> {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())?;
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
