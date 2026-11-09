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
//! Both modes add [`HARDENING_CONFIG`], `GIT_CONFIG_NOSYSTEM=1` and the
//! [`CLEARED_ENV`] removals, and the subcommand gets [`DIFF_SAFETY`] and
//! [`IGNORE_SUBMODULES`] where it takes them.
//!
//! **Filter drivers.** A clean/smudge/process filter's name is arbitrary, so
//! no fixed `-c` switches it off, and attributes can assign one from the work
//! tree's `.gitattributes`, the git dir's `info/attributes` or a
//! `core.attributesFile`. So every call first reads the filter drivers the
//! repository's own config defines (`git config --show-scope --get-regexp
//! '^filter\.'`, which runs nothing) and passes each one empty
//! (`clean`/`smudge`/`process` = "", `required` = false; an empty command is
//! pass-through on git 2.50). A driver defined only in the operator's GLOBAL
//! config (Git LFS's `git lfs install` default) is left alone: it is the
//! operator's own program, and keeping it keeps LFS checkouts working. A
//! driver the repository's config defines or redefines (`git lfs install
//! --local` included) never runs. The overrides travel as
//! `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, so a driver
//! name holding `=` or a space is still named exactly.
//!
//! Textconv and external diff drivers cannot be switched off the same way
//! (git refuses to run an empty one rather than skipping it), so they are
//! handled by `--no-textconv --no-ext-diff` on every subcommand that renders
//! a diff.
//!
//! `tests/host_git_conformance.rs` fails on any new `git` spawn in production
//! code outside this module.

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
/// - `diff.ignoreSubmodules=dirty`: the default for any diff machinery that
///   takes no `--ignore-submodules` flag. It is only a default: a
///   `.gitmodules` the run wrote can set a submodule's own `ignore = none`
///   over it, which is why [`IGNORE_SUBMODULES`] goes on the command line too.
pub const HARDENING_CONFIG: [&str; 7] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "commit.gpgsign=false",
    "log.showSignature=false",
    "core.sshCommand=ssh",
    "credential.helper=",
    "diff.ignoreSubmodules=dirty",
];

/// Added after a subcommand that renders a diff: no `diff.<driver>.textconv`
/// and no external diff driver (`diff.<driver>.command`, `diff.external`)
/// runs.
pub const DIFF_SAFETY: [&str; 2] = ["--no-textconv", "--no-ext-diff"];

/// Added after `status` and every diff-rendering subcommand: git does not
/// spawn a child git inside a submodule's work tree to check it for changes.
/// That child discovers the submodule's own `.git`, which the run can plant,
/// and git clears this process's `-c` settings for it (proven: a planted
/// submodule's `core.fsmonitor` and clean filter ran through `--git-dir`
/// mode). On the command line it beats a run-written `.gitmodules`'
/// `ignore = none`. A submodule whose recorded commit moved is still
/// reported; uncommitted work INSIDE a submodule is not.
pub const IGNORE_SUBMODULES: &str = "--ignore-submodules=dirty";

/// The subcommands that render a diff: they take both [`DIFF_SAFETY`] and
/// [`IGNORE_SUBMODULES`].
const DIFF_FAMILY: [&str; 7] = ["diff", "diff-index", "diff-files", "diff-tree", "log", "show", "format-patch"];

/// Inherited variables removed from every call. The repository-location ones
/// make the directory (or `--git-dir`) the caller names the repository git
/// uses: a darkmux process started from a git hook inherits `GIT_DIR` and
/// `GIT_INDEX_FILE`. The rest name config or a program for git to run
/// (`GIT_CONFIG_PARAMETERS` is how an outer `git -c` reaches a child); none
/// is needed by a call this helper makes, which never fetches or pushes.
/// `GIT_CONFIG_GLOBAL` is deliberately kept: it is where the operator keeps
/// their own global config, so clearing it would make darkmux read a
/// different one than the operator's own git does.
pub const CLEARED_ENV: [&str; 13] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXTERNAL_DIFF",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_ASKPASS",
    "GIT_EXEC_PATH",
];

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
}

impl HostGit {
    /// A checkout the operator owns. git runs with `dir` as its working
    /// directory and finds the repository from there.
    pub fn trusted(dir: impl Into<PathBuf>) -> Self {
        Self { repo: Repo::Trusted { dir: dir.into() } }
    }

    /// A work tree a run wrote. `git_dir` MUST be a git directory the caller
    /// holds independently of the work tree (for a linked worktree, the
    /// `<repo>/.git/worktrees/<name>` admin dir found from the main
    /// repository), never one read out of the tree: the tree's `.git` is
    /// whatever the run left there. git runs in `work_tree` with
    /// `--git-dir` and `--work-tree` given, so it discovers nothing.
    pub fn run_written(git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Self {
        Self { repo: Repo::RunWritten { git_dir: git_dir.into(), work_tree: work_tree.into() } }
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
    /// subcommand. Building it runs one `git config` read (see the module
    /// doc's "Filter drivers"). The caller may add stdio or env; it must not
    /// add a `current_dir` or a `-C`, which would move git away from the
    /// directory this was built for.
    pub fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut cmd = self.base();
        neutralize_filters(&mut cmd, &self.repo_filter_drivers());
        let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
        let at = subcommand_end(&args);
        cmd.args(&args[..at]);
        cmd.args(subcommand_flags(&args[..at]));
        cmd.args(&args[at..]);
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

    /// `git` with the env, the hardening `-c` settings and the mode's
    /// location, before any subcommand.
    fn base(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        for var in CLEARED_ENV {
            cmd.env_remove(var);
        }
        for setting in HARDENING_CONFIG {
            cmd.arg("-c").arg(setting);
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
        cmd
    }

    /// The filter driver names any config but the operator's global one
    /// defines for this repository. Reading config runs nothing. A failed
    /// read (no repository, no driver) is an empty list.
    fn repo_filter_drivers(&self) -> Vec<String> {
        let out = self.base().args(["config", "-z", "--show-scope", "--get-regexp", "^filter\\."]).output();
        match out {
            Ok(o) if o.status.success() => repo_defined_drivers(&o.stdout),
            _ => Vec::new(),
        }
    }
}

/// Parse `git config -z --show-scope --get-regexp '^filter\.'`: entries of
/// `<scope> NUL <key> LF <value> NUL`. The driver name is the key between
/// `filter.` and its last `.`; drivers whose every key is GLOBAL are left out.
fn repo_defined_drivers(raw: &[u8]) -> Vec<String> {
    let fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    let mut names: Vec<String> = Vec::new();
    for pair in fields.chunks_exact(2) {
        if pair[0] == b"global" {
            continue;
        }
        let key = pair[1].split(|b| *b == b'\n').next().unwrap_or_default();
        let key = String::from_utf8_lossy(key);
        let Some(name) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.')).map(|(n, _)| n) else {
            continue;
        };
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Pass every driver in `names` empty, through `GIT_CONFIG_COUNT` (exact for
/// any name, unlike `-c`, which splits at the first `=`).
fn neutralize_filters(cmd: &mut Command, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let mut i = 0usize;
    for name in names {
        for (key, value) in [("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")] {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), format!("filter.{name}.{key}"));
            cmd.env(format!("GIT_CONFIG_VALUE_{i}"), value);
            i += 1;
        }
    }
    cmd.env("GIT_CONFIG_COUNT", i.to_string());
}

/// The index just past the subcommand: past `args[0]`, or past `show` in
/// `stash show`, so a flag lands where that subcommand reads it.
fn subcommand_end(args: &[&OsStr]) -> usize {
    match (args.first().and_then(|a| a.to_str()), args.get(1).and_then(|a| a.to_str())) {
        (None, _) => 0,
        (Some("stash"), Some("show")) => 2,
        _ => 1,
    }
}

/// The flags the subcommand that ends `head` takes: [`DIFF_SAFETY`] and
/// [`IGNORE_SUBMODULES`] for one that renders a diff (and `stash show`),
/// [`DIFF_SAFETY`] for `blame`, [`IGNORE_SUBMODULES`] for `status`.
fn subcommand_flags(head: &[&OsStr]) -> Vec<&'static str> {
    let sub = head.last().and_then(|a| a.to_str()).unwrap_or_default();
    match sub {
        _ if DIFF_FAMILY.contains(&sub) => vec![DIFF_SAFETY[0], DIFF_SAFETY[1], IGNORE_SUBMODULES],
        "blame" => DIFF_SAFETY.to_vec(),
        "status" => vec![IGNORE_SUBMODULES],
        _ => Vec::new(),
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
