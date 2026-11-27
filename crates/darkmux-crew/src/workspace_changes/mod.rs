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
use std::time::{Duration, Instant};

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
    /// The bound on each read's git calls together.
    timeout: Duration,
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
    /// git cannot read. Each read's git calls share one bound,
    /// `runtime.step_command_timeout_seconds` (the bound on every command
    /// darkmux runs for a step), so a hung call never holds the dispatch's
    /// close: past it the changes are not measured.
    pub fn take(root: &Path, git_dir: Option<&Path>) -> Self {
        Self::take_bounded(root, git_dir, crate::bounded_command::configured_timeout())
    }

    /// [`WorkspaceBaseline::take`] with the bound given.
    pub fn take_bounded(root: &Path, git_dir: Option<&Path>, timeout: Duration) -> Self {
        let before = match git_dir.and_then(|g| git_before(root, g, timeout)) {
            Some(git) => Before::Git(git),
            None => match snapshot::compute_snapshot(root) {
                Ok(s) => Before::Snapshot(s),
                Err(e) => Before::Unread {
                    method: WorkspaceMethod::Snapshot,
                    why: format!(
                        "reading the workspace before the dispatch failed: {}",
                        walk_reason(&e)
                    ),
                },
            },
        };
        WorkspaceBaseline {
            root: root.to_path_buf(),
            before,
            timeout,
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
            Before::Git(git) => measure_git(&self.root, git, self.timeout),
            Before::Snapshot(before) => measure_snapshot(&self.root, before),
            Before::Unread { method, why } => unmeasured(*method, why.clone()),
        }
    }

    /// The record of a dispatch that ended before its workspace was read
    /// again: the method it would have used, and `why`.
    pub fn not_read(&self, why: &str) -> WorkspaceChanges {
        let method = match &self.before {
            Before::Git(_) => WorkspaceMethod::Git,
            Before::Snapshot(_) => WorkspaceMethod::Snapshot,
            Before::Unread { method, .. } => *method,
        };
        unmeasured(method, why.to_string()).summary
    }
}

/// A fixed reason a walk failed, naming no path: `not_measured` rides the
/// flow stream, which carries no path.
fn walk_reason(e: &snapshot::WalkError) -> String {
    match e {
        snapshot::WalkError::Overflow { .. } => {
            format!(
                "the workspace holds more than {} files",
                snapshot::MAX_FILE_COUNT
            )
        }
        snapshot::WalkError::Io(_) => "the workspace could not be read".to_string(),
    }
}

/// When the calls started now must end; `None` for no bound.
fn deadline_after(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout)
}

/// The git calls of one read: `git` in `root` against the held `git_dir`,
/// new objects into `objects` with the repository's own store as an
/// alternate, every call (the filter-driver read each one needs included)
/// bounded by what is left before `deadline`. A failure says which
/// subcommand failed and how, never git's own words, which name paths.
struct GitCalls<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    objects: &'a Path,
    deadline: Option<Instant>,
}

impl GitCalls<'_> {
    fn host(&self) -> darkmux_types::host_git::HostGit {
        darkmux_types::host_git::HostGit::run_written(self.git_dir, self.root)
    }

    /// The time left for `sub`; an error once the bound has passed.
    fn left(&self, sub: &str) -> Result<Duration, String> {
        let left = self
            .deadline
            .map_or(crate::bounded_command::UNBOUNDED, |d| {
                d.saturating_duration_since(Instant::now())
            });
        if left.is_zero() {
            return Err(format!(
                "git {sub} was not started: the read's time bound had passed"
            ));
        }
        Ok(left)
    }

    /// The filter drivers the repository's config defines, read now and
    /// bounded. (#3236 review) That config can `include.path` a file in the
    /// work tree, which a run can replace with a FIFO; `HostGit::command`'s
    /// own read of it is unbounded. Read per call, not once, so a driver the
    /// run defined through such an include is still passed empty.
    fn drivers(&self) -> Result<Vec<String>, String> {
        use crate::bounded_command::{run_bounded, Bounded};
        match run_bounded(self.host().filter_drivers_command(), self.left("config")?) {
            Bounded::Finished {
                success: true,
                stdout,
                ..
            } => Ok(darkmux_types::host_git::filter_drivers_in(&stdout)),
            Bounded::Finished { .. } => Ok(Vec::new()),
            other => Err(bounded_failure("config", other)),
        }
    }

    /// `git <args>`, hardened, with this call's drivers passed empty.
    fn command(
        &self,
        index: Option<&Path>,
        args: &[&str],
    ) -> Result<std::process::Command, String> {
        let mut cmd = self.host().command_with_drivers(args, &self.drivers()?);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OBJECT_DIRECTORY", self.objects)
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                self.git_dir.join("objects"),
            );
        if let Some(i) = index {
            cmd.env("GIT_INDEX_FILE", i);
        }
        Ok(cmd)
    }

    /// `git <args>` run to completion under the bound: its stdout.
    fn run(&self, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, String> {
        use crate::bounded_command::{run_bounded_with_cuts, Bounded};
        let sub = args.first().copied().unwrap_or("(no subcommand)");
        let cmd = self.command(index, args)?;
        match run_bounded_with_cuts(cmd, self.left(sub)?) {
            (
                Bounded::Finished {
                    success: true,
                    stdout,
                    ..
                },
                cut,
            ) if cut.stdout == 0 => Ok(stdout),
            (Bounded::Finished { success: true, .. }, _) => {
                Err(format!("git {sub} wrote more output than darkmux keeps"))
            }
            (other, _) => Err(bounded_failure(sub, other)),
        }
    }

    /// Whether `tree` holds a submodule, streamed under the bound.
    fn has_gitlinks(&self, tree: &str) -> Result<bool, String> {
        git_tree::has_gitlinks(
            self.command(None, &["ls-tree", "-r", "-z", tree])?,
            self.left("ls-tree")?,
        )
    }
}

/// A fixed reason for a bounded git call that did not succeed.
fn bounded_failure(sub: &str, how: crate::bounded_command::Bounded) -> String {
    use crate::bounded_command::Bounded;
    match how {
        Bounded::Finished { code, .. } => {
            format!(
                "git {sub} failed (exit {})",
                code.map_or("by signal".to_string(), |c| c.to_string())
            )
        }
        Bounded::TimedOut { seconds } => format!("git {sub} did not finish within {seconds} s"),
        Bounded::Interrupted => format!("git {sub} was stopped by a signal"),
        Bounded::SpawnFailed(_) => format!("git {sub} could not start"),
    }
}

fn git_before(root: &Path, git_dir: &Path, timeout: Duration) -> Option<GitBefore> {
    let objects = tempfile::tempdir().ok()?;
    let tree = {
        let calls = GitCalls {
            root,
            git_dir,
            objects: objects.path(),
            deadline: deadline_after(timeout),
        };
        // A repository whose own config defines a filter driver is measured
        // by snapshot: darkmux runs none, so git would hash it unfiltered.
        if !calls.drivers().ok()?.is_empty() {
            return None;
        }
        let git = |index: Option<&Path>, args: &[&str]| calls.run(index, args);
        git_tree::snapshot_tree(root, &git).ok()?
    };
    Some(GitBefore {
        git_dir: git_dir.to_path_buf(),
        objects,
        tree,
    })
}

fn measure_git(root: &Path, before: &GitBefore, timeout: Duration) -> EnvelopeWorkspaceChanges {
    let calls = GitCalls {
        root,
        git_dir: &before.git_dir,
        objects: before.objects.path(),
        deadline: deadline_after(timeout),
    };
    let git = |index: Option<&Path>, args: &[&str]| calls.run(index, args);
    let read = || -> Result<(Vec<WorkspaceFileChange>, Vec<String>), String> {
        let after = git_tree::snapshot_tree(root, &git)?;
        Ok((
            git_files(&git, &before.tree.tree, &after.tree)?,
            git_excluded(&calls, &before.tree, &after)?,
        ))
    };
    match read() {
        Ok((files, excluded)) => described(WorkspaceMethod::Git, files, excluded),
        Err(e) => unmeasured(
            WorkspaceMethod::Git,
            format!("reading the workspace when the dispatch ended failed: {e}"),
        ),
    }
}

/// The files that differ between two trees, each with its line counts.
fn git_files(
    git: git_tree::GitRun<'_>,
    before: &str,
    after: &str,
) -> Result<Vec<WorkspaceFileChange>, String> {
    let lines: std::collections::BTreeMap<String, git_tree::TreeFileChange> =
        git_tree::numstat(git, before, after)?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();
    Ok(git_tree::name_status(git, before, after)?
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
        .collect())
}

/// What the git path did not look at: ignored files always, repositories
/// the run made inside the checkout, and work inside submodules.
fn git_excluded(
    calls: &GitCalls<'_>,
    before: &git_tree::TreeSnapshot,
    after: &git_tree::TreeSnapshot,
) -> Result<Vec<String>, String> {
    let mut excluded = vec![WorkspaceChanges::GIT_IGNORED.to_string()];
    if !before.skipped.is_empty() || !after.skipped.is_empty() {
        excluded.push(WorkspaceChanges::NESTED_REPOSITORIES.to_string());
    }
    if calls.has_gitlinks(&before.tree)? || calls.has_gitlinks(&after.tree)? {
        excluded.push(WorkspaceChanges::SUBMODULES.to_string());
    }
    Ok(excluded)
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
                format!(
                    "reading the workspace when the dispatch ended failed: {}",
                    walk_reason(&e)
                ),
            );
        }
    };
    let delta = snapshot::diff_snapshots(before, &after);
    let added = delta.added.iter().map(|p| {
        let lines = added_lines(root, p, &after.files[p]);
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
    let mut excluded: Vec<String> = before.excluded.union(&after.excluded).cloned().collect();
    if before.unhashed || after.unhashed {
        excluded.push(WorkspaceChanges::LARGE_FILES.to_string());
    }
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
/// unterminated last line is a line; a symlink is one line, its target).
/// `None` for a binary file (a NUL in its first 8000 bytes, git's rule), one
/// over the hashing cap, or one that cannot be opened as a regular file
/// without following a link. Read in chunks, never whole.
fn added_lines(root: &Path, rel: &Path, entry: &snapshot::FileSnapshot) -> Option<u64> {
    use std::io::Read;
    if entry.kind == snapshot::EntryKind::Symlink {
        return Some(1);
    }
    if entry.size > snapshot::MAX_FILE_SIZE_BYTES {
        return None;
    }
    let mut file = crate::contained_file::open_contained(root, rel).ok()?;
    let (mut newlines, mut seen, mut last) = (0u64, 0u64, None);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        let head = 8000u64.saturating_sub(seen).min(n as u64) as usize;
        if chunk[..head].contains(&0) {
            return None;
        }
        newlines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        seen += n as u64;
        last = chunk.last().copied();
        if seen > snapshot::MAX_FILE_SIZE_BYTES {
            return None;
        }
    }
    Some(newlines + u64::from(last.is_some_and(|b| b != b'\n')))
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

/// A step's output as a downstream step's model is sent it: a dispatch
/// envelope's `workspace_changes` keeps its counts and loses its file list,
/// because that message is recorded on the flow stream (the next
/// `dispatch.start`'s prompt), which carries no path. Any other text, and an
/// envelope with no list, is returned as it was, byte for byte.
pub fn without_file_list(output: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let Ok(serde_json::Value::Object(mut envelope)) =
        serde_json::from_str::<serde_json::Value>(output.trim())
    else {
        return Cow::Borrowed(output);
    };
    let Some(serde_json::Value::Object(changes)) = envelope.get_mut("workspace_changes") else {
        return Cow::Borrowed(output);
    };
    let listed = changes.remove("files");
    let omitted = changes.remove("files_omitted");
    if listed.is_none() && omitted.is_none() {
        return Cow::Borrowed(output);
    }
    serde_json::to_string(&envelope).map_or(Cow::Borrowed(output), Cow::Owned)
}
