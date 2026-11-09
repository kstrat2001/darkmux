//! Host git for a paired app's workspaces, kept away from anything a run can
//! write.
//!
//! A workspace is mounted read-write into the run's container, so whatever
//! the run leaves in it is the run's: a `.git` directory or file, its
//! config (`core.fsmonitor`, a clean or smudge filter, `diff.textconv`,
//! `gpg.program` with `commit.gpgsign`, `core.sshCommand`,
//! `credential.helper`, `include.path`), its hooks. Host git that
//! discovered its repository from the work tree would obey all of it, on
//! the host, after the run ends.
//!
//! So host git never discovers a repository in a workspace. Every call
//! names the repository darkmux holds for it ([`WorkCopy::git_dir`]),
//! outside the workspace and recorded beside it when darkmux made it
//! (`.<name>.gitdir` under the worktrees base): for a clone, the
//! `--separate-git-dir` it was cloned with; for a worktree of the user's
//! checkout, that worktree's admin directory under the checkout's own
//! `.git/worktrees/`. The workspace's own `.git` is never read, so a run that
//! rewrites or replaces it changes nothing on the host. On top of that every
//! call turns off the config that runs a program ([`HOST_GIT_CONFIG`]), and
//! reads no system config (`GIT_CONFIG_NOSYSTEM`); diffs pass
//! `--no-textconv --no-ext-diff`.

use std::path::{Path, PathBuf};

/// Config every host git call in a workspace sets, so even config darkmux
/// did not expect to find runs nothing.
pub const HOST_GIT_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "commit.gpgsign=false",
    "tag.gpgsign=false",
    "core.sshCommand=ssh",
    "credential.helper=",
    "diff.external=",
    "core.untrackedCache=false",
];

/// Environment a host git call must not inherit: it would point git at
/// another repository or index, or run another program. (`client serve`'s
/// own environment is the operator's, never a run's; the operator's own
/// config, `GIT_CONFIG_*` included, still applies.)
const GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_EXTERNAL_DIFF",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "GIT_EXEC_PATH",
];

/// A workspace as host git sees it: its work tree, and the repository
/// darkmux holds for it outside that tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCopy {
    pub work_tree: PathBuf,
    pub git_dir: PathBuf,
}

/// Where darkmux records workspace `name`'s repository: beside it, never
/// inside it.
fn git_dir_record(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.gitdir"))
}

/// The repository a clone named `name` is cloned into (`--separate-git-dir`).
pub fn clone_git_dir(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.git"))
}

/// Record workspace `name`'s repository. Replaces any earlier record, never
/// through a link.
pub fn record_git_dir(base: &Path, name: &str, git_dir: &Path) -> Result<(), String> {
    use std::io::Write;
    let path = git_dir_record(base, name);
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{}", git_dir.display()))
        .map_err(|e| format!("recording {}'s repository: {e}", path.display()))
}

/// Forget workspace `name`'s repository record.
pub fn forget_git_dir(base: &Path, name: &str) {
    let _ = std::fs::remove_file(git_dir_record(base, name));
}

/// Workspace `name` (at `work_tree`) as host git may touch it: its recorded
/// repository, which must be a real directory (not a link) outside the work
/// tree. `None` when darkmux made no record (a folder it did not make, or a
/// workspace from before records), so no git runs there at all.
pub fn work_copy_of(base: &Path, name: &str, work_tree: &Path) -> Option<WorkCopy> {
    let record = git_dir_record(base, name);
    if !std::fs::symlink_metadata(&record).ok()?.is_file() {
        return None;
    }
    let git_dir = PathBuf::from(
        std::fs::read_to_string(&record)
            .ok()?
            .trim_end_matches('\n'),
    );
    let meta = std::fs::symlink_metadata(&git_dir).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let (canon_git, canon_tree) = (git_dir.canonicalize().ok()?, work_tree.canonicalize().ok()?);
    if canon_git.starts_with(&canon_tree) {
        return None;
    }
    Some(WorkCopy {
        work_tree: canon_tree,
        git_dir: canon_git,
    })
}

/// A worktree's own admin directory, read from the `.git` file `git
/// worktree add` just wrote in `work_tree` (before any run has been there),
/// and only when it lies under the checkout's `<common>/worktrees/`.
pub fn worktree_admin_dir(work_tree: &Path, common: &Path) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(work_tree.join(".git"))
        .map_err(|e| format!("reading the new worktree's .git: {e}"))?;
    let pointer = text
        .trim()
        .strip_prefix("gitdir:")
        .ok_or("the new worktree's .git is not a gitdir pointer")?
        .trim();
    let dir = PathBuf::from(pointer)
        .canonicalize()
        .map_err(|e| format!("resolving the new worktree's repository: {e}"))?;
    if !dir.starts_with(common.join("worktrees")) {
        return Err(format!(
            "the new worktree's repository {} is not under {}",
            dir.display(),
            common.display()
        ));
    }
    Ok(dir)
}

/// `git` against `wc`'s held repository and work tree, with
/// [`HOST_GIT_CONFIG`] set, no system config, no inherited git environment,
/// and `index` as the index when given.
pub fn command(wc: &WorkCopy, index: Option<&Path>) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    for var in GIT_ENV {
        cmd.env_remove(var);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--git-dir")
        .arg(&wc.git_dir)
        .arg("--work-tree")
        .arg(&wc.work_tree);
    for c in HOST_GIT_CONFIG {
        cmd.args(["-c", c]);
    }
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    cmd.current_dir(&wc.work_tree)
        .stdin(std::process::Stdio::null());
    cmd
}

/// [`command`] run to completion; its output when git succeeded.
pub fn run(wc: &WorkCopy, index: Option<&Path>, args: &[&str]) -> Option<std::process::Output> {
    let out = command(wc, index).args(args).output().ok()?;
    out.status.success().then_some(out)
}

/// [`run`]'s stdout, trimmed.
pub fn line(wc: &WorkCopy, args: &[&str]) -> Option<String> {
    run(wc, None, args).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// [`command`] for its effect; the last stderr line on failure.
pub fn quiet(wc: &WorkCopy, args: &[&str]) -> Result<(), String> {
    let out = command(wc, None)
        .args(args)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err
        .lines()
        .last()
        .unwrap_or("git failed")
        .trim()
        .to_string())
}

/// The repository `git_dir` shares its objects and config with (itself, or
/// a worktree's main checkout's `.git`).
pub fn common_of(git_dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
        .canonicalize()
        .ok()
}
