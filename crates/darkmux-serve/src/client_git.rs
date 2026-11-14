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
//! `--no-textconv --no-ext-diff`. (#3457) Every call is built by
//! `darkmux_types::host_git::HostGit` (run-written mode with the held git
//! dir), so it also gets that helper's submodule flag, its neutralization of
//! every filter driver the repository's config defines, and its cleared
//! environment.

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
    // A relative pointer (`worktree.useRelativePaths`) is relative to the
    // worktree's own folder, never to this process's working directory.
    let dir = work_tree
        .join(pointer)
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

/// `git <args>` against `wc`'s held repository and work tree, through
/// `HostGit::run_written` with [`HOST_GIT_CONFIG`] added, no terminal, and
/// `index` as the index when given.
pub fn command(wc: &WorkCopy, index: Option<&Path>, args: &[&str]) -> std::process::Command {
    let mut cmd = darkmux_types::host_git::HostGit::run_written(&wc.git_dir, &wc.work_tree)
        .with_config(HOST_GIT_CONFIG)
        .command(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// The subcommands that read or write a work copy's file CONTENT, and so
/// pass it through any filter driver attributes name: `add` and `commit`
/// hash files, `revert` (and the rest) check them out.
const CONTENT_SUBCOMMANDS: [&str; 13] = [
    "add",
    "commit",
    "revert",
    "checkout",
    "switch",
    "restore",
    "reset",
    "stash",
    "merge",
    "cherry-pick",
    "rebase",
    "apply",
    "pull",
];

/// (#3457) Refuses a content subcommand in a work copy whose held
/// repository's config (not the operator's global one) defines a filter
/// driver. `HostGit` runs no such driver, since a run could have written
/// it, so the subcommand would hash or check out files unfiltered: a
/// git-crypt repo would commit plaintext, an LFS repo whole files. Refused,
/// never silently done the wrong way.
fn refuse_repo_filters(wc: &WorkCopy, args: &[&str]) -> Result<(), String> {
    let sub = darkmux_types::host_git::subcommand_of(args).unwrap_or_default();
    if !CONTENT_SUBCOMMANDS.contains(&sub) {
        return Ok(());
    }
    let drivers = darkmux_types::host_git::HostGit::run_written(&wc.git_dir, &wc.work_tree)
        .repo_filter_drivers();
    match drivers.is_empty() {
        true => Ok(()),
        false => Err(repo_filter_refusal(
            &drivers,
            &format!("`git {sub}` in {}", wc.work_tree.display()),
        )),
    }
}

/// (#3457) Why darkmux refuses `what` in a repository whose own config
/// defines the filter `drivers`, and the fix: the driver in the GLOBAL config,
/// where darkmux does run it.
pub fn repo_filter_refusal(drivers: &[String], what: &str) -> String {
    let named: Vec<String> = drivers.iter().map(|d| format!("`filter.{d}.*`")).collect();
    let first = &drivers[0];
    format!(
        "refusing {what}: the repository's own git config defines the filter driver {}, and darkmux \
         runs no filter a repository's config names (a run that had it mounted could have written \
         that config), so its files would be read or written unfiltered. Define the driver in your \
         global config instead, where darkmux runs it: `git config --global filter.{first}.clean ...` \
         (and `.smudge`, `.process`, `.required` as the repo's config has them; `git lfs install` \
         writes LFS's there by default, and git-crypt's four lines can be copied from the repo's \
         .git/config), then remove them from the repository's config and retry.",
        named.join(", ")
    )
}

/// [`command`] run to completion; its output when git succeeded.
pub fn run(wc: &WorkCopy, index: Option<&Path>, args: &[&str]) -> Option<std::process::Output> {
    refuse_repo_filters(wc, args).ok()?;
    let out = command(wc, index, args).output().ok()?;
    out.status.success().then_some(out)
}

/// [`run`]'s stdout, trimmed.
pub fn line(wc: &WorkCopy, args: &[&str]) -> Option<String> {
    run(wc, None, args).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// [`command`] run to completion; its output, or git's error (every stderr
/// line, so a failure names the file it failed on).
pub fn run_or_why(
    wc: &WorkCopy,
    index: Option<&Path>,
    args: &[&str],
) -> Result<std::process::Output, String> {
    refuse_repo_filters(wc, args)?;
    let out = command(wc, index, args)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if out.status.success() {
        return Ok(out);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = err
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    Err(if lines.is_empty() {
        format!("git {} failed", args.join(" "))
    } else {
        lines.join("; ")
    })
}

/// [`command`] for its effect; the last stderr line on failure.
pub fn quiet(wc: &WorkCopy, args: &[&str]) -> Result<(), String> {
    refuse_repo_filters(wc, args)?;
    let out = command(wc, None, args)
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
    let out = darkmux_types::host_git::HostGit::held_git_dir(git_dir)
        .command(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A checkout with one commit, and a worktree of it at `wt` made with
    /// `extra` config.
    fn checkout_with_worktree(extra: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "--quiet", "-b", "main"]);
        std::fs::write(src.join("a"), "a").unwrap();
        git(&src, &["add", "a"]);
        git(&src, &["commit", "--quiet", "-m", "a"]);
        let wt = tmp.path().join("ws");
        let mut args: Vec<&str> = extra.to_vec();
        let wt_s = wt.to_str().unwrap().to_string();
        args.extend(["worktree", "add", "--quiet", "-b", "mux/t-1", &wt_s]);
        git(&src, &args);
        (tmp, src.join(".git").canonicalize().unwrap(), wt)
    }

    /// (review) A relative `gitdir:` pointer (`worktree.useRelativePaths`,
    /// git 2.48+) is resolved against the worktree's own folder, not this
    /// process's working directory.
    #[test]
    fn a_relative_worktree_pointer_resolves_against_the_worktree() {
        let (_tmp, common, wt) = checkout_with_worktree(&["-c", "worktree.useRelativePaths=true"]);
        let text = std::fs::read_to_string(wt.join(".git")).unwrap();
        let pointer = text
            .trim()
            .strip_prefix("gitdir:")
            .unwrap()
            .trim()
            .to_string();
        // Older git writes an absolute pointer even when asked; write the
        // relative one by hand then.
        if Path::new(&pointer).is_absolute() {
            let rel = format!(
                "gitdir: ../src/.git/worktrees/{}\n",
                wt.file_name().unwrap().to_str().unwrap()
            );
            std::fs::write(wt.join(".git"), rel).unwrap();
        }
        assert_eq!(
            worktree_admin_dir(&wt, &common),
            Ok(common.join("worktrees").join("ws"))
        );
    }

    /// (review) The admin dir must be under the checkout's own
    /// `<common>/worktrees`: a pointer anywhere else is refused.
    #[test]
    fn a_worktree_pointer_outside_the_checkouts_worktrees_is_refused() {
        let (tmp, common, wt) = checkout_with_worktree(&[]);
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();
        let e = worktree_admin_dir(&wt, &common).unwrap_err();
        assert!(e.contains("is not under"), "{e}");
    }

    /// (review) A recorded repository inside the work tree (where a run can
    /// write) is no repository for host git.
    #[test]
    fn a_recorded_repository_inside_the_work_tree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, tree) = (tmp.path().join("base"), tmp.path().join("base/ws"));
        std::fs::create_dir_all(tree.join(".git")).unwrap();
        let outside = tmp.path().join("held");
        std::fs::create_dir_all(&outside).unwrap();
        record_git_dir(&base, "ws", &tree.join(".git")).unwrap();
        assert_eq!(
            work_copy_of(&base, "ws", &tree),
            None,
            "inside the work tree"
        );
        record_git_dir(&base, "ws", &outside).unwrap();
        assert!(work_copy_of(&base, "ws", &tree).is_some(), "outside it");
    }

    /// (review) Every host git command turns `core.fsmonitor` off, so even a
    /// setting darkmux did not expect to find (the operator's own) runs
    /// nothing. Read off the built command, since the structural fix leaves
    /// no repository a run controls for a test to plant one in.
    #[test]
    fn every_host_git_command_turns_off_the_program_running_settings() {
        let wc = WorkCopy {
            work_tree: PathBuf::from("/w"),
            git_dir: PathBuf::from("/g"),
        };
        let cmd = command(&wc, None, &["status"]);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for setting in [
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "commit.gpgsign=false",
            "credential.helper=",
            "tag.gpgsign=false",
            "core.untrackedCache=false",
            "diff.external=",
        ] {
            let at = args
                .iter()
                .position(|a| a == setting)
                .unwrap_or_else(|| panic!("{setting} missing: {args:?}"));
            assert_eq!(args[at - 1], "-c", "{args:?}");
        }
        let sub = args.iter().position(|a| a == "status").unwrap();
        for held in ["--git-dir=/g", "--work-tree=/w"] {
            let at = args
                .iter()
                .position(|a| a == held)
                .unwrap_or_else(|| panic!("{held} missing: {args:?}"));
            assert!(at < sub, "{held} must come before the subcommand: {args:?}");
        }
        let nosystem = cmd
            .get_envs()
            .any(|(k, v)| k == "GIT_CONFIG_NOSYSTEM" && v == Some(std::ffi::OsStr::new("1")));
        assert!(nosystem);
    }
}
