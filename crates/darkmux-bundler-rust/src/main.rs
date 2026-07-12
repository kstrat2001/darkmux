//! `darkmux-bundler-rust` — the reference implementation of darkmux's
//! external `--bundler <cmd> --worktree <dir> --diff <file>` plugin
//! contract (darkmux#1319): a Rust-source-aware bundler, standing in for
//! the built-in TypeScript-only bundler when the reviewed diff touches
//! `.rs` files.
//!
//! This is the dogfood: darkmux's own self-review (a public repo, a
//! self-hosted runner, no PR checkout — `.github/workflows/darkmux-review.yml`
//! dispatches `--github "$REPO" --head-sha "$HEAD_SHA"`, never
//! `--worktree`) can only review its own Rust PRs through this plugin.
//! If the plugin can't run through the SAME public `--bundler` seam a
//! third-party plugin author would use, the extension point isn't real.
//!
//! Usage: `darkmux-bundler-rust --diff <path> [--worktree <dir>]`
//! Emits the frozen `BundleSet` JSON (`{"bundles": [...]}`) on stdout,
//! or a clear message on stderr + non-zero exit for a genuine failure
//! (no `.rs` files touched, an unreadable diff file, …) — the caller's
//! `external_bundles` treats both a non-zero exit and an empty bundle
//! array as loud errors, never a silent pass.

mod bundler;
mod facts;
mod scan;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

/// `<cmd> --worktree <dir> --diff <file>` — the frozen `--bundler`
/// contract's exact argv shape (`worktree` omitted entirely by the
/// caller when there's no checkout to hand over).
#[derive(Parser)]
#[command(name = "darkmux-bundler-rust", version, about)]
struct Args {
    /// Local checkout to read full file contents from, for more reliable
    /// function-boundary resolution. Absent when the caller has no
    /// checkout (darkmux's own self-review workflow never passes this) —
    /// the diff-only path still produces bundles, just from the diff's
    /// own context lines rather than the whole file.
    #[arg(long)]
    worktree: Option<PathBuf>,

    /// The diff file to bundle (unified format, as produced by `git diff`
    /// / `gh pr diff`).
    #[arg(long)]
    diff: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let diff_text = std::fs::read_to_string(&args.diff)
        .with_context(|| format!("reading diff file {}", args.diff.display()))?;

    let set = bundler::build_bundles(args.worktree.as_deref(), &diff_text)?;

    if set.bundles.is_empty() {
        eprintln!(
            "darkmux-bundler-rust: no .rs files with reviewable hunks in this diff — nothing \
             to bundle. (If this diff touches non-Rust files, the built-in bundler or another \
             --bundler plugin may be the right tool for it; this plugin only handles .rs.)"
        );
        std::process::exit(1);
    }

    println!("{}", serde_json::to_string(&set)?);
    Ok(())
}
