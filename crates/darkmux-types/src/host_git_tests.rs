//! (#3457) Tests for [`super::HostGit`]: the flags on every built command,
//! and real git against a work tree whose `.git` a "run" replaced.

use super::*;
use std::path::Path;
use std::process::Command;

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn env_of(cmd: &Command, key: &str) -> Option<Option<String>> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

/// The settings, spelled out rather than read from `HARDENING_CONFIG`, so
/// dropping one from the constant fails here.
const EXPECTED_CONFIG: [&str; 7] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "commit.gpgsign=false",
    "log.showSignature=false",
    "core.sshCommand=ssh",
    "credential.helper=",
    "diff.ignoreSubmodules=dirty",
];
const EXPECTED_CLEARED: [&str; 6] =
    ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"];

/// Every hardening `-c` pair before the subcommand.
fn assert_hardened(args: &[String], subcommand: &str) {
    let sub_at = args.iter().position(|a| a == subcommand).expect("subcommand present");
    for setting in EXPECTED_CONFIG {
        let at = args
            .windows(2)
            .position(|w| w[0] == "-c" && w[1] == setting)
            .unwrap_or_else(|| panic!("`-c {setting}` missing from {args:?}"));
        assert!(at < sub_at, "`-c {setting}` must come before the subcommand: {args:?}");
    }
}

#[test]
fn trusted_command_carries_every_flag_and_runs_in_its_dir() {
    let cmd = HostGit::trusted("/repo").command(&["status", "--porcelain"]);
    assert_eq!(cmd.get_program(), "git");
    let args = args_of(&cmd);
    assert_hardened(&args, "status");
    assert_eq!(&args[args.len() - 2..], ["status", "--porcelain"]);
    assert!(!args.iter().any(|a| a.starts_with("--git-dir") || a.starts_with("--work-tree")), "{args:?}");
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/repo")));
    assert_eq!(env_of(&cmd, "GIT_CONFIG_NOSYSTEM"), Some(Some("1".to_string())));
    for var in EXPECTED_CLEARED {
        assert_eq!(env_of(&cmd, var), Some(None), "{var} must be removed");
    }
}

#[test]
fn run_written_command_names_the_held_git_dir_and_work_tree() {
    let cmd = HostGit::run_written("/repo/.git/worktrees/p1", "/wt/p1").command(&["rev-list", "HEAD"]);
    let args = args_of(&cmd);
    assert_hardened(&args, "rev-list");
    let sub_at = args.iter().position(|a| a == "rev-list").unwrap();
    let gd = args.iter().position(|a| a == "--git-dir=/repo/.git/worktrees/p1").expect("--git-dir");
    let wt = args.iter().position(|a| a == "--work-tree=/wt/p1").expect("--work-tree");
    assert!(gd < sub_at && wt < sub_at, "{args:?}");
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/wt/p1")));
    assert_eq!(env_of(&cmd, "GIT_CONFIG_NOSYSTEM"), Some(Some("1".to_string())));
    for var in EXPECTED_CLEARED {
        assert_eq!(env_of(&cmd, var), Some(None), "{var} must be removed");
    }
}

#[test]
fn diff_family_gets_no_textconv_and_no_ext_diff_right_after_the_subcommand() {
    for sub in ["diff", "log", "show"] {
        for git in [HostGit::trusted("/r"), HostGit::run_written("/g", "/w")] {
            let args = args_of(&git.command(&[sub, "main"]));
            let at = args.iter().position(|a| a == sub).unwrap();
            assert_eq!(&args[at + 1..], ["--no-textconv", "--no-ext-diff", "main"], "{sub}: {args:?}");
        }
    }
    let args = args_of(&HostGit::trusted("/r").command(&["status"]));
    assert!(!args.iter().any(|a| a.starts_with("--no-")), "status takes no diff flags: {args:?}");
}

// ── real git: a work tree whose `.git` the run replaced ─────────────────────

/// Fixture git: identity on the command line (a local shim refuses fixture
/// commits that carry it in the environment), file-protocol submodules on.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "protocol.file.allow=always"])
        .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A main repo with one commit and a linked worktree `wt` on branch `phase`.
/// Returns (main repo, worktree, the worktree's admin git dir).
pub(super) fn repo_with_worktree(root: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = root.join("wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "phase"]);
    let admin = repo.join(".git").join("worktrees").join("wt");
    assert!(admin.is_dir(), "admin dir at {}", admin.display());
    (repo, wt, admin)
}

/// What the run does: replace `wt/.git` (a pointer file) with a git
/// directory whose config runs `touch <fsmonitor marker>` as the fsmonitor
/// and `touch <clean marker>` as the clean filter every path is given, then
/// change a tracked file without changing its size, so git must re-hash it.
fn plant_git_dir(root: &Path, repo: &Path, wt: &Path, admin: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let staging = root.join("planted");
    git(root, &["clone", "-q", "--no-checkout", repo.to_str().unwrap(), staging.to_str().unwrap()]);
    std::fs::remove_file(wt.join(".git")).expect("the pointer file is there to replace");
    std::fs::rename(staging.join(".git"), wt.join(".git")).unwrap();
    std::fs::copy(admin.join("index"), wt.join(".git").join("index")).unwrap();
    let fs_marker = root.join("fsmonitor-ran");
    let clean_marker = root.join("clean-ran");
    let mut config = std::fs::read_to_string(wt.join(".git").join("config")).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = \"touch '{}'; false\"\n[filter \"x\"]\n\tclean = \"touch '{}'; cat\"\n",
        fs_marker.display(),
        clean_marker.display()
    ));
    std::fs::write(wt.join(".git").join("config"), config).unwrap();
    std::fs::write(wt.join(".gitattributes"), "* filter=x\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(wt.join("a.txt"), "b\n").unwrap();
    (fs_marker, clean_marker)
}

fn ran(marker: &Path) -> bool {
    let was = marker.exists();
    let _ = std::fs::remove_file(marker);
    was
}

#[test]
fn a_planted_git_dir_runs_its_commands_under_plain_git() {
    // The fixture can fail: plain git in the tree runs both planted commands.
    // Without this, the tests below could pass on a fixture that never armed.
    let tmp = tempfile::TempDir::new().unwrap();
    let (repo, wt, admin) = repo_with_worktree(tmp.path());
    let (fs_marker, clean_marker) = plant_git_dir(tmp.path(), &repo, &wt, &admin);
    let _ = Command::new("git").current_dir(&wt).args(["status", "--porcelain"]).output().unwrap();
    assert!(ran(&fs_marker), "plain git status must run the planted fsmonitor");
    assert!(ran(&clean_marker), "plain git status must run the planted clean filter");
}

#[test]
fn run_written_mode_never_reads_a_planted_git_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (repo, wt, admin) = repo_with_worktree(tmp.path());
    let (fs_marker, clean_marker) = plant_git_dir(tmp.path(), &repo, &wt, &admin);
    let host = HostGit::run_written(&admin, &wt);

    let status = host.stdout(&["status", "--porcelain"]).expect("status runs");
    assert!(status.contains("a.txt"), "the real index sees the change: {status:?}");
    host.stdout(&["diff", "--name-only", "main"]).expect("diff --name-only runs");
    let diff = host.stdout(&["diff", "main"]).expect("diff runs");
    assert!(diff.contains("+b"), "{diff}");
    assert_eq!(host.stdout(&["branch", "--show-current"]).as_deref(), Some("phase"));
    host.stdout(&["rev-list", "--max-count=1", "HEAD", "--not", "--remotes"]).expect("rev-list runs");

    assert!(!ran(&fs_marker), "the planted fsmonitor ran on the host");
    assert!(!ran(&clean_marker), "the planted clean filter ran on the host");
}

#[test]
fn trusted_mode_flags_stop_the_fsmonitor_and_dropping_tree_attributes_stops_the_filter() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (repo, wt, admin) = repo_with_worktree(tmp.path());
    let (fs_marker, clean_marker) = plant_git_dir(tmp.path(), &repo, &wt, &admin);
    let host = HostGit::trusted(&wt);

    // The limit, pinned: the flags alone cannot name an arbitrary filter.
    host.stdout(&["status", "--porcelain"]).expect("status runs");
    assert!(!ran(&fs_marker), "core.fsmonitor=false must win over the planted config");
    assert!(ran(&clean_marker), "documented limit: a trusted repo's own filter still runs");

    let no_attrs = host.clone().without_tree_attributes().expect("object format known");
    let status = no_attrs.stdout(&["status", "--porcelain"]).expect("status runs");
    assert!(status.contains("a.txt"), "{status:?}");
    assert!(!ran(&fs_marker), "the planted fsmonitor ran on the host");
    assert!(!ran(&clean_marker), "with no tree attributes no filter is named");
}

#[test]
fn a_submodule_the_run_planted_runs_nothing_under_run_written_mode() {
    // git checks a populated submodule's work tree with a CHILD git that
    // discovers the submodule's own `.git` and drops the parent's `-c`
    // settings. `diff.ignoreSubmodules=dirty` is what keeps it from spawning.
    let tmp = tempfile::TempDir::new().unwrap();
    let sub_src = tmp.path().join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    git(&sub_src, &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    git(&sub_src, &["add", "s.txt"]);
    git(&sub_src, &["commit", "-qm", "s"]);
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = tmp.path().join("wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "phase"]);
    let admin = repo.join(".git").join("worktrees").join("wt");

    // The run populates the submodule path with a repo of its own.
    std::fs::remove_dir(wt.join("sub")).unwrap();
    git(tmp.path(), &["clone", "-q", sub_src.to_str().unwrap(), wt.join("sub").to_str().unwrap()]);
    let fs_marker = tmp.path().join("fsmonitor-ran");
    let clean_marker = tmp.path().join("clean-ran");
    let cfg = wt.join("sub").join(".git").join("config");
    let mut config = std::fs::read_to_string(&cfg).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = \"touch '{}'; false\"\n[filter \"x\"]\n\tclean = \"touch '{}'; cat\"\n",
        fs_marker.display(),
        clean_marker.display()
    ));
    std::fs::write(&cfg, config).unwrap();
    std::fs::write(wt.join("sub").join(".gitattributes"), "* filter=x\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(wt.join("sub").join("s.txt"), "t\n").unwrap();

    let host = HostGit::run_written(&admin, &wt);
    host.stdout(&["status", "--porcelain"]).expect("status runs");
    host.stdout(&["diff", "main"]).expect("diff runs");
    assert!(!ran(&fs_marker), "the planted submodule's fsmonitor ran on the host");
    assert!(!ran(&clean_marker), "the planted submodule's clean filter ran on the host");
}
