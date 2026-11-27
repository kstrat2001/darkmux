//! (#3236) What a workspace measure says, on real directories and real git.

use super::*;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A checkout with `calc.py` (three lines), `notes.md` and a gitignored
/// `build.log`, all committed but the log.
fn repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    write(
        dir.path(),
        "calc.py",
        b"def add(a, b):\n    return a - b\n# end\n",
    );
    write(dir.path(), "notes.md", b"one\ntwo\n");
    write(dir.path(), ".gitignore", b"*.log\n");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    write(dir.path(), "build.log", b"ignored\n");
    dir
}

fn object_files(git_dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out)
            } else {
                out.push(p)
            }
        }
    }
    let mut out = Vec::new();
    walk(&git_dir.join("objects"), &mut out);
    out.sort();
    out
}

#[test]
fn a_fresh_workspace_that_gains_one_file_measures_it_with_its_lines() {
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    write(ws.path(), "out/answer.txt", b"one\ntwo\nthree");
    let m = baseline.measure();
    assert_eq!(m.summary.method, WorkspaceMethod::Snapshot);
    assert_eq!(m.summary.files_changed, Some(1));
    assert_eq!(m.summary.not_measured, None);
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (Some(3), Some(0)),
        "an unterminated last line counts"
    );
    assert_eq!(
        m.files,
        vec![WorkspaceFileChange {
            path: "out/answer.txt".into(),
            change: WorkspaceChangeKind::Added,
            lines_added: Some(3),
            lines_removed: Some(0)
        }]
    );
}

#[test]
fn a_workspace_nothing_changed_in_reads_zero_not_null() {
    let ws = TempDir::new().unwrap();
    write(ws.path(), "a.txt", b"same\n");
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(0));
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (Some(0), Some(0))
    );
    assert_eq!(m.summary.not_measured, None);
    assert!(m.files.is_empty());
}

#[cfg(unix)]
#[test]
fn an_unreadable_workspace_is_null_with_the_reason_never_zero() {
    use std::os::unix::fs::PermissionsExt;
    let ws = TempDir::new().unwrap();
    write(ws.path(), "locked/secret.txt", b"x\n");
    let locked = ws.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = fs::read_dir(&locked).is_ok(); // root reads it whatever the mode
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    let m = baseline.measure();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable_anyway {
        return;
    }
    assert_eq!(m.summary.files_changed, None);
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (None, None)
    );
    let why = m.summary.not_measured.expect("the reason is recorded");
    assert!(why.contains("before the dispatch"), "{why}");
}

#[test]
fn an_unreadable_workspace_after_the_run_is_null_with_the_reason() {
    let parent = TempDir::new().unwrap();
    let ws = parent.path().join("ws");
    fs::create_dir(&ws).unwrap();
    let baseline = WorkspaceBaseline::take(&ws, None);
    // The run replaced its workspace's root with a file: the post walk
    // cannot list it.
    fs::remove_dir(&ws).unwrap();
    fs::write(&ws, b"not a directory").unwrap();
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, None);
    assert!(
        m.summary
            .not_measured
            .as_deref()
            .unwrap_or("")
            .contains("when the dispatch ended"),
        "{:?}",
        m.summary.not_measured
    );
}

#[test]
fn the_git_path_counts_lines_with_numstat_and_never_writes_the_repository() {
    let dir = repo();
    let git_dir = dir.path().join(".git");
    let objects_before = object_files(&git_dir);
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(
        dir.path(),
        "calc.py",
        b"def add(a, b):\n    return a + b\n# end\n",
    );
    write(
        dir.path(),
        "test_calc.py",
        b"import calc\nassert calc.add(1, 2) == 3\n",
    );
    fs::remove_file(dir.path().join("notes.md")).unwrap();
    write(dir.path(), "build.log", b"ignored, and changed\n");
    let m = baseline.measure();
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(m.summary.not_measured, None);
    assert_eq!(m.summary.files_changed, Some(3));
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (Some(3), Some(3))
    );
    assert_eq!(
        m.summary.excluded,
        vec![WorkspaceChanges::GIT_IGNORED.to_string()]
    );
    assert_eq!(
        m.files,
        vec![
            WorkspaceFileChange {
                path: "calc.py".into(),
                change: WorkspaceChangeKind::Modified,
                lines_added: Some(1),
                lines_removed: Some(1)
            },
            WorkspaceFileChange {
                path: "notes.md".into(),
                change: WorkspaceChangeKind::Removed,
                lines_added: Some(0),
                lines_removed: Some(2)
            },
            WorkspaceFileChange {
                path: "test_calc.py".into(),
                change: WorkspaceChangeKind::Added,
                lines_added: Some(2),
                lines_removed: Some(0)
            },
        ]
    );
    assert_eq!(
        object_files(&git_dir),
        objects_before,
        "the operator's object store is only read"
    );
    assert_eq!(
        git(dir.path(), &["status", "--porcelain"]).lines().count(),
        3,
        "the checkout's own index is untouched"
    );
}

#[test]
fn the_git_path_measures_a_tree_that_was_already_dirty_against_itself() {
    let dir = repo();
    write(dir.path(), "calc.py", b"operator's own edit\n");
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    let m = baseline.measure();
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(
        m.summary.files_changed,
        Some(0),
        "the operator's edit before the run is not the run's"
    );
}

#[test]
fn a_binary_file_on_the_git_path_has_null_lines_and_null_totals() {
    let dir = repo();
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(dir.path(), "logo.bin", &[0u8, 1, 2, 0, 255]);
    write(dir.path(), "calc.py", b"x\n");
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(2));
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (None, None),
        "a total needs every file's lines"
    );
    let bin = m.files.iter().find(|f| f.path == "logo.bin").unwrap();
    assert_eq!((bin.lines_added, bin.lines_removed), (None, None));
    let calc = m.files.iter().find(|f| f.path == "calc.py").unwrap();
    assert_eq!(
        (calc.lines_added, calc.lines_removed),
        (Some(1), Some(3)),
        "per-file entries keep what is known"
    );
}

#[test]
fn a_repository_whose_config_defines_a_filter_is_measured_by_snapshot() {
    let dir = repo();
    git(dir.path(), &["config", "filter.crypt.clean", "cat"]);
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(dir.path(), "calc.py", b"x\n");
    let m = baseline.measure();
    assert_eq!(
        m.summary.method,
        WorkspaceMethod::Snapshot,
        "darkmux runs no filter a repo's config names, so git would hash unfiltered"
    );
    assert_eq!(m.summary.files_changed, Some(1));
}

#[test]
fn a_modified_file_on_the_snapshot_path_has_null_lines_and_null_totals() {
    let ws = TempDir::new().unwrap();
    write(ws.path(), "calc.py", b"a - b\n");
    write(ws.path(), "gone.txt", b"bye\n");
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    write(ws.path(), "calc.py", b"a + b\n");
    fs::remove_file(ws.path().join("gone.txt")).unwrap();
    write(ws.path(), "new.txt", b"hi\n");
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(3));
    assert_eq!(
        (m.summary.lines_added, m.summary.lines_removed),
        (None, None)
    );
    assert_eq!(
        m.files,
        vec![
            WorkspaceFileChange {
                path: "calc.py".into(),
                change: WorkspaceChangeKind::Modified,
                lines_added: None,
                lines_removed: None
            },
            WorkspaceFileChange {
                path: "gone.txt".into(),
                change: WorkspaceChangeKind::Removed,
                lines_added: None,
                lines_removed: None
            },
            WorkspaceFileChange {
                path: "new.txt".into(),
                change: WorkspaceChangeKind::Added,
                lines_added: Some(1),
                lines_removed: Some(0)
            },
        ]
    );
}

#[test]
fn an_added_binary_file_on_the_snapshot_path_has_null_lines() {
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    write(ws.path(), "a.bin", &[b'x', 0, b'\n']);
    let m = baseline.measure();
    assert_eq!(m.files[0].lines_added, None);
    assert_eq!(m.summary.lines_added, None);
}

#[test]
fn the_snapshot_names_the_directories_it_skipped() {
    let ws = TempDir::new().unwrap();
    write(ws.path(), "src/main.rs", b"fn main() {}\n");
    write(ws.path(), "node_modules/x/index.js", b"1\n");
    write(ws.path(), "target/debug/out", b"1\n");
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    write(ws.path(), "node_modules/x/index.js", b"2\n");
    write(ws.path(), "dist/bundle.js", b"built\n");
    let m = baseline.measure();
    assert_eq!(
        m.summary.files_changed,
        Some(0),
        "only skipped directories changed"
    );
    assert_eq!(
        m.summary.excluded,
        vec![
            "dist".to_string(),
            "node_modules".to_string(),
            "target".to_string()
        ]
    );
}

#[test]
fn the_file_list_is_capped_and_the_counts_cover_every_file() {
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    for i in 0..(MAX_LISTED_FILES + 2) {
        write(ws.path(), &format!("f{i:04}.txt"), b"x\n");
    }
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(MAX_LISTED_FILES as u64 + 2));
    assert_eq!(m.summary.lines_added, Some(MAX_LISTED_FILES as u64 + 2));
    assert_eq!(m.files.len(), MAX_LISTED_FILES);
    assert_eq!(m.files_omitted, 2);
    assert_eq!(m.files[0].path, "f0000.txt");
}

#[test]
fn the_envelope_shape_flattens_the_counts_beside_the_file_list() {
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    let wire = serde_json::to_value(baseline.measure()).unwrap();
    let keys: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "method",
            "files_changed",
            "not_measured",
            "lines_added",
            "lines_removed",
            "excluded",
            "files",
            "files_omitted"
        ]
    );
}

#[test]
fn a_repository_the_run_made_inside_the_checkout_is_named_by_a_marker_never_a_path() {
    let dir = repo();
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(dir.path(), "vendor/lib/a.txt", b"a\n");
    git(&dir.path().join("vendor/lib"), &["init", "-q"]);
    let m = baseline.measure();
    assert_eq!(
        m.summary.files_changed,
        Some(0),
        "a nested repository is left out, as the client work copy leaves it out"
    );
    assert_eq!(
        m.summary.excluded,
        vec![
            WorkspaceChanges::GIT_IGNORED.to_string(),
            WorkspaceChanges::NESTED_REPOSITORIES.to_string()
        ],
        "the flow stream carries no path, not even a skipped repository's"
    );
}

/// Every object the git path needs that the repository already holds is
/// read in place through the alternate, so the scratch store stays empty:
/// the operator's repository is read, never copied. Without the alternate,
/// git writes every blob and tree again into scratch.
#[test]
fn the_git_path_reads_the_repositorys_objects_in_place() {
    let dir = repo();
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    let scratch = baseline.scratch_objects().expect("measured with git");
    let mut written = Vec::new();
    for e in fs::read_dir(scratch).unwrap().flatten() {
        if e.path().is_dir() {
            written.extend(fs::read_dir(e.path()).unwrap().flatten().map(|f| f.path()));
        }
    }
    // `build.log` is ignored and the rest is committed: nothing new to write.
    assert!(
        written.is_empty(),
        "the repository's objects are read in place: {written:?}"
    );
    write(dir.path(), "calc.py", b"x\n");
    assert_eq!(baseline.measure().summary.files_changed, Some(1));
}

/// A checkout with a submodule `sub` whose `.git` the run replaced with a
/// writable copy whose config defines a filter driver, assigned to every
/// file there: `filter` is its `clean` command. The reviewer's setup.
fn repo_with_a_planted_submodule(filter: &str) -> (TempDir, TempDir) {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-q"]);
    write(upstream.path(), "a.txt", b"a\n");
    git(upstream.path(), &["add", "-A"]);
    git(upstream.path(), &["commit", "-q", "-m", "sub"]);
    let dir = repo();
    let url = upstream.path().to_string_lossy().to_string();
    git(
        dir.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &url,
            "sub",
        ],
    );
    git(dir.path(), &["commit", "-q", "-m", "add sub"]);
    let planted = dir.path().join("sub/.git");
    fs::remove_file(&planted).unwrap();
    let copy = Command::new("cp")
        .arg("-R")
        .arg(dir.path().join(".git/modules/sub"))
        .arg(&planted)
        .status()
        .unwrap();
    assert!(copy.success());
    git(
        dir.path(),
        &[
            "config",
            "-f",
            "sub/.git/config",
            "--unset",
            "core.worktree",
        ],
    );
    git(
        dir.path(),
        &[
            "config",
            "-f",
            "sub/.git/config",
            "filter.mark.clean",
            filter,
        ],
    );
    write(dir.path(), "sub/.git/info/attributes", b"* filter=mark\n");
    // Same size, so git must hash it (through the filter) to tell.
    write(dir.path(), "sub/a.txt", b"b\n");
    (dir, upstream)
}

/// (#3236 review, PROVEN host code execution) The git path never runs git
/// inside a submodule, whose `.git` a run can write: its index is fresh and
/// empty, so it holds no gitlink to check (`host_git.rs`'s rule). A seeded
/// index ran the planted filter on the host.
#[test]
fn a_planted_submodule_filter_never_runs_on_the_host() {
    let marker_dir = TempDir::new().unwrap();
    let marker = marker_dir.path().join("ran");
    let (dir, _upstream) =
        repo_with_a_planted_submodule(&format!("sh -c 'touch {}; cat'", marker.display()));
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(dir.path(), "sub/a.txt", b"c\n");
    let m = baseline.measure();
    assert!(!marker.exists(), "the planted filter ran on the host");
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(
        m.summary.files_changed,
        Some(0),
        "uncommitted work inside a submodule is not in its gitlink"
    );
    assert!(
        m.summary
            .excluded
            .contains(&WorkspaceChanges::SUBMODULES.to_string()),
        "and the record says so: {:?}",
        m.summary.excluded
    );
}

/// (#3236 review, PROVEN unbounded hang) A planted filter that never returns
/// cannot hold the measure: it is never run.
#[test]
fn a_planted_submodule_filter_that_hangs_cannot_hang_the_measure() {
    let (dir, _upstream) = repo_with_a_planted_submodule("sleep 60");
    let git_dir = dir.path().join(".git");
    let started = std::time::Instant::now();
    let baseline =
        WorkspaceBaseline::take_bounded(dir.path(), Some(&git_dir), Duration::from_secs(20));
    let m = baseline.measure();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(m.summary.files_changed, Some(0));
}

/// (#3236 review) Every git call is bounded: a command that does not return
/// (here a filter from the operator's own global config, which darkmux runs
/// by design, assigned by a `.gitattributes` the run wrote) ends at the
/// bound, and the record says the changes were not measured, never `0`.
#[test]
#[serial_test::serial] // sets GIT_CONFIG_GLOBAL for the git it starts
fn a_git_call_that_never_returns_ends_at_the_bound_as_not_measured() {
    let dir = repo();
    let global = TempDir::new().unwrap();
    let config = global.path().join("gitconfig");
    fs::write(&config, "[filter \"slow\"]\n\tclean = sleep 60\n").unwrap();
    let prev = std::env::var_os("GIT_CONFIG_GLOBAL");
    // SAFETY: serial, and restored before the test ends.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &config) };
    let git_dir = dir.path().join(".git");
    let baseline =
        WorkspaceBaseline::take_bounded(dir.path(), Some(&git_dir), Duration::from_secs(3));
    write(dir.path(), ".gitattributes", b"*.py filter=slow\n");
    write(dir.path(), "calc.py", b"changed\n");
    let started = std::time::Instant::now();
    let m = baseline.measure();
    let took = started.elapsed();
    match prev {
        Some(v) => unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", v) },
        None => unsafe { std::env::remove_var("GIT_CONFIG_GLOBAL") },
    }
    assert!(took < Duration::from_secs(10), "took {took:?}");
    assert_eq!(m.summary.files_changed, None);
    let why = m.summary.not_measured.expect("why");
    assert!(why.contains("git add did not finish within"), "{why}");
}

/// (#3236 review, PROVEN) `not_measured` rides the flow stream, so it is a
/// fixed reason: git's own error names the file it failed on.
#[cfg(unix)]
#[test]
fn a_failed_read_names_no_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo();
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    let secret = dir.path().join("customer-acme-payroll-2026.csv");
    fs::write(&secret, b"x\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&secret).is_ok() {
        return; // root reads it whatever the mode
    }
    let m = baseline.measure();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    let why = m
        .summary
        .not_measured
        .clone()
        .expect("git add fails on the unreadable file");
    assert_eq!(m.summary.files_changed, None);
    assert!(why.contains("git add failed (exit"), "{why}");
    let wire = serde_json::to_string(&m.summary).unwrap();
    assert!(
        !wire.contains("customer") && !wire.contains(&*dir.path().to_string_lossy()),
        "{wire}"
    );
}

/// (#3236 review) A file the checkout's index marks `--assume-unchanged` or
/// `--skip-worktree` is still counted when the run edits it: the measure's
/// index is its own.
#[test]
fn an_assume_unchanged_or_skip_worktree_file_is_still_counted() {
    let dir = repo();
    git(
        dir.path(),
        &["update-index", "--assume-unchanged", "calc.py"],
    );
    git(dir.path(), &["update-index", "--skip-worktree", "notes.md"]);
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    write(dir.path(), "calc.py", b"edited\n");
    write(dir.path(), "notes.md", b"edited\n");
    let m = baseline.measure();
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(m.summary.files_changed, Some(2), "{:?}", m.files);
}

/// (#3236 review) A symlink is counted by its target, added or retargeted,
/// and never followed.
#[cfg(unix)]
#[test]
fn a_symlink_added_or_retargeted_is_counted_by_its_target() {
    use std::os::unix::fs::symlink;
    let ws = TempDir::new().unwrap();
    symlink("/etc/hosts", ws.path().join("old")).unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    fs::remove_file(ws.path().join("old")).unwrap();
    symlink("/etc/passwd", ws.path().join("old")).unwrap();
    symlink("/dev/zero", ws.path().join("new")).unwrap();
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(2));
    assert_eq!(
        m.files,
        vec![
            WorkspaceFileChange {
                path: "new".into(),
                change: WorkspaceChangeKind::Added,
                lines_added: Some(1),
                lines_removed: Some(0)
            },
            WorkspaceFileChange {
                path: "old".into(),
                change: WorkspaceChangeKind::Modified,
                lines_added: None,
                lines_removed: None
            },
        ],
        "a link to /dev/zero is its target's one line, never read"
    );
}

/// (#3236 review) A permission change is a change.
#[cfg(unix)]
#[test]
fn a_mode_change_is_counted() {
    use std::os::unix::fs::PermissionsExt;
    let ws = TempDir::new().unwrap();
    write(ws.path(), "run.sh", b"echo hi\n");
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    fs::set_permissions(ws.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(1));
    assert_eq!(m.files[0].change, WorkspaceChangeKind::Modified);
}

/// (#3236 review) A file over the hashing cap is compared by size and mtime:
/// a same-size edit that moved its mtime is counted, one that kept both is
/// not seen, and the record names that it compared such files that way.
/// Lines of an added file over the cap are `null`, never read whole.
#[test]
fn a_file_over_the_hashing_cap_is_compared_by_size_and_mtime_and_named() {
    let ws = TempDir::new().unwrap();
    let big = ws.path().join("data.bin");
    let size = snapshot::MAX_FILE_SIZE_BYTES + 1;
    fs::File::create(&big).unwrap().set_len(size).unwrap();
    let aged = std::time::SystemTime::now() - Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    let edit = |keep_mtime: bool| {
        use std::io::{Seek, Write};
        let mut f = fs::File::options().write(true).open(&big).unwrap();
        f.seek(std::io::SeekFrom::Start(10)).unwrap();
        f.write_all(b"edited").unwrap();
        if keep_mtime {
            f.set_modified(aged).unwrap();
        }
    };
    edit(true);
    let kept = baseline.measure();
    assert_eq!(kept.summary.files_changed, Some(0));
    assert!(
        kept.summary
            .excluded
            .contains(&WorkspaceChanges::LARGE_FILES.to_string()),
        "{:?}",
        kept.summary.excluded
    );
    edit(false);
    let moved = baseline.measure();
    assert_eq!(moved.summary.files_changed, Some(1), "a moved mtime counts");

    let fresh = TempDir::new().unwrap();
    let added = WorkspaceBaseline::take(fresh.path(), None);
    {
        use std::io::Write;
        let mut f =
            std::io::BufWriter::new(fs::File::create(fresh.path().join("big.txt")).unwrap());
        let line = [b'a'; 1023]
            .iter()
            .copied()
            .chain([b'\n'])
            .collect::<Vec<u8>>();
        for _ in 0..=(size / 1024) {
            f.write_all(&line).unwrap();
        }
    }
    let m = added.measure();
    assert_eq!(
        m.files[0].lines_added, None,
        "a text file past the cap: its lines are not counted"
    );
}

/// (#3236 review) A name that is not UTF-8 (possible on Linux, refused by
/// APFS) is walked with its whole subtree, not skipped.
#[cfg(unix)]
#[test]
fn a_name_that_is_not_utf8_is_counted() {
    use std::os::unix::ffi::OsStrExt;
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    let odd = ws.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    if fs::create_dir(&odd).is_err() {
        return; // this filesystem refuses the name
    }
    fs::write(odd.join("inside.txt"), b"x\n").unwrap();
    let m = baseline.measure();
    assert_eq!(m.summary.files_changed, Some(1), "{:?}", m.files);
}

/// (#3236 review) A step's output forwarded to the next step's model keeps
/// the workspace counts and loses the file list; anything else is unchanged.
#[test]
fn a_forwarded_envelope_keeps_the_counts_and_drops_the_file_list() {
    let ws = TempDir::new().unwrap();
    let baseline = WorkspaceBaseline::take(ws.path(), None);
    write(ws.path(), "customer-acme.csv", b"x\n");
    let envelope = serde_json::json!({"result": "stop", "final_assistant": "done", "workspace_changes": baseline.measure()}).to_string();
    let forwarded = without_file_list(&envelope);
    assert!(!forwarded.contains("customer-acme"), "{forwarded}");
    let v: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
    assert_eq!(v["workspace_changes"]["files_changed"], 1);
    assert!(
        v["workspace_changes"].get("files").is_none()
            && v["workspace_changes"].get("files_omitted").is_none()
    );
    assert_eq!(v["final_assistant"], "done");
    for other in [
        "plain text",
        r#"{"result":"stop"}"#,
        "[1,2]",
        r#"{"workspace_changes": 3}"#,
    ] {
        assert_eq!(without_file_list(other), other, "returned byte for byte");
    }
}

/// The cost of both walks on a generated 10,000-file tree (4 KiB each, in 100
/// directories), on the snapshot path and the git path. A probe, not an
/// assertion:
/// `cargo nextest run -p darkmux-crew --lib workspace_changes_cost_probe --run-ignored only --no-capture`.
#[test]
#[ignore = "cost probe: prints timings for a 10k-file workspace"]
fn workspace_changes_cost_probe() {
    use std::time::Instant;
    let dir = TempDir::new().unwrap();
    let body = "x".repeat(4095) + "\n";
    for i in 0..10_000 {
        write(
            dir.path(),
            &format!("d{:02}/f{i:05}.txt", i % 100),
            body.as_bytes(),
        );
    }
    let timed = |label: &str, git_dir: Option<&Path>| {
        let t = Instant::now();
        let baseline = WorkspaceBaseline::take(dir.path(), git_dir);
        let pre = t.elapsed();
        write(dir.path(), "d00/f00000.txt", b"changed\n");
        let t = Instant::now();
        let m = baseline.measure();
        let post = t.elapsed();
        write(dir.path(), "d00/f00000.txt", body.as_bytes());
        eprintln!(
            "COST {label}: pre {pre:?}  post {post:?}  method {:?} files_changed {:?}",
            m.summary.method, m.summary.files_changed
        );
    };
    timed("snapshot", None);
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    timed("git", Some(&dir.path().join(".git")));
}
