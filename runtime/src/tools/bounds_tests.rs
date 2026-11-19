//! What a tool may hold in memory, and that no file can block one. `read`,
//! `search`, `edit`, `write` and `create_finding` open a file once, refuse
//! anything but a regular file at that open (a named pipe with no writer
//! would block a plain open, or a read, for ever, and a tool call has no
//! deadline), and never read more of it than their cap. Memory is the most
//! the calling thread held at once ([`thread_alloc`]), so a whole-file read
//! of a sparse file far over the cap shows, and nothing is ever resident on
//! disk.

use super::*;
use crate::test_support::thread_alloc::peak_allocated_during;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

const MIB: usize = 1024 * 1024;

fn workspace() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("darkmux-runtime-bounds-test").tempdir().expect("create tempdir")
}

/// A sparse file of `len` bytes starting with `head`: it takes no disk and
/// no memory until something reads it.
fn sparse(path: &Path, head: &[u8], len: usize) {
    fs::write(path, head).unwrap();
    fs::OpenOptions::new().write(true).open(path).unwrap().set_len(len as u64).unwrap();
}

/// A named pipe at `path`, with no writer and no reader.
fn fifo(path: &Path) -> PathBuf {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo {path:?}");
    path.to_path_buf()
}

/// `f`'s result within five seconds, or a failure naming what blocked. A
/// call still blocked on the pipe is released by opening its other end, so
/// a failing run does not leave a thread behind.
fn within_seconds<T: Send + 'static>(what: &str, pipe: &Path, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(out) => out,
        Err(_) => {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(pipe);
            panic!("{what} on a named pipe was still blocked after 5 s");
        }
    }
}

fn read_args(path: &str) -> String {
    serde_json::json!({"path": path, "offset": 1, "limit": 1}).to_string()
}

#[test]
fn read_of_a_file_far_over_its_cap_holds_about_the_cap() {
    let ws = workspace();
    sparse(&ws.path().join("big.txt"), b"first line\n", 256 * MIB);
    let (peak, out) = peak_allocated_during(|| execute_read(&read_args("big.txt"), ws.path()));
    let out = out.unwrap();
    assert!(out.starts_with("1: first line"), "{out}");
    assert!(out.contains(&format!("original file size {} bytes", 256 * MIB)), "{out}");
    assert!(peak < 8 * MIB, "read held {peak} bytes for a 256 MiB file; its cap is {READ_MAX_BYTES}");
}

#[test]
fn search_of_a_file_far_over_its_cap_holds_about_the_cap_and_says_so() {
    let ws = workspace();
    sparse(&ws.path().join("big.txt"), b"needle here\n", 256 * MIB);
    fs::write(ws.path().join("small.txt"), b"a needle too\n").unwrap();
    let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
    let (peak, out) = peak_allocated_during(|| execute_search(&raw, ws.path()));
    let out = out.unwrap();
    assert!(out.contains("big.txt:1:needle here") && out.contains("small.txt:1:a needle too"), "{out}");
    assert!(
        out.contains(&format!("only their first {SEARCH_FILE_MAX_BYTES} bytes were searched: big.txt")),
        "the model is told which file was cut: {out}"
    );
    assert!(!out.contains("small.txt]") && !out.contains("small.txt,"), "a file under the cap is not named: {out}");
    assert!(peak < 8 * MIB, "search held {peak} bytes for a 256 MiB file");
}

#[test]
fn search_under_the_cap_carries_no_note() {
    let ws = workspace();
    fs::write(ws.path().join("small.txt"), b"a needle\n").unwrap();
    let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
    assert_eq!(execute_search(&raw, ws.path()).unwrap(), "small.txt:1:a needle");
    let none = serde_json::json!({"pattern": "absent", "path": "."}).to_string();
    assert_eq!(execute_search(&none, ws.path()).unwrap(), "No matches for pattern \"absent\" in ..");
}

#[test]
fn edit_of_a_file_over_its_cap_is_refused_without_reading_it() {
    let ws = workspace();
    let len = WHOLE_FILE_MAX_BYTES as usize * 2;
    sparse(&ws.path().join("big.txt"), b"old\n", len);
    let raw = serde_json::json!({"path": "big.txt", "edits": [{"old_string": "old", "new_string": "new"}]}).to_string();
    let (peak, out) = peak_allocated_during(|| execute_edit(&raw, ws.path()));
    let err = format!("{:#}", out.unwrap_err());
    assert!(err.contains(&format!("{len} bytes")) && err.contains(&WHOLE_FILE_MAX_BYTES.to_string()), "{err}");
    assert!(peak < 2 * MIB, "edit held {peak} bytes refusing a {len}-byte file");
    assert_eq!(fs::metadata(ws.path().join("big.txt")).unwrap().len(), len as u64, "the file is untouched");
}

#[test]
fn edit_under_its_cap_still_edits() {
    let ws = workspace();
    fs::write(ws.path().join("a.txt"), b"old\n").unwrap();
    let raw = serde_json::json!({"path": "a.txt", "edits": [{"old_string": "old", "new_string": "new"}]}).to_string();
    execute_edit(&raw, ws.path()).unwrap();
    assert_eq!(fs::read_to_string(ws.path().join("a.txt")).unwrap(), "new\n");
}

#[test]
fn create_finding_on_a_file_over_its_cap_is_refused_without_reading_it() {
    let ws = workspace();
    fs::create_dir_all(ws.path().join("src")).unwrap();
    let len = WHOLE_FILE_MAX_BYTES as usize * 2;
    sparse(&ws.path().join("src/a.rs"), b"let x = 1;\n", len);
    let out = workspace();
    let (peak, msg) = peak_allocated_during(|| capture_finding_context("src/a.rs", 1, "let x = 1;", ws.path()));
    let msg = match msg {
        Err(reason) => reason,
        Ok(_) => panic!("a finding in a {len}-byte file was captured"),
    };
    assert!(msg.contains(&format!("{len} bytes")), "{msg}");
    assert!(peak < 2 * MIB, "create_finding held {peak} bytes");
    drop(out);
}

#[test]
fn read_of_a_named_pipe_is_refused_at_once() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    let root = ws.path().to_path_buf();
    let err = within_seconds("read", &pipe, move || execute_read(&read_args("p"), &root).map_err(|e| format!("{e:#}")));
    let err = err.unwrap_err();
    assert!(err.contains("named pipe") && err.contains("not a regular file"), "{err}");
}

#[test]
fn edit_of_a_named_pipe_is_refused_at_once() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    let root = ws.path().to_path_buf();
    let raw = serde_json::json!({"path": "p", "edits": [{"old_string": "a", "new_string": "b"}]}).to_string();
    let err = within_seconds("edit", &pipe, move || execute_edit(&raw, &root).map_err(|e| format!("{e:#}")));
    let err = err.unwrap_err();
    assert!(err.contains("named pipe") && err.contains("not a regular file"), "{err}");
}

#[test]
fn write_to_a_named_pipe_is_refused_at_once() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    let root = ws.path().to_path_buf();
    let raw = serde_json::json!({"path": "p", "content": "x"}).to_string();
    let err = within_seconds("write", &pipe, move || execute_write(&raw, &root).map_err(|e| format!("{e:#}")));
    let err = err.unwrap_err();
    assert!(err.contains("named pipe") && err.contains("not a regular file"), "{err}");
}

#[test]
fn create_finding_on_a_named_pipe_is_refused_at_once() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    let root = ws.path().to_path_buf();
    let msg = within_seconds("create_finding", &pipe, move || capture_finding_context("p", 1, "x", &root).err());
    let msg = msg.expect("a finding on a pipe is refused");
    assert!(msg.contains("named pipe"), "{msg}");
}

#[test]
fn search_skips_a_named_pipe_in_a_tree_and_refuses_one_named_directly() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    fs::write(ws.path().join("a.txt"), b"needle\n").unwrap();
    let root = ws.path().to_path_buf();
    let tree = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
    let found = within_seconds("search", &pipe, move || execute_search(&tree, &root).unwrap());
    assert_eq!(found, "a.txt:1:needle");
    let direct = serde_json::json!({"pattern": "needle", "path": "p"}).to_string();
    let root = ws.path().to_path_buf();
    let err = within_seconds("search", &pipe, move || execute_search(&direct, &root).map_err(|e| format!("{e:#}")));
    assert!(err.is_err(), "{err:?}");
}

#[test]
fn a_mod_attaching_a_named_pipe_is_not_recorded() {
    let ws = workspace();
    let pipe = fifo(&ws.path().join("p"));
    let out = workspace();
    let (root, out_dir) = (ws.path().to_path_buf(), out.path().to_path_buf());
    let raw = serde_json::json!({"kit": "k", "attach": ["p"]}).to_string();
    let run = within_seconds("create_mod", &pipe, move || execute_create_mod_with(&raw, &out_dir, &root, None, "").unwrap());
    assert!(run.result.starts_with("NOT RECORDED") && run.result.contains("not a file"), "{}", run.result);
}

#[test]
fn a_directory_is_named_as_one() {
    let ws = workspace();
    fs::create_dir(ws.path().join("d")).unwrap();
    let err = format!("{:#}", execute_read(&read_args("d"), ws.path()).unwrap_err());
    assert!(err.contains("a directory"), "{err}");
}

#[test]
fn a_cut_that_splits_a_character_still_searches_what_comes_before_it() {
    let ws = workspace();
    // 7 bytes, then two-byte characters: the cap at an even byte count
    // falls inside one.
    let body = format!("needle\n{}", "é".repeat(SEARCH_FILE_MAX_BYTES as usize / 2 + 10));
    assert_eq!((SEARCH_FILE_MAX_BYTES as usize - 7) % 2, 1, "the cap splits a character");
    fs::write(ws.path().join("big.txt"), body).unwrap();
    let raw = serde_json::json!({"pattern": "needle", "path": "big.txt"}).to_string();
    let out = execute_search(&raw, ws.path()).unwrap();
    assert!(out.starts_with("big.txt:1:needle"), "{out}");
}
