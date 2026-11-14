//! Tests for `client_changes`: every kind of change reads back exactly as the
//! per-file reader it replaced read it, and the read costs a fixed number of
//! git calls whatever the branch holds.

use super::*;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// `owned`, borrowed as entries.
fn borrow(owned: &[(String, Vec<u8>, Vec<u8>)]) -> Vec<Entry<'_>> {
    owned
        .iter()
        .map(|(m, c, p)| (m.as_str(), c.as_slice(), p.as_slice()))
        .collect()
}

/// A file in a test tree: its mode, its content (for a gitlink, the commit
/// id it records), and its path as raw bytes.
type Entry<'a> = (&'a str, &'a [u8], &'a [u8]);

/// Plain git in the test repository (tests are outside the host-git rule).
fn raw_git(dir: &Path, index: Option<&Path>, args: &[&OsStr], input: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let mut child = cmd.spawn().expect("git runs");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_str(dir: &Path, args: &[&str]) -> String {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    String::from_utf8(raw_git(dir, None, &args, b""))
        .unwrap()
        .trim()
        .to_string()
}

fn init_repo(dir: &Path) -> WorkCopy {
    git_str(dir, &["init", "--quiet"]);
    WorkCopy {
        work_tree: dir.to_path_buf(),
        git_dir: dir.join(".git"),
    }
}

/// A commit of exactly `entries`, written through a scratch index, so any
/// path (a newline in it, bytes that are not UTF-8) and any mode go in as
/// given, never through the file system.
fn commit_of(dir: &Path, entries: &[Entry], parent: Option<&str>) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let index = scratch.path().join("index");
    let mut info = Vec::new();
    let mut written: std::collections::HashMap<&[u8], String> = std::collections::HashMap::new();
    for (mode, content, path) in entries {
        let oid = if *mode == "160000" {
            String::from_utf8(content.to_vec()).unwrap()
        } else {
            let args = [
                OsStr::new("hash-object"),
                OsStr::new("-w"),
                OsStr::new("--stdin"),
            ];
            let hash = || {
                String::from_utf8(raw_git(dir, None, &args, content))
                    .unwrap()
                    .trim()
                    .to_string()
            };
            written.entry(content).or_insert_with(hash).clone()
        };
        info.extend_from_slice(format!("{mode} {oid}\t").as_bytes());
        info.extend_from_slice(path);
        info.push(0);
    }
    let args = [
        OsStr::new("update-index"),
        OsStr::new("-z"),
        OsStr::new("--index-info"),
    ];
    raw_git(dir, Some(&index), &args, &info);
    let tree =
        String::from_utf8(raw_git(dir, Some(&index), &[OsStr::new("write-tree")], b"")).unwrap();
    let mut args = vec!["commit-tree", tree.trim(), "-m", "c"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    git_str(dir, &args)
}

/// The reader `branch_files` replaced, kept as the oracle: `git show` and
/// `git ls-tree` for each changed file.
fn per_file_oracle(
    wc: &WorkCopy,
    base: &str,
    head: &str,
    cap: usize,
) -> Result<Vec<BranchFile>, ChangesError> {
    let git = |args: &[&str]| crate::client_git::run(wc, None, args);
    let Some(listing) = git(&["diff", "--no-renames", "--name-status", "-z", base, head]) else {
        return Err(ChangesError::Listing);
    };
    let fields: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect();
    let mut files = Vec::new();
    let mut total = 0usize;
    for pair in fields.chunks(2) {
        let [status, path] = pair else { continue };
        if status.starts_with('D') {
            files.push(BranchFile {
                path: path.clone(),
                deleted: Some(true),
                mode: None,
                content: None,
            });
            continue;
        }
        let Some(blob) = git(&["show", &format!("{head}:{path}")]) else {
            return Err(ChangesError::Unreadable(path.clone()));
        };
        total += blob.stdout.len();
        if total > cap {
            return Err(ChangesError::TooLarge);
        }
        let mode =
            crate::client_git::line(wc, &["--literal-pathspecs", "ls-tree", head, "--", path])
                .and_then(|l| l.split_whitespace().next().map(str::to_string))
                .unwrap_or_else(|| "100644".to_string());
        files.push(BranchFile {
            path: path.clone(),
            deleted: None,
            mode: Some(mode),
            content: Some(crate::client_workspace::base64_encode(&blob.stdout)),
        });
    }
    Ok(files)
}

/// What `/changes` would send for a read: the files' JSON, or the error.
fn sent(read: Result<Vec<BranchFile>, ChangesError>) -> String {
    match read {
        Ok(files) => serde_json::to_string(&files).unwrap(),
        Err(e) => format!("{e:?}"),
    }
}

/// One line per file (path, then `deleted` or mode and content length), or
/// the error: the shape each scenario is pinned to.
fn summary(read: &Result<Vec<BranchFile>, ChangesError>) -> String {
    match read {
        Err(e) => format!("{e:?}"),
        Ok(files) => files
            .iter()
            .map(|f| match (&f.deleted, &f.mode, &f.content) {
                (Some(true), _, _) => format!("{:?} deleted", f.path),
                (_, Some(m), Some(c)) => {
                    let len = crate::client_workspace::base64_decode(c).unwrap().len();
                    format!("{:?} {m} {len}", f.path)
                }
                _ => format!("{:?} ?", f.path),
            })
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

const SUBMODULE_COMMIT: &[u8] = b"0123456789abcdef0123456789abcdef01234567";
const BINARY: &[u8] = b"\x00\x01\xff\xfe\x89PNG\r\n\x1a\n\x00tail";

struct Scenario {
    name: &'static str,
    base: Vec<Entry<'static>>,
    head: Vec<Entry<'static>>,
    cap: usize,
    expect: &'static str,
}

fn scenario(
    name: &'static str,
    base: Vec<Entry<'static>>,
    head: Vec<Entry<'static>>,
    expect: &'static str,
) -> Scenario {
    Scenario {
        name,
        base,
        head,
        cap: MAX,
        expect,
    }
}

const MAX: usize = 25 * 1024 * 1024;
const KEEP: Entry = ("100644", b"keep\n", b"keep.txt");

fn scenarios() -> Vec<Scenario> {
    vec![
        scenario(
            "added",
            vec![KEEP],
            vec![KEEP, ("100644", b"new\n", b"a.txt")],
            r#""a.txt" 100644 4"#,
        ),
        scenario(
            "deleted",
            vec![KEEP, ("100644", b"old\n", b"gone.txt")],
            vec![KEEP],
            r#""gone.txt" deleted"#,
        ),
        scenario(
            "modified",
            vec![("100644", b"one\n", b"keep.txt")],
            vec![("100644", b"two\r\n", b"keep.txt")],
            r#""keep.txt" 100644 5"#,
        ),
        scenario(
            "empty",
            vec![KEEP],
            vec![KEEP, ("100644", b"", b"empty")],
            r#""empty" 100644 0"#,
        ),
        scenario(
            "binary",
            vec![KEEP],
            vec![KEEP, ("100644", BINARY, b"img.png")],
            r#""img.png" 100644 17"#,
        ),
        scenario(
            "symlink",
            vec![KEEP],
            vec![KEEP, ("120000", b"keep.txt", b"link")],
            r#""link" 120000 8"#,
        ),
        scenario(
            "mode change",
            vec![("100644", b"run\n", b"x.sh")],
            vec![("100755", b"run\n", b"x.sh")],
            r#""x.sh" 100755 4"#,
        ),
        scenario(
            "file becomes a symlink",
            vec![KEEP, ("100644", b"was a file\n", b"t")],
            vec![KEEP, ("120000", b"keep.txt", b"t")],
            r#""t" 120000 8"#,
        ),
        scenario(
            "renamed",
            vec![KEEP, ("100644", b"same content\n", b"old/name.txt")],
            vec![KEEP, ("100644", b"same content\n", b"new/name.txt")],
            r#""new/name.txt" 100644 13 | "old/name.txt" deleted"#,
        ),
        scenario(
            "awkward names",
            vec![KEEP],
            vec![
                KEEP,
                ("100644", b"s\n", b"with space.txt"),
                ("100644", b"n\n", b"new\nline.txt"),
                ("100644", b"d\n", b"-dash"),
                ("100644", b"c\n", b"a:b"),
                ("100644", b"u\n", "日本.txt".as_bytes()),
                ("100644", b"q\n", b"quote\"tab\t.txt"),
                ("100644", b"g\n", b"*.glob"),
            ],
            r#""*.glob" 100644 2 | "-dash" 100644 2 | "a:b" 100644 2 | "new\nline.txt" 100644 2 | "quote\"tab\t.txt" 100644 2 | "with space.txt" 100644 2 | "日本.txt" 100644 2"#,
        ),
        // A name that is not UTF-8 reaches the reply as its lossy form; a
        // changed one cannot be read back by that name.
        scenario(
            "non-UTF-8 name, added",
            vec![KEEP],
            vec![KEEP, ("100644", b"x\n", b"bad\xff.txt")],
            "Unreadable(\"bad\u{fffd}.txt\")",
        ),
        scenario(
            "non-UTF-8 name, deleted",
            vec![KEEP, ("100644", b"x\n", b"bad\xff.txt")],
            vec![KEEP],
            "\"bad\u{fffd}.txt\" deleted",
        ),
        scenario(
            "submodule",
            vec![KEEP],
            vec![KEEP, ("160000", SUBMODULE_COMMIT, b"sub")],
            r#"Unreadable("sub")"#,
        ),
        Scenario {
            name: "too large",
            base: vec![KEEP],
            head: vec![
                KEEP,
                ("100644", b"0123456789", b"a"),
                ("100644", b"0123456789", b"b"),
            ],
            cap: 15,
            expect: "TooLarge",
        },
        Scenario {
            name: "exactly the cap",
            base: vec![KEEP],
            head: vec![
                KEEP,
                ("100644", b"0123456789", b"a"),
                ("100644", b"01234", b"b"),
            ],
            cap: 15,
            expect: r#""a" 100644 10 | "b" 100644 5"#,
        },
        Scenario {
            name: "unreadable before too large",
            base: vec![KEEP],
            head: vec![
                KEEP,
                ("160000", SUBMODULE_COMMIT, b"a-sub"),
                ("100644", b"0123456789", b"b"),
            ],
            cap: 5,
            expect: r#"Unreadable("a-sub")"#,
        },
        Scenario {
            name: "too large before unreadable",
            base: vec![KEEP],
            head: vec![
                KEEP,
                ("100644", b"0123456789", b"a"),
                ("160000", SUBMODULE_COMMIT, b"z-sub"),
            ],
            cap: 5,
            expect: "TooLarge",
        },
        scenario("nothing changed", vec![KEEP], vec![KEEP], ""),
    ]
}

/// Every kind of change reads back byte for byte as the per-file reader
/// read it, and as each scenario pins.
#[test]
fn every_kind_of_change_reads_back_exactly_as_the_per_file_reader_did() {
    for s in scenarios() {
        let dir = tempfile::tempdir().unwrap();
        let wc = init_repo(dir.path());
        let base = commit_of(dir.path(), &s.base, None);
        let head = commit_of(dir.path(), &s.head, Some(&base));
        let new = branch_files(&wc, &base, &head, s.cap);
        assert_eq!(summary(&new), s.expect, "{}", s.name);
        assert_eq!(
            sent(new),
            sent(per_file_oracle(&wc, &base, &head, s.cap)),
            "{}",
            s.name
        );
    }
}

/// A branch of every readable kind at once, in git's order, reads back as
/// the per-file reader read it.
#[test]
fn a_mixed_branch_reads_back_exactly_as_the_per_file_reader_did() {
    let readable: Vec<Scenario> = scenarios()
        .into_iter()
        .filter(|s| s.cap == MAX && !s.expect.starts_with("Unreadable"))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    // Each scenario in a folder of its own, so their paths never collide.
    let prefixed = |entries: &[Entry<'static>], i: usize| -> Vec<(String, Vec<u8>, Vec<u8>)> {
        entries
            .iter()
            .map(|(m, c, p)| {
                (
                    m.to_string(),
                    c.to_vec(),
                    [format!("s{i:02}/").as_bytes(), p].concat(),
                )
            })
            .collect()
    };
    let (mut base, mut head) = (Vec::new(), Vec::new());
    for (i, s) in readable.iter().enumerate() {
        base.extend(prefixed(&s.base, i));
        head.extend(prefixed(&s.head, i));
    }
    let base_commit = commit_of(dir.path(), &borrow(&base), None);
    let head_commit = commit_of(dir.path(), &borrow(&head), Some(&base_commit));
    let new = branch_files(&wc, &base_commit, &head_commit, MAX);
    let files = new.as_ref().expect("the mixed branch reads");
    assert!(files.len() >= 15, "the fixture must arm: {}", summary(&new));
    assert_eq!(
        sent(new),
        sent(per_file_oracle(&wc, &base_commit, &head_commit, MAX))
    );
}

/// Reading a branch's files runs nothing the repository's config names: a
/// smudge filter and a textconv driver assigned to every file stay unrun.
#[test]
fn reading_the_files_runs_no_filter_or_textconv_the_repository_names() {
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    let marker = dir.path().join("ran");
    let touch = format!("touch '{}'; cat", marker.display());
    for key in [
        "filter.mark.smudge",
        "filter.mark.clean",
        "diff.mark.textconv",
    ] {
        git_str(dir.path(), &["config", key, &touch]);
    }
    let base = commit_of(dir.path(), &[KEEP], None);
    let attrs: Entry = (
        "100644",
        b"* filter=mark diff=mark text eol=crlf\n",
        b".gitattributes",
    );
    let head = commit_of(
        dir.path(),
        &[KEEP, attrs, ("100644", b"lf\nonly\n", b"f.txt")],
        Some(&base),
    );
    let read = branch_files(&wc, &base, &head, MAX);
    assert_eq!(
        summary(&read),
        r#"".gitattributes" 100644 38 | "f.txt" 100644 8"#
    );
    assert_eq!(sent(read), sent(per_file_oracle(&wc, &base, &head, MAX)));
    assert!(
        !marker.exists(),
        "reading the files ran the repository's filter or textconv"
    );
}

/// A submodule whose commit this repository happens to hold is refused like
/// any other: its commit is not file content. (The per-file reader sent
/// `git show`'s rendering of that commit as the file.)
#[test]
fn a_submodule_whose_commit_the_repository_holds_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    let base = commit_of(dir.path(), &[KEEP], None);
    let head = commit_of(
        dir.path(),
        &[KEEP, ("160000", base.as_bytes(), b"sub")],
        Some(&base),
    );
    assert_eq!(
        summary(&branch_files(&wc, &base, &head, MAX)),
        r#"Unreadable("sub")"#
    );
}

/// A read refused for size returns at once, even with megabytes still to
/// come from git.
#[test]
fn a_read_past_the_cap_stops_with_git_still_sending() {
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    let base = commit_of(dir.path(), &[KEEP], None);
    let big = vec![b'x'; 4 * 1024 * 1024];
    let head = commit_of(
        dir.path(),
        &[
            KEEP,
            ("100644", b"small", b"a"),
            ("100644", &big, b"b"),
            ("100644", &big, b"c"),
        ],
        Some(&base),
    );
    assert_eq!(summary(&branch_files(&wc, &base, &head, 1024)), "TooLarge");
}

/// Ten thousand files read in full: more blob ids (410 KB) than the pipe
/// and git's read buffer hold while git's answers wait to be read.
#[test]
fn a_branch_of_thousands_of_files_reads_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    let base = commit_of(dir.path(), &[KEEP], None);
    let names: Vec<String> = (0..10_000)
        .map(|i| format!("d{}/f{i:05}.txt", i % 10))
        .collect();
    let body = b"line of content\n".repeat(8);
    let mut head: Vec<Entry> = vec![KEEP];
    head.extend(
        names
            .iter()
            .map(|n| ("100644", body.as_slice(), n.as_bytes())),
    );
    let head = commit_of(dir.path(), &head, Some(&base));
    let files = branch_files(&wc, &base, &head, MAX).expect("the branch reads");
    assert_eq!(files.len(), 10_000);
    assert!(files.iter().all(|f| f
        .content
        .as_deref()
        .and_then(crate::client_workspace::base64_decode)
        .as_deref()
        == Some(body.as_slice())));
}

/// The git found first on `PATH`.
fn real_git() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH")
}

/// Runs `f` with a `git` on `PATH` that logs each call's arguments (one
/// line each) to `log`, then runs the real git.
fn with_logged_git<T>(log: &Path, f: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt;
    let shim = tempfile::tempdir().unwrap();
    let script = shim.path().join("git");
    let body = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        log.display(),
        real_git().display()
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = std::env::var_os("PATH").unwrap();
    let mut paths = vec![shim.path().to_path_buf()];
    paths.extend(std::env::split_paths(&old));
    // SAFETY: only the test that runs in its own process calls this.
    unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
    let out = f();
    unsafe { std::env::set_var("PATH", old) };
    out
}

/// How many git processes reading `files` changed files spawns.
fn git_calls_for(files: usize) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let wc = init_repo(dir.path());
    let base = commit_of(dir.path(), &[KEEP], None);
    let names: Vec<String> = (0..files).map(|i| format!("f{i}.txt")).collect();
    let mut head: Vec<Entry> = vec![KEEP];
    head.extend(
        names
            .iter()
            .map(|n| ("100644", b"x\n" as &[u8], n.as_bytes())),
    );
    let head = commit_of(dir.path(), &head, Some(&base));
    let log = dir.path().join("calls.log");
    let read = with_logged_git(&log, || branch_files(&wc, &base, &head, MAX));
    assert_eq!(read.expect("the branch reads").len(), files);
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let tree = wc.work_tree.display().to_string();
    calls.lines().filter(|l| l.contains(&tree)).count()
}

/// Reading a branch's files costs the same few git calls for 1 file or 40,
/// not a `show` and an `ls-tree` (each with its config read) per file.
///
/// Runs in its own process: the logging `git` goes on `PATH`, which every
/// test in this binary reads, and `#[serial]` holds back only other serial
/// tests. Under `cargo test` (one process) a test spawning git alongside
/// found the shim, whose folder was then deleted under it.
#[test]
fn reading_a_branch_costs_a_fixed_number_of_git_calls() {
    darkmux_types::run_in_own_process!();
    let (one, forty) = (git_calls_for(1), git_calls_for(40));
    assert_eq!(
        one, forty,
        "git calls grew with the file count: {one} for 1 file, {forty} for 40"
    );
    assert!(one <= 4, "{one} git calls for one file");
}
