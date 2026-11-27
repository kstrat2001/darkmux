//! A work tree as one git tree id, and what changed between two of them.
//!
//! (#3236) Moved here from `darkmux-serve`'s client work copy, which still
//! reads a run's changes with it, so a dispatch measures its workspace with
//! the same mechanism. The caller builds every git command ([`GitRun`]): it
//! holds the git directory and decides the config, so nothing here chooses
//! which repository git reads, or discovers one from a tree a run wrote.

use std::path::Path;
use std::process::Output;

/// `git <args>` for one work tree, with `index` as its index when given;
/// the output when git succeeded, else why it did not.
pub type GitRun<'a> = &'a dyn Fn(Option<&Path>, &[&str]) -> Result<Output, String>;

/// A work tree as one tree, and the repositories inside it the tree leaves
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSnapshot {
    pub tree: String,
    pub skipped: Vec<String>,
}

/// The whole work tree as a git tree id: tracked AND untracked files (not
/// ignored ones), staged or not, written through a throwaway index so the
/// work tree's own index, branch and files are never touched (only blob and
/// tree objects are added to the object store `git` writes to). `seed_index`,
/// when given and present, is copied in first with its mtime kept, so a file
/// whose stat data still matches its entry is not hashed again (git's own
/// racy-timestamp rule still re-reads one written in the index's second).
pub fn snapshot_tree(
    work_tree: &Path,
    git: GitRun<'_>,
    seed_index: Option<&Path>,
) -> Result<TreeSnapshot, String> {
    let scratch = tempfile::tempdir().map_err(|e| format!("a scratch index: {e}"))?;
    let index = scratch.path().join("index");
    if let Some(seed) = seed_index.filter(|s| s.is_file()) {
        copy_keeping_mtime(seed, &index)
            .map_err(|e| format!("copying the index {}: {e}", seed.display()))?;
    }
    let skipped = nested_repositories(work_tree, git)?;
    // A repository inside the work tree (an empty `git init sub` makes
    // `git add` fail outright; one with commits would be added as a link)
    // is left out, by name, literally.
    let excludes: Vec<String> = skipped
        .iter()
        .map(|p| format!(":(exclude,literal){p}"))
        .collect();
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(excludes.iter().map(String::as_str));
    git(Some(&index), &add)?;
    let tree = git(Some(&index), &["write-tree"])?;
    Ok(TreeSnapshot {
        tree: String::from_utf8_lossy(&tree.stdout).trim().to_string(),
        skipped,
    })
}

fn copy_keeping_mtime(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options()
        .write(true)
        .open(to)?
        .set_modified(modified)
}

/// The untracked repositories inside the work tree: git lists each as one
/// `path/` entry and does not descend into it.
fn nested_repositories(work_tree: &Path, git: GitRun<'_>) -> Result<Vec<String>, String> {
    let out = git(None, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.strip_suffix('/'))
        .filter(|p| std::fs::symlink_metadata(work_tree.join(p).join(".git")).is_ok())
        .map(str::to_string)
        .collect())
}

/// One file that differs between two trees: lines added and removed, `None`
/// for a binary file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFileChange {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// The files that differ between trees `before` and `after`, with their line
/// counts (`git diff --numstat`, no renames).
pub fn numstat(git: GitRun<'_>, before: &str, after: &str) -> Result<Vec<TreeFileChange>, String> {
    let out = git(
        None,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "-z",
            before,
            after,
        ],
    )?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (added, removed, path) = (parts.next()?, parts.next()?, parts.next()?);
            let n = |s: &str| s.parse::<u64>().ok();
            Some(TreeFileChange {
                path: path.to_string(),
                added: n(added),
                removed: n(removed),
            })
        })
        .collect())
}

/// The files that differ between trees `before` and `after`, each with git's
/// status letter (`A`, `M`, `D`, `T`), no renames.
pub fn name_status(
    git: GitRun<'_>,
    before: &str,
    after: &str,
) -> Result<Vec<(char, String)>, String> {
    let out = git(
        None,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--name-status",
            "-z",
            before,
            after,
        ],
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut entries = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        entries.push((status.chars().next().unwrap_or('M'), path.to_string()));
    }
    Ok(entries)
}
