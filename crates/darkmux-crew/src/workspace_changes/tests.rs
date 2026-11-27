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

/// A checkout whose files are older than its index, as a real one is: every
/// object the measure needs is already the repository's own, read through
/// the alternate, so the scratch store stays empty. Without the alternate,
/// git writes every blob and tree again into scratch. (The seeded index only
/// saves the hashing, which no object shows: the cost check measures it.)
#[test]
fn the_git_path_reuses_the_checkouts_index_and_reads_its_objects_in_place() {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    for (rel, body) in [("a.txt", "a\n"), ("b.txt", "b\n"), ("c/d.txt", "d\n")] {
        write(dir.path(), rel, body.as_bytes());
        fs::File::options()
            .write(true)
            .open(dir.path().join(rel))
            .unwrap()
            .set_modified(aged)
            .unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    let git_dir = dir.path().join(".git");
    let baseline = WorkspaceBaseline::take(dir.path(), Some(&git_dir));
    let scratch = baseline.scratch_objects().expect("measured with git");
    let written: Vec<PathBuf> = {
        let mut v = Vec::new();
        for e in fs::read_dir(scratch).unwrap().flatten() {
            if e.path().is_dir() {
                v.extend(fs::read_dir(e.path()).unwrap().flatten().map(|f| f.path()));
            }
        }
        v
    };
    assert!(
        written.is_empty(),
        "the repository's objects are read in place: {written:?}"
    );
    write(dir.path(), "a.txt", b"a\nmore\n");
    let m = baseline.measure();
    assert_eq!(m.summary.method, WorkspaceMethod::Git);
    assert_eq!(
        (
            m.summary.files_changed,
            m.summary.lines_added,
            m.summary.lines_removed
        ),
        (Some(1), Some(1), Some(0))
    );
}

/// The cost of both walks on a generated 10,000-file tree (4 KiB each, in 100
/// directories, aged so a seeded index matches), on the snapshot path and the
/// git path (with and without the seeded index). A probe, not an assertion:
/// `cargo nextest run -p darkmux-crew --lib workspace_changes_cost_probe --run-ignored only --no-capture`.
#[test]
#[ignore = "cost probe: prints timings for a 10k-file workspace"]
fn workspace_changes_cost_probe() {
    use std::time::Instant;
    let dir = TempDir::new().unwrap();
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    let body = "x".repeat(4095) + "\n";
    for i in 0..10_000 {
        let rel = format!("d{:02}/f{i:05}.txt", i % 100);
        write(dir.path(), &rel, body.as_bytes());
        fs::File::options()
            .write(true)
            .open(dir.path().join(rel))
            .unwrap()
            .set_modified(aged)
            .unwrap();
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
    let git_dir = dir.path().join(".git");
    timed("git, seeded index", Some(&git_dir));
    let objects = TempDir::new().unwrap();
    let run = git_runner(dir.path(), &git_dir, objects.path());
    let t = Instant::now();
    git_tree::snapshot_tree(dir.path(), &run, None).unwrap();
    eprintln!(
        "COST git, one tree with a fresh index (no seed): {:?}",
        t.elapsed()
    );
    let t = Instant::now();
    git_tree::snapshot_tree(dir.path(), &run, Some(&git_dir.join("index"))).unwrap();
    eprintln!(
        "COST git, one tree with the seeded index: {:?}",
        t.elapsed()
    );
}
