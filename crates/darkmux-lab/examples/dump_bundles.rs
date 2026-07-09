// Local parity-check helper (NOT for commit): dump the built-in bundler's
// BundleSet as JSON for structural diff against bundler.py output.
// Usage: cargo run -p darkmux-lab --example dump_bundles -- <worktree> <diff-file>
use darkmux_lab::lab::bundle::{build_bundles, source::FileSource};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let worktree = args.next().expect("usage: dump_bundles <worktree> <diff>");
    let diff_path = args.next().expect("usage: dump_bundles <worktree> <diff>");
    let diff = std::fs::read_to_string(&diff_path)?;
    let set = build_bundles(&FileSource::Worktree(worktree.into()), &diff)?;
    println!("{}", serde_json::to_string_pretty(&set)?);
    Ok(())
}
