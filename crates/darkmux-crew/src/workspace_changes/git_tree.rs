//! A work tree as one git tree id, and what changed between two of them.
//!
//! (#3236) Moved here from `darkmux-serve`'s client work copy, which still
//! reads a run's changes with it, so a dispatch measures its workspace with
//! the same mechanism. The caller builds every git command ([`GitRun`]): it
//! holds the git directory and decides the config, so nothing here chooses
//! which repository git reads, or discovers one from a tree a run wrote.

use std::path::Path;

/// `git <args>` for one work tree, with `index` as its index when given;
/// its stdout when git succeeded, else why it did not.
pub type GitRun<'a> = &'a dyn Fn(Option<&Path>, &[&str]) -> Result<Vec<u8>, String>;

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
/// tree objects are added to the object store `git` writes to).
///
/// The index is FRESH and EMPTY, never a copy of the checkout's own: an
/// index entry for a populated submodule makes `git add` spawn git inside
/// it, in a `.git` a run can have written, where its config's commands run
/// (`darkmux_types::host_git`'s module doc: a caller "adds into a fresh,
/// empty index, which holds no gitlink to check"). A copied index also
/// carries `--assume-unchanged` and `--skip-worktree` bits that hide a
/// file's change from the add. So every file is hashed, every time.
pub fn snapshot_tree(work_tree: &Path, git: GitRun<'_>) -> Result<TreeSnapshot, String> {
    let scratch = tempfile::tempdir().map_err(|e| format!("a scratch index: {e}"))?;
    let index = scratch.path().join("index");
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
        tree: String::from_utf8_lossy(&tree).trim().to_string(),
        skipped,
    })
}

/// Whether the `git ls-tree -r -z <tree>` that `cmd` runs lists a submodule
/// (a gitlink, mode 160000) anywhere. A tree names a submodule by its commit
/// only, so work inside one that is not committed there is not in the tree.
///
/// (#3236 review) Streamed, never collected: the listing is about 60 bytes
/// an entry, so a capped capture made every repository past ~16k files (and,
/// with `--format=%(objectmode)`, past ~150k) unmeasurable, and `--format`
/// needs git 2.36. Plain `ls-tree -r -z` works on every git; this keeps only
/// a "saw 160000" flag, stops git as soon as it is set, and kills it at
/// `timeout`. `cmd`'s stdout is replaced with a pipe.
pub fn has_gitlinks(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Result<bool, String> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|_| "git ls-tree could not start".to_string())?;
    let scan = scan_in_background(child.stdout.take().expect("piped above"));
    let found = match await_scan(&scan, timeout) {
        Ok(found) => found,
        Err(e) => {
            stop(&mut child);
            return Err(e);
        }
    };
    if found == Some(true) {
        stop(&mut child);
        return Ok(true);
    }
    let status = child
        .wait()
        .map_err(|_| "git ls-tree could not be waited on".to_string())?;
    match (found, status.success()) {
        (Some(false), true) => Ok(false),
        _ => Err(format!(
            "git ls-tree failed (exit {})",
            status
                .code()
                .map_or("by signal".to_string(), |c| c.to_string())
        )),
    }
}

/// Read `out` to its end on a thread: `Some(true)` as soon as a gitlink is
/// seen, `Some(false)` at the end without one, `None` on a read error.
fn scan_in_background(
    mut out: std::process::ChildStdout,
) -> std::sync::mpsc::Receiver<Option<bool>> {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut scan = GitlinkScan::default();
        let mut buf = [0u8; 64 * 1024];
        let found = loop {
            match out.read(&mut buf) {
                Ok(0) => break Some(false),
                Ok(n) if scan.feed(&buf[..n]) => break Some(true),
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        let _ = tx.send(found);
    });
    rx
}

/// The scan's answer, or an error once `timeout` passed or darkmux was
/// interrupted.
fn await_scan(
    rx: &std::sync::mpsc::Receiver<Option<bool>>,
    timeout: std::time::Duration,
) -> Result<Option<bool>, String> {
    use std::sync::mpsc::RecvTimeoutError;
    let started = std::time::Instant::now();
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(found) => return Ok(found),
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
            Err(RecvTimeoutError::Timeout)
                if started.elapsed() >= timeout || darkmux_types::interrupt::is_set() =>
            {
                return Err(format!(
                    "git ls-tree did not finish within {} s",
                    timeout.as_secs()
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn stop(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The state of a scan over `git ls-tree -z` output, which may arrive in any
/// chunks: each record is `<mode> SP <type> SP <oid> TAB <path> NUL`.
#[derive(Debug)]
pub(crate) struct GitlinkScan {
    in_mode: bool,
    mode: Vec<u8>,
}

impl Default for GitlinkScan {
    fn default() -> Self {
        GitlinkScan {
            in_mode: true,
            mode: Vec::with_capacity(8),
        }
    }
}

impl GitlinkScan {
    /// Feed the next bytes; `true` once a record's mode is `160000`.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            if self.in_mode {
                if b == b' ' {
                    if self.mode == b"160000" {
                        return true;
                    }
                    self.in_mode = false;
                    self.mode.clear();
                } else if self.mode.len() < 8 {
                    self.mode.push(b);
                }
            } else if b == 0 {
                self.in_mode = true;
            }
        }
        false
    }
}

/// The untracked repositories inside the work tree: git lists each as one
/// `path/` entry and does not descend into it.
fn nested_repositories(work_tree: &Path, git: GitRun<'_>) -> Result<Vec<String>, String> {
    let out = git(None, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    Ok(String::from_utf8_lossy(&out)
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
    Ok(String::from_utf8_lossy(&out)
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
    let text = String::from_utf8_lossy(&out);
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut entries = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        entries.push((status.chars().next().unwrap_or('M'), path.to_string()));
    }
    Ok(entries)
}

#[cfg(test)]
mod await_scan_tests {
    use super::await_scan;
    use std::time::Duration;

    #[test]
    fn a_finished_scan_answers_and_a_dropped_one_reads_as_unknown() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(true)).unwrap();
        assert_eq!(await_scan(&rx, Duration::from_secs(5)), Ok(Some(true)));
        drop(tx);
        assert_eq!(await_scan(&rx, Duration::from_secs(5)), Ok(None));
    }

    #[test]
    fn a_scan_that_never_answers_ends_at_the_bound() {
        let (_tx, rx) = std::sync::mpsc::channel::<Option<bool>>();
        let started = std::time::Instant::now();
        let err = await_scan(&rx, Duration::from_millis(250)).unwrap_err();
        assert!(err.contains("did not finish within"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

