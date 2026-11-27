//! (#3236) What a dispatch's workspace actually changed.
//!
//! darkmux describes; it never reads the model's claim and never changes a
//! dispatch's result because of what it measures here. The host reads the
//! workspace right before the sandbox starts ([`WorkspaceBaseline::take`])
//! and again when the dispatch ends ([`WorkspaceBaseline::measure`]), which
//! is the tree as it was then: a process that outlives the dispatch can
//! still write afterward.
//!
//! Two mechanisms, both moved here rather than written again:
//!
//! - **git** ([`git_tree`], from the client work copy): when the workspace is
//!   a checkout whose own `.git` directory the run could not write, two
//!   trees of the work tree through a throwaway index and a scratch object
//!   store, so the operator's repository is only read. Tracked and untracked
//!   files count; ignored ones do not (`excluded` says so). Lines come from
//!   `git diff --numstat`; a binary file has none.
//! - **snapshot** ([`snapshot`], from the lab's workspace delta): every other
//!   workspace (a fresh tempdir, a lab clone, a client work copy, a coder
//!   worktree, a `.git` the run could write). Each file's size and content
//!   hash, before and after, skipping the directory names in
//!   [`artifact_dirs`] and the build-output ones the walk names (`excluded`
//!   lists those it met). An added file's lines are counted from its
//!   content; a modified or removed file's are not measured, because its
//!   old content was never kept.
//!
//! `null` is never `0`: `files_changed` is `0` only when the workspace was
//! read and nothing changed, and a line total is `null` unless every changed
//! file's lines were measured.

pub mod artifact_dirs;
pub mod git_tree;
pub mod snapshot;

#[cfg(test)]
mod tests;

use darkmux_flow::payload::{WorkspaceChanges, WorkspaceMethod};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The most files the envelope lists by name; the counts and totals cover
/// every file. Keeps one envelope (and a downstream step's message, which
/// carries it) bounded however much a run wrote.
pub const MAX_LISTED_FILES: usize = 100;

/// What the envelope says a dispatch's workspace changed: the flow stream's
/// counts, plus the files by name.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct EnvelopeWorkspaceChanges {
    #[serde(flatten)]
    pub summary: WorkspaceChanges,
    /// The changed files, sorted by path, at most [`MAX_LISTED_FILES`]. Paths
    /// are relative to the workspace, `/`-separated, and named by the run:
    /// untrusted text.
    pub files: Vec<WorkspaceFileChange>,
    /// Changed files past the listed ones.
    pub files_omitted: u64,
}

/// One file the dispatch changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WorkspaceFileChange {
    pub path: String,
    pub change: WorkspaceChangeKind,
    /// `null` when not measured: a binary file, or a modified or removed file
    /// on the snapshot path.
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
}

/// How one file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChangeKind {
    Added,
    Modified,
    Removed,
}

/// The workspace before the dispatch ran: what its changes are read against.
pub struct WorkspaceBaseline {
    root: PathBuf,
    before: Before,
}

enum Before {
    Git(GitBefore),
    Snapshot(snapshot::WorkspaceSnapshot),
    Unread {
        method: WorkspaceMethod,
        why: String,
    },
}

/// The git path's state between the two reads: the held git directory, the
/// scratch object store new blobs go to (the repository's own is an
/// alternate, only read), and the tree before.
struct GitBefore {
    git_dir: PathBuf,
    objects: tempfile::TempDir,
    tree: git_tree::TreeSnapshot,
}

impl WorkspaceBaseline {
    /// Read `root` now. `git_dir` is the checkout's own git directory when the
    /// run cannot write it (mounted read-only); `None` measures by snapshot,
    /// and so does a repository whose own config defines a filter driver
    /// (darkmux runs none, so git would hash those files unfiltered) or one
    /// git cannot read.
    pub fn take(root: &Path, git_dir: Option<&Path>) -> Self {
        let before = match git_dir.and_then(|g| git_before(root, g)) {
            Some(git) => Before::Git(git),
            None => match snapshot::compute_snapshot(root) {
                Ok(s) => Before::Snapshot(s),
                Err(e) => Before::Unread {
                    method: WorkspaceMethod::Snapshot,
                    why: format!("reading the workspace before the dispatch failed: {e}"),
                },
            },
        };
        WorkspaceBaseline {
            root: root.to_path_buf(),
            before,
        }
    }

    /// The git path's scratch object store, for a test to look in.
    #[cfg(test)]
    fn scratch_objects(&self) -> Option<&Path> {
        match &self.before {
            Before::Git(git) => Some(git.objects.path()),
            _ => None,
        }
    }

    /// What changed since [`WorkspaceBaseline::take`], read now.
    pub fn measure(&self) -> EnvelopeWorkspaceChanges {
        match &self.before {
            Before::Git(git) => measure_git(&self.root, git),
            Before::Snapshot(before) => measure_snapshot(&self.root, before),
            Before::Unread { method, why } => unmeasured(*method, why.clone()),
        }
    }
}

/// `git <args>` in `root` against the held `git_dir`, new objects into
/// `objects` with the repository's own store as an alternate.
fn git_runner<'a>(
    root: &'a Path,
    git_dir: &'a Path,
    objects: &'a Path,
) -> impl Fn(Option<&Path>, &[&str]) -> Result<std::process::Output, String> + 'a {
    move |index, args| {
        let mut cmd = darkmux_types::host_git::HostGit::run_written(git_dir, root).command(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OBJECT_DIRECTORY", objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", git_dir.join("objects"))
            .stdin(std::process::Stdio::null());
        if let Some(i) = index {
            cmd.env("GIT_INDEX_FILE", i);
        }
        let out = cmd.output().map_err(|e| format!("running git: {e}"))?;
        if out.status.success() {
            return Ok(out);
        }
        Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn git_before(root: &Path, git_dir: &Path) -> Option<GitBefore> {
    if !darkmux_types::host_git::HostGit::run_written(git_dir, root)
        .repo_filter_drivers()
        .is_empty()
    {
        return None;
    }
    let objects = tempfile::tempdir().ok()?;
    let tree = {
        let git = git_runner(root, git_dir, objects.path());
        git_tree::snapshot_tree(root, &git, Some(&git_dir.join("index"))).ok()?
    };
    Some(GitBefore {
        git_dir: git_dir.to_path_buf(),
        objects,
        tree,
    })
}

fn measure_git(root: &Path, before: &GitBefore) -> EnvelopeWorkspaceChanges {
    let git = git_runner(root, &before.git_dir, before.objects.path());
    let read = || -> Result<(Vec<WorkspaceFileChange>, Vec<String>), String> {
        let after = git_tree::snapshot_tree(root, &git, Some(&before.git_dir.join("index")))?;
        let lines: std::collections::BTreeMap<String, git_tree::TreeFileChange> =
            git_tree::numstat(&git, &before.tree.tree, &after.tree)?
                .into_iter()
                .map(|f| (f.path.clone(), f))
                .collect();
        let files = git_tree::name_status(&git, &before.tree.tree, &after.tree)?
            .into_iter()
            .map(|(status, path)| {
                let counted = lines.get(&path);
                WorkspaceFileChange {
                    change: git_change_kind(status),
                    lines_added: counted.and_then(|c| c.added),
                    lines_removed: counted.and_then(|c| c.removed),
                    path,
                }
            })
            .collect();
        let mut excluded = vec![WorkspaceChanges::GIT_IGNORED.to_string()];
        if !before.tree.skipped.is_empty() || !after.skipped.is_empty() {
            excluded.push(WorkspaceChanges::NESTED_REPOSITORIES.to_string());
        }
        Ok((files, excluded))
    };
    match read() {
        Ok((files, excluded)) => described(WorkspaceMethod::Git, files, excluded),
        Err(e) => unmeasured(
            WorkspaceMethod::Git,
            format!("reading the workspace when the dispatch ended failed: {e}"),
        ),
    }
}

fn git_change_kind(status: char) -> WorkspaceChangeKind {
    match status {
        'A' => WorkspaceChangeKind::Added,
        'D' => WorkspaceChangeKind::Removed,
        _ => WorkspaceChangeKind::Modified,
    }
}

fn measure_snapshot(root: &Path, before: &snapshot::WorkspaceSnapshot) -> EnvelopeWorkspaceChanges {
    let after = match snapshot::compute_snapshot(root) {
        Ok(a) => a,
        Err(e) => {
            return unmeasured(
                WorkspaceMethod::Snapshot,
                format!("reading the workspace when the dispatch ended failed: {e}"),
            );
        }
    };
    let delta = snapshot::diff_snapshots(before, &after);
    let added = delta.added.iter().map(|p| {
        let lines = added_lines(&root.join(p));
        file_change(p, WorkspaceChangeKind::Added, lines, lines.map(|_| 0))
    });
    let modified = delta
        .modified
        .iter()
        .map(|p| file_change(p, WorkspaceChangeKind::Modified, None, None));
    let removed = delta
        .removed
        .iter()
        .map(|p| file_change(p, WorkspaceChangeKind::Removed, None, None));
    let mut files: Vec<WorkspaceFileChange> = added.chain(modified).chain(removed).collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let excluded = before.excluded.union(&after.excluded).cloned().collect();
    described(WorkspaceMethod::Snapshot, files, excluded)
}

fn file_change(
    path: &Path,
    change: WorkspaceChangeKind,
    lines_added: Option<u64>,
    lines_removed: Option<u64>,
) -> WorkspaceFileChange {
    let path = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    WorkspaceFileChange {
        path,
        change,
        lines_added,
        lines_removed,
    }
}

/// The lines of an added file, counted as git's numstat counts them (an
/// unterminated last line is a line); `None` for a binary file (a NUL in its
/// first 8000 bytes, git's rule) or one that cannot be read.
fn added_lines(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    Some(newlines + u64::from(bytes.last().is_some_and(|&b| b != b'\n')))
}

/// The measured description: every file counted, the first
/// [`MAX_LISTED_FILES`] listed, and each line total only when every changed
/// file's lines were measured.
fn described(
    method: WorkspaceMethod,
    mut files: Vec<WorkspaceFileChange>,
    excluded: Vec<String>,
) -> EnvelopeWorkspaceChanges {
    let total =
        |pick: fn(&WorkspaceFileChange) -> Option<u64>| files.iter().map(pick).sum::<Option<u64>>();
    let summary = WorkspaceChanges {
        method,
        files_changed: Some(files.len() as u64),
        not_measured: None,
        lines_added: total(|f| f.lines_added),
        lines_removed: total(|f| f.lines_removed),
        excluded,
    };
    let files_omitted = files.len().saturating_sub(MAX_LISTED_FILES) as u64;
    files.truncate(MAX_LISTED_FILES);
    EnvelopeWorkspaceChanges {
        summary,
        files,
        files_omitted,
    }
}

fn unmeasured(method: WorkspaceMethod, why: String) -> EnvelopeWorkspaceChanges {
    EnvelopeWorkspaceChanges {
        summary: WorkspaceChanges {
            method,
            not_measured: Some(why),
            ..WorkspaceChanges::default()
        },
        files: Vec::new(),
        files_omitted: 0,
    }
}
