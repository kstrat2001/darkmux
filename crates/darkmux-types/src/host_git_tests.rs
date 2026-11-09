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
const EXPECTED_CLEARED: [&str; 13] = [
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
    assert_eq!(&args[args.len() - 3..], ["status", "--ignore-submodules=dirty", "--porcelain"]);
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
fn a_held_git_dir_names_only_the_git_dir_and_extra_config_follows_the_hardening() {
    let cmd = HostGit::held_git_dir("/g").with_config(&["tag.gpgsign=false"]).command(&["rev-parse", "HEAD"]);
    let args = args_of(&cmd);
    assert_hardened(&args, "rev-parse");
    assert!(args.contains(&"--git-dir=/g".to_string()) && !args.iter().any(|a| a.starts_with("--work-tree")), "{args:?}");
    let extra = args.iter().position(|a| a == "tag.gpgsign=false").expect("extra -c");
    assert_eq!(args[extra - 1], "-c");
    assert!(extra > args.iter().position(|a| a == "diff.ignoreSubmodules=dirty").unwrap(), "after the hardening: {args:?}");
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/g")));
}

#[test]
fn each_subcommand_gets_its_flags_right_after_it() {
    const ALL: [&str; 3] = ["--no-textconv", "--no-ext-diff", "--ignore-submodules=dirty"];
    for sub in ["diff", "diff-index", "diff-files", "diff-tree", "log", "show", "format-patch"] {
        for git in [HostGit::trusted("/r"), HostGit::run_written("/g", "/w")] {
            let args = args_of(&git.command(&[sub, "main"]));
            let at = args.iter().position(|a| a == sub).unwrap();
            assert_eq!(&args[at + 1..], [ALL[0], ALL[1], ALL[2], "main"], "{sub}: {args:?}");
        }
    }
    let args = args_of(&HostGit::trusted("/r").command(&["stash", "show", "-p"]));
    assert!(args.ends_with(&["stash".into(), "show".into(), ALL[0].into(), ALL[1].into(), ALL[2].into(), "-p".into()]), "{args:?}");
    let args = args_of(&HostGit::trusted("/r").command(&["blame", "a.txt"]));
    assert!(args.ends_with(&["blame".into(), ALL[0].into(), ALL[1].into(), "a.txt".into()]), "{args:?}");
    let args = args_of(&HostGit::trusted("/r").command(&["status"]));
    assert!(args.ends_with(&["status".into(), ALL[2].into()]), "{args:?}");
    let args = args_of(&HostGit::trusted("/r").command(&["--literal-pathspecs", "log", "-1"]));
    assert!(args.ends_with(&["--literal-pathspecs".into(), "log".into(), ALL[0].into(), ALL[1].into(), ALL[2].into(), "-1".into()]), "a leading global option: {args:?}");
    let args = args_of(&HostGit::trusted("/r").command(&["-c", "x.y=z", "--no-pager", "diff", "HEAD"]));
    assert!(args.ends_with(&["diff".into(), ALL[0].into(), ALL[1].into(), ALL[2].into(), "HEAD".into()]), "-c takes a value: {args:?}");
    let args = args_of(&HostGit::trusted("/r").command(&["branch", "-D", "log"]));
    assert!(args.ends_with(&["branch".into(), "-D".into(), "log".into()]), "a branch named log is not a subcommand: {args:?}");
}

#[test]
fn filter_drivers_from_any_scope_but_global_are_named() {
    let raw = b"global\0filter.lfs.clean\ngit-lfs clean -- %f\0local\0filter.my drv.clean\ntouch m\0\
                worktree\0filter.a.b.process\nx\0local\0filter.my drv.smudge\ny\0local\0filter.lfs.smudge\nevil\0";
    assert_eq!(repo_defined_drivers(raw), vec!["my drv".to_string(), "a.b".to_string(), "lfs".to_string()]);
    assert!(repo_defined_drivers(b"").is_empty());
}

#[test]
fn every_repo_defined_filter_driver_is_passed_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["config", "filter.x=y.clean", "touch m"]);
    let cmd = HostGit::trusted(tmp.path()).command(&["status"]);
    let env = |k: &str| env_of(&cmd, k).flatten();
    assert_eq!(env("GIT_CONFIG_COUNT").as_deref(), Some("4"));
    let pairs: Vec<(String, String)> =
        (0..4).map(|i| (env(&format!("GIT_CONFIG_KEY_{i}")).unwrap(), env(&format!("GIT_CONFIG_VALUE_{i}")).unwrap())).collect();
    assert_eq!(
        pairs,
        [("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")]
            .map(|(k, v)| (format!("filter.x=y.{k}"), v.to_string()))
    );
    let none = HostGit::trusted(tempfile::TempDir::new().unwrap().path()).command(&["status"]);
    assert_eq!(env_of(&none, "GIT_CONFIG_COUNT"), Some(None), "no driver: the inherited count is still cleared");
}

// ── real git: a work tree whose `.git` the run replaced ─────────────────────

use super::fixture::{git, AttrPlace, PlantedWorktree, TreeChange};

#[test]
fn a_planted_git_dir_runs_its_commands_under_plain_git() {
    // The fixture can fail: plain git in the tree runs every planted
    // command. Without this, the tests below could pass on a fixture that
    // never armed.
    let f = PlantedWorktree::new();
    f.plant(TreeChange::Content);
    let _ = Command::new("git").current_dir(&f.wt).args(["status", "--porcelain"]).output().unwrap();
    assert!(f.fsmonitor_ran(), "plain git status must run the planted fsmonitor");
    assert!(f.clean_ran(), "plain git status must run the planted clean filter");
    let _ = Command::new("git").current_dir(&f.wt).args(["diff", "main"]).output().unwrap();
    assert!(f.textconv_ran(), "plain git diff must run the planted textconv");
}

#[test]
fn run_written_mode_never_reads_a_planted_git_dir() {
    let f = PlantedWorktree::new();
    f.plant(TreeChange::Content);
    let host = HostGit::run_written(&f.admin, &f.wt);

    let status = host.stdout(&["status", "--porcelain"]).expect("status runs");
    assert!(status.contains("a.txt"), "the real index sees the change: {status:?}");
    host.stdout(&["diff", "--name-only", "main"]).expect("diff --name-only runs");
    let diff = host.stdout(&["diff", "main"]).expect("diff runs");
    assert!(diff.contains("+z"), "{diff}");
    assert_eq!(host.stdout(&["branch", "--show-current"]).as_deref(), Some("phase"), "the held HEAD, not the planted one");
    host.stdout(&["rev-list", "--max-count=1", "HEAD", "--not", "--remotes"]).expect("rev-list runs");

    assert!(!f.fsmonitor_ran(), "the planted fsmonitor ran on the host");
    assert!(!f.clean_ran(), "the planted clean filter ran on the host");
    assert!(!f.textconv_ran(), "the planted textconv ran on the host");
}

#[test]
fn trusted_mode_runs_nothing_a_planted_repo_config_names_wherever_the_attributes_are() {
    for place in [AttrPlace::WorkTree, AttrPlace::InfoAttributes, AttrPlace::AttributesFile] {
        let f = PlantedWorktree::new();
        f.plant_with(TreeChange::Content, place);
        let host = HostGit::trusted(&f.wt);
        let status = host.stdout(&["status", "--porcelain"]).expect("status runs");
        assert!(status.contains("a.txt"), "{status:?}");
        host.stdout(&["diff", "main"]).expect("diff runs");
        host.stdout(&["log", "-p", "-2"]).expect("log runs");
        assert!(!f.fsmonitor_ran(), "core.fsmonitor=false must win over the planted config");
        assert!(!f.clean_ran(), "the planted clean filter ran on the host");
        assert!(!f.textconv_ran(), "--no-textconv must win over the planted driver");
    }
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
    // The run also edits the work tree's `.gitmodules`: a per-submodule
    // `ignore` there beats `diff.ignoreSubmodules` from `-c`; only the
    // command-line `--ignore-submodules` beats it.
    let mut gitmodules = std::fs::read_to_string(wt.join(".gitmodules")).unwrap();
    gitmodules.push_str("\tignore = none\n");
    std::fs::write(wt.join(".gitmodules"), gitmodules).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(wt.join("sub").join("s.txt"), "t\n").unwrap();

    // The fixture arms: an explicit --git-dir alone still spawns the child.
    let _ = Command::new("git")
        .arg(format!("--git-dir={}", admin.display()))
        .arg(format!("--work-tree={}", wt.display()))
        .args(["status", "--porcelain"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(clean_marker.exists(), "without diff.ignoreSubmodules the planted submodule's filter runs");
    let _ = std::fs::remove_file(&clean_marker);
    let _ = std::fs::remove_file(&fs_marker);

    let host = HostGit::run_written(&admin, &wt);
    host.stdout(&["status", "--porcelain"]).expect("status runs");
    assert!(!fs_marker.exists() && !clean_marker.exists(), "status ran the planted submodule's commands");
    host.stdout(&["diff", "main"]).expect("diff runs");
    host.stdout(&["log", "-p", "-1"]).expect("log runs");
    assert!(!fs_marker.exists(), "the planted submodule's fsmonitor ran on the host");
    assert!(!clean_marker.exists(), "the planted submodule's clean filter ran on the host");
}
