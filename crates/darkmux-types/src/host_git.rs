//! (#3457) The one way darkmux runs `git` on the host.
//!
//! A dispatch container mounts its workdir read-write, so a run can write
//! whatever git reads there: the `.git` directory of a checkout, or the
//! `.git` POINTER FILE of a linked worktree, which it can replace with a
//! directory of its own. Several git settings run a command (`core.fsmonitor`,
//! clean/smudge filters, `diff.<driver>.textconv` and `.command`, hooks,
//! `gpg.program`, `core.sshCommand`, `credential.helper`), so host git in such
//! a directory could execute what the run wrote, outside the container. A
//! redirected pointer also changes what host git READS (which branch HEAD
//! names, which index it compares against).
//!
//! Every production `git` spawn goes through [`HostGit`], in one of two modes:
//!
//! - [`HostGit::trusted`]: a checkout the operator owns (the repo a mission was
//!   launched from, the ACP panel's project). git finds `.git` from the
//!   directory as usual.
//! - [`HostGit::run_written`]: a work tree a run wrote (the coder phase's
//!   worktree). The caller hands over the git directory it HOLDS (derived from
//!   the main repository, never read out of the work tree) and git is given
//!   `--git-dir` and `--work-tree`, so nothing is discovered from the tree:
//!   a planted `.git` there is never read, its config never applies.
//!
//! Both modes add [`HARDENING_CONFIG`] and `GIT_CONFIG_NOSYSTEM=1`, and every
//! `diff`/`log`/`show` gets [`DIFF_SAFETY`]. What the flags cannot do is stop
//! a clean or smudge filter that a TRUSTED-mode repository's own config
//! defines: a filter driver's name is arbitrary, so no fixed `-c` can switch
//! it off. That is why a work tree a run wrote uses `run_written` (its config
//! comes from the git directory darkmux holds), and why a call that only needs
//! to know whether a trusted tree changed can drop in-tree attributes with
//! [`HostGit::without_tree_attributes`].
//!
//! `tests/host_git_conformance.rs` fails on any new `Command::new("git")` in
//! production code outside this module.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The `-c` settings every host git call carries, each switching off a
/// setting that runs a command or makes git read a repository the run wrote.
///
/// - `core.fsmonitor=false`: the fsmonitor hook runs on every index refresh.
/// - `core.hooksPath=/dev/null`: no hook runs (a `post-checkout` on
///   `worktree add`, for one).
/// - `commit.gpgsign=false` / `log.showSignature=false`: `gpg.program` never
///   runs to sign or to verify.
/// - `core.sshCommand=ssh` / `credential.helper=`: a repo's config cannot
///   name the program a network operation runs (an empty `credential.helper`
///   resets the helper list).
/// - `diff.ignoreSubmodules=dirty`: `status` and `diff` do not spawn a child
///   git inside a submodule's work tree. That child discovers the
///   submodule's own `.git`, which the run can plant, and git clears the
///   `-c` settings above for it (proven: a planted submodule's
///   `core.fsmonitor` and clean filter ran through `--git-dir` mode until
///   this was set). A submodule whose recorded commit changed is still
///   reported; uncommitted work INSIDE a submodule is not.
pub const HARDENING_CONFIG: [&str; 7] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "commit.gpgsign=false",
    "log.showSignature=false",
    "core.sshCommand=ssh",
    "credential.helper=",
    "diff.ignoreSubmodules=dirty",
];

/// Added right after a `diff`, `log` or `show` subcommand: no
/// `diff.<driver>.textconv` and no external diff driver runs.
pub const DIFF_SAFETY: [&str; 2] = ["--no-textconv", "--no-ext-diff"];

/// The subcommands that render diffs and so get [`DIFF_SAFETY`].
const DIFF_FAMILY: [&str; 3] = ["diff", "log", "show"];

/// Inherited repository-location variables, removed from every call so the
/// directory (or `--git-dir`) the caller names is the repository git uses.
/// A darkmux process started from a git hook inherits `GIT_DIR` and
/// `GIT_INDEX_FILE`; without this, host git would answer about that repo.
pub const CLEARED_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// git's empty tree, by object format: the attribute source
/// [`HostGit::without_tree_attributes`] points at.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// Where a [`HostGit`] runs, and what git may discover there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Repo {
    /// A checkout the operator owns: git discovers `.git` from `dir`.
    Trusted { dir: PathBuf },
    /// A work tree a run wrote: git is told the git dir, never discovers it.
    RunWritten { git_dir: PathBuf, work_tree: PathBuf },
}

/// A host git invocation target. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGit {
    repo: Repo,
    /// `Some(tree)`: `--attr-source=<tree>`, so `.gitattributes` in the work
    /// tree name no filter or diff driver.
    attr_source: Option<&'static str>,
}

impl HostGit {
    /// A checkout the operator owns. git runs with `dir` as its working
    /// directory and finds the repository from there.
    pub fn trusted(dir: impl Into<PathBuf>) -> Self {
        Self { repo: Repo::Trusted { dir: dir.into() }, attr_source: None }
    }

    /// A work tree a run wrote. `git_dir` MUST be a git directory the caller
    /// holds independently of the work tree (for a linked worktree, the
    /// `<repo>/.git/worktrees/<name>` admin dir found from the main
    /// repository), never one read out of the tree: the tree's `.git` is
    /// whatever the run left there. git runs in `work_tree` with
    /// `--git-dir` and `--work-tree` given, so it discovers nothing.
    pub fn run_written(git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Self {
        Self { repo: Repo::RunWritten { git_dir: git_dir.into(), work_tree: work_tree.into() }, attr_source: None }
    }

    /// Read attributes from git's empty tree instead of the work tree, so no
    /// path has a filter or diff driver: a clean filter the repository's own
    /// config defines cannot run. For a question like "did anything change"
    /// in a TRUSTED-mode tree a run may have written. The cost: a file that
    /// really is filtered (Git LFS) and whose stat changed reads as changed.
    /// `None` when the object format is unknown (the probe failed), in which
    /// case the caller has no safe way to ask and should say so.
    pub fn without_tree_attributes(self) -> Option<Self> {
        let tree = match self.stdout(&["rev-parse", "--show-object-format"])?.as_str() {
            "sha1" => EMPTY_TREE_SHA1,
            "sha256" => EMPTY_TREE_SHA256,
            _ => return None,
        };
        Some(Self { attr_source: Some(tree), ..self })
    }

    /// The directory git runs in: the trusted checkout, or the run's work
    /// tree.
    pub fn dir(&self) -> &Path {
        match &self.repo {
            Repo::Trusted { dir } => dir,
            Repo::RunWritten { work_tree, .. } => work_tree,
        }
    }

    /// The hardened `git <args>` command, not yet run. `args[0]` is the
    /// subcommand. The caller may add stdio or env; it must not add a
    /// `current_dir` or a `-C`, which would move git away from the
    /// directory this was built for.
    pub fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        for var in CLEARED_ENV {
            cmd.env_remove(var);
        }
        for setting in HARDENING_CONFIG {
            cmd.arg("-c").arg(setting);
        }
        if let Some(tree) = self.attr_source {
            cmd.arg(format!("--attr-source={tree}"));
        }
        match &self.repo {
            Repo::Trusted { dir } => {
                cmd.current_dir(dir);
            }
            Repo::RunWritten { git_dir, work_tree } => {
                cmd.arg(prefixed("--git-dir=", git_dir));
                cmd.arg(prefixed("--work-tree=", work_tree));
                cmd.current_dir(work_tree);
            }
        }
        let mut rest = args.iter();
        if let Some(sub) = rest.next() {
            cmd.arg(sub);
            if sub.as_ref().to_str().is_some_and(|s| DIFF_FAMILY.contains(&s)) {
                cmd.args(DIFF_SAFETY);
            }
        }
        cmd.args(rest);
        cmd
    }

    /// Run `git <args>` and collect its output.
    pub fn output<S: AsRef<OsStr>>(&self, args: &[S]) -> std::io::Result<Output> {
        self.command(args).output()
    }

    /// Trimmed stdout of `git <args>` when it ran and succeeded; `None`
    /// otherwise.
    pub fn stdout<S: AsRef<OsStr>>(&self, args: &[S]) -> Option<String> {
        let out = self.output(args).ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

fn prefixed(flag: &str, path: &Path) -> OsString {
    let mut s = OsString::from(flag);
    s.push(path.as_os_str());
    s
}

/// (#3457) The run's side, for host-git tests in every crate: a worktree
/// whose `.git` was replaced by a directory that runs marker commands.
#[cfg(any(test, feature = "test-support"))]
#[path = "host_git_fixture.rs"]
pub mod fixture;

#[cfg(test)]
#[path = "host_git_tests.rs"]
mod tests;
