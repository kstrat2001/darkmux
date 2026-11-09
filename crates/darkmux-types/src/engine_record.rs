//! (#3448) The record of each engine server darkmux started (DESIGN.md,
//! "Engine servers darkmux starts").
//!
//! A server darkmux starts outlives the darkmux process that started it, so
//! "its parent is darkmux" cannot be what makes it darkmux's: a daemon
//! restart, or a CLI dispatch that exits, leaves the server reparented. The
//! record is what does. One small JSON file per endpoint,
//! `<darkmux-home>/run/engines/<endpoint>.json`, written atomically when the
//! server starts and removed when darkmux stops it.
//!
//! A server is darkmux's only while its record matches the live process: the
//! pid AND the process start stamp (`Platform::process_start_stamp`). A pid
//! whose process exited, or was reused by another process, makes the record
//! stale, and darkmux never signals that pid (contract 4). A record this
//! darkmux cannot read (another schema, a corrupt file) claims nothing.
//!
//! The directory is the user's own, as `config.json` is: a record edited by
//! hand to name a live process with its real start stamp would read as
//! darkmux's, since nothing here compares the process's command line. The
//! trust boundary is the home directory, not this file.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The record shape this darkmux writes and reads.
pub const ENGINE_RECORD_SCHEMA: u32 = 1;

/// One engine server darkmux started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineRecord {
    pub schema: u32,
    /// The `endpoints` id it serves.
    pub endpoint: String,
    /// The `managed` token (`tensorfold`).
    pub engine: String,
    /// The model it was started with (a profile model's `id`).
    pub model: String,
    /// The context it was started with (`--context`).
    pub n_ctx: u32,
    pub port: u16,
    pub pid: u32,
    /// `Platform::process_start_stamp(pid)` read right after the start.
    pub start_stamp: u64,
    /// The darkmux process that started it. Informational: ownership is
    /// darkmux's, whichever darkmux process started the server.
    pub owner_pid: u32,
    /// The command darkmux ran, for an operator reading the file.
    pub argv: Vec<String>,
    /// Where the server's output goes.
    pub log: PathBuf,
    pub started_at_ms: u64,
    /// The engine's own version line when it started.
    pub engine_version: String,
}

/// What a record says about its server now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    /// The recorded process is alive and started when the record says: the
    /// server is darkmux's.
    Running,
    /// No process has the recorded pid: the server exited.
    Exited,
    /// Another process has the recorded pid. The server exited, and the pid
    /// is someone else's now: never signal it.
    PidReused,
}

impl EngineRecord {
    /// The state of the recorded server, given how this machine reads a
    /// process's start stamp (`Platform::process_start_stamp` in production).
    pub fn state(&self, stamp_of: impl Fn(u32) -> Option<u64>) -> ServerState {
        match stamp_of(self.pid) {
            Some(s) if s == self.start_stamp => ServerState::Running,
            Some(_) => ServerState::PidReused,
            None => ServerState::Exited,
        }
    }

    /// [`EngineRecord::state`] on this machine.
    pub fn state_now(&self) -> ServerState {
        self.state(|pid| darkmux_platform::current().process_start_stamp(pid))
    }
}

/// Why a record file could not be used. It claims nothing either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableRecord {
    pub path: PathBuf,
    pub why: String,
}

/// `<darkmux-home>/run/engines`.
pub fn dir() -> PathBuf {
    crate::paths::user_root_guarded().join("run").join("engines")
}

/// The longest file name a record gets, inside every common filesystem's
/// 255-byte limit with room for the temp and lock suffixes.
const MAX_NAME_BYTES: usize = 200;

/// The file an endpoint's record lives in. An endpoint id is the
/// operator's text, so every byte outside `[a-z0-9._-]` (uppercase
/// included, so a case-insensitive disk never merges two ids), and a
/// leading `.`, is written `%XX`: no id can name a path outside `dir` or a
/// hidden file, and two ids never share a file.
pub fn path_in(dir: &Path, endpoint: &str) -> PathBuf {
    dir.join(format!("{}.json", file_stem(endpoint)))
}

fn file_stem(endpoint: &str) -> String {
    let mut name = String::new();
    for (i, b) in endpoint.bytes().enumerate() {
        let plain = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || (b == b'.' && i > 0);
        if plain {
            name.push(b as char);
        } else {
            name.push_str(&format!("%{b:02X}"));
        }
    }
    name
}

fn invalid(why: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, why)
}

/// `path_in`, refusing an id no record can be kept for.
fn checked_path(dir: &Path, endpoint: &str) -> std::io::Result<PathBuf> {
    if endpoint.is_empty() {
        return Err(invalid("an endpoint id is empty".into()));
    }
    if file_stem(endpoint).len() > MAX_NAME_BYTES {
        return Err(invalid(format!("endpoint id `{endpoint}` is too long for a record file name")));
    }
    Ok(path_in(dir, endpoint))
}

/// A symlinked records directory is never read or written through.
fn real_dir(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => Err(format!("{} is a symlink, which darkmux never follows", dir.display())),
        _ => Ok(()),
    }
}

/// The pid of a process darkmux could have started: never the kernel's (0)
/// or init's (1).
fn plausible(rec: &EngineRecord) -> Result<(), String> {
    if rec.pid < 2 {
        return Err(format!("records pid {}, which no server darkmux starts can have", rec.pid));
    }
    Ok(())
}

/// Hold `endpoint`'s record lock while a write or a compare-and-remove runs,
/// so neither sees the other half done.
fn record_lock(dir: &Path, endpoint: &str) -> std::io::Result<crate::flock::FlockGuard> {
    real_dir(dir).map_err(invalid)?;
    let lock = checked_path(dir, endpoint)?.with_extension("json.lock");
    if std::fs::symlink_metadata(&lock).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(invalid(format!("{} is a symlink, which darkmux never follows", lock.display())));
    }
    crate::flock::lock_exclusive(&lock).map_err(|e| std::io::Error::other(format!("{e:#}")))
}

/// Write `rec` atomically (a temp file of this writer's own, created fresh
/// so a planted file or symlink is never written through, then renamed over
/// the record), creating `dir` as needed.
pub fn write_in(dir: &Path, rec: &EngineRecord) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    plausible(rec).map_err(invalid)?;
    let path = checked_path(dir, &rec.endpoint)?;
    std::fs::create_dir_all(dir)?;
    real_dir(dir).map_err(invalid)?;
    let _lock = record_lock(dir, &rec.endpoint)?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(rec).map_err(std::io::Error::other)?;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()));
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The record for `endpoint`: `Ok(None)` when there is none, `Err` when the
/// file is there but this darkmux cannot use it.
pub fn read_in(dir: &Path, endpoint: &str) -> Result<Option<EngineRecord>, UnreadableRecord> {
    let path = path_in(dir, endpoint);
    let unreadable = |why: String| UnreadableRecord { path: path.clone(), why };
    real_dir(dir).map_err(unreadable)?;
    if !regular_file(&path).map_err(unreadable)? {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| unreadable(e.to_string()))?;
    let rec = parse(&text).map_err(unreadable)?;
    if rec.endpoint != endpoint {
        return Err(unreadable(format!("names endpoint `{}`, not `{endpoint}`", rec.endpoint)));
    }
    Ok(Some(rec))
}

/// Whether a record file is there (`false` for none), refusing a symlink
/// and an error reading its metadata, which is never "no record".
fn regular_file(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
        Ok(m) if m.file_type().is_symlink() => Err("a symlink, which darkmux never follows".into()),
        Ok(_) => Ok(true),
    }
}

fn parse(text: &str) -> Result<EngineRecord, String> {
    let doc: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match doc.get("schema").and_then(|s| s.as_u64()) {
        Some(s) if s == u64::from(ENGINE_RECORD_SCHEMA) => {}
        other => return Err(format!("record schema {other:?}; this darkmux reads {ENGINE_RECORD_SCHEMA}")),
    }
    let rec: EngineRecord = serde_json::from_value(doc).map_err(|e| e.to_string())?;
    plausible(&rec)?;
    Ok(rec)
}

/// Every record in `dir`, and every file there that could not be used: one
/// this darkmux cannot read, a symlink, or one whose record names another
/// endpoint than its file is for. A missing `dir` is no records.
pub fn list_in(dir: &Path) -> (Vec<EngineRecord>, Vec<UnreadableRecord>) {
    let (mut records, mut unreadable) = (Vec::new(), Vec::new());
    if let Err(why) = real_dir(dir) {
        unreadable.push(UnreadableRecord { path: dir.to_path_buf(), why });
        return (records, unreadable);
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return (records, unreadable) };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for path in paths {
        match list_one(dir, &path) {
            Ok(rec) => records.push(rec),
            Err(why) => unreadable.push(UnreadableRecord { path, why }),
        }
    }
    (records, unreadable)
}

fn list_one(dir: &Path, path: &Path) -> Result<EngineRecord, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("a symlink, which darkmux never follows".into());
    }
    let rec = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| parse(&t))?;
    if path_in(dir, &rec.endpoint) != path {
        return Err(format!("names endpoint `{}`, whose record is another file", rec.endpoint));
    }
    Ok(rec)
}

/// Remove `rec`'s file only while it still records the same server (pid and
/// start stamp), compared and removed under the record lock, so a stop never
/// deletes the record of a server another darkmux process started on that
/// endpoint since. `Ok(true)` when removed.
pub fn remove_if_in(dir: &Path, rec: &EngineRecord) -> std::io::Result<bool> {
    let _lock = record_lock(dir, &rec.endpoint)?;
    match read_in(dir, &rec.endpoint) {
        Ok(Some(now)) if now.pid == rec.pid && now.start_stamp == rec.start_stamp => {
            std::fs::remove_file(path_in(dir, &rec.endpoint))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(endpoint: &str, pid: u32, stamp: u64) -> EngineRecord {
        EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: "TensorFold/Qwen3.8-27B-MLX-4bit".into(),
            n_ctx: 262_144,
            port: 8091,
            pid,
            start_stamp: stamp,
            owner_pid: 42,
            argv: vec!["tensorfold-native".into(), "serve".into()],
            log: PathBuf::from("/tmp/tf.log"),
            started_at_ms: 1,
            engine_version: "tensorfold-native 1.2.4".into(),
        }
    }

    /// The server is darkmux's only while the pid AND its start stamp match;
    /// an exited pid and a reused one are told apart, and neither is Running.
    #[test]
    fn a_record_is_running_only_while_pid_and_start_stamp_match() {
        let r = rec("tf", 100, 5_000);
        assert_eq!(r.state(|pid| (pid == 100).then_some(5_000)), ServerState::Running);
        assert_eq!(r.state(|_| None), ServerState::Exited);
        assert_eq!(r.state(|pid| (pid == 100).then_some(9_999)), ServerState::PidReused);
    }

    #[test]
    fn a_record_round_trips_and_is_written_without_a_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = rec("tf", 100, 5_000);
        write_in(dir.path(), &r).unwrap();
        assert_eq!(read_in(dir.path(), "tf").unwrap(), Some(r.clone()));
        let mut names: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["tf.json", "tf.json.lock"], "the record and its lock, no temp file");
        assert_eq!(read_in(dir.path(), "other").unwrap(), None, "no file is no record");
        assert_eq!(list_in(&dir.path().join("missing")), (vec![], vec![]), "no directory is no records");
    }

    /// An id cannot name a path outside the directory, a hidden file, or
    /// another id's file.
    #[test]
    fn an_endpoint_id_never_escapes_the_directory_or_collides() {
        let d = Path::new("/r");
        assert_eq!(path_in(d, "tensorfold-27b.v2"), PathBuf::from("/r/tensorfold-27b.v2.json"));
        assert_eq!(path_in(d, "TF"), PathBuf::from("/r/%54%46.json"), "uppercase is escaped");
        assert_eq!(path_in(d, "../x"), PathBuf::from("/r/%2E.%2Fx.json"));
        assert_eq!(path_in(d, ".hidden"), PathBuf::from("/r/%2Ehidden.json"));
        assert_eq!(path_in(d, "a/b"), PathBuf::from("/r/a%2Fb.json"));
        assert_ne!(path_in(d, "a/b"), path_in(d, "a%2Fb"), "a literal % is escaped too");
        for id in ["../x", "a/b", "/etc/passwd", "é"] {
            let p = path_in(d, id);
            assert_eq!(p.parent(), Some(d), "{id}");
        }
    }

    /// A file this darkmux cannot use claims nothing: another schema, or not
    /// a record at all, is listed as unreadable, never as a server.
    #[test]
    fn a_record_of_another_schema_or_a_corrupt_file_claims_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_in(dir.path(), &rec("good", 1001, 1)).unwrap();
        let mut newer = serde_json::to_value(rec("newer", 1002, 2)).unwrap();
        newer["schema"] = serde_json::json!(2);
        std::fs::write(path_in(dir.path(), "newer"), newer.to_string()).unwrap();
        std::fs::write(path_in(dir.path(), "junk"), "not json").unwrap();
        let (records, unreadable) = list_in(dir.path());
        assert_eq!(records.iter().map(|r| r.endpoint.as_str()).collect::<Vec<_>>(), ["good"]);
        assert_eq!(unreadable.len(), 2);
        assert!(unreadable.iter().any(|u| u.why.contains("schema")), "{unreadable:?}");
        assert!(read_in(dir.path(), "newer").is_err());
    }

    /// A stop removes the record only while it still names the server that
    /// was stopped, never one a later start wrote.
    #[test]
    fn a_record_is_removed_only_while_it_names_the_same_server() {
        let dir = tempfile::tempdir().unwrap();
        let first = rec("tf", 100, 5_000);
        write_in(dir.path(), &first).unwrap();
        let second = rec("tf", 200, 6_000);
        write_in(dir.path(), &second).unwrap();
        assert!(!remove_if_in(dir.path(), &first).unwrap(), "a newer server's record stays");
        assert_eq!(read_in(dir.path(), "tf").unwrap(), Some(second.clone()));
        assert!(remove_if_in(dir.path(), &second).unwrap());
        assert_eq!(read_in(dir.path(), "tf").unwrap(), None);
        assert!(!remove_if_in(dir.path(), &second).unwrap(), "already gone");
    }

    /// (#3455 review) Two writers of one endpoint never share a temp file, so
    /// a write that reports `Ok` always leaves a readable record and no write
    /// fails because another renamed its temp file away.
    #[test]
    fn concurrent_writers_of_one_endpoint_never_lose_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        for round in 0..200u32 {
            let (a, b) = (d.clone(), d.clone());
            let ta = std::thread::spawn(move || write_in(&a, &rec("tf", 1001, u64::from(round))));
            let tb = std::thread::spawn(move || write_in(&b, &rec("tf", 1002, u64::from(round))));
            assert!(ta.join().unwrap().is_ok() && tb.join().unwrap().is_ok(), "round {round}: a write failed");
            let now = read_in(&d, "tf").unwrap().expect("a record survives");
            assert!(now.pid == 1001 || now.pid == 1002, "round {round}");
        }
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left behind: {leftovers:?}");
    }

    /// (#3455 review) A symlinked record file, a symlinked directory, and a
    /// planted symlink where a temp file would go are never followed.
    #[test]
    fn symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        write_in(&real, &rec("tf", 1001, 1)).unwrap();
        let linked_dir = dir.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked_dir).unwrap();
        assert!(read_in(&linked_dir, "tf").is_err(), "a symlinked directory is refused");
        assert!(list_in(&linked_dir).0.is_empty(), "and lists no records");
        assert!(write_in(&linked_dir, &rec("tf", 1002, 2)).is_err(), "and is never written through");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(path_in(&real, "tf"), path_in(&other, "tf")).unwrap();
        let err = read_in(&other, "tf").unwrap_err();
        assert!(err.why.contains("symlink"), "{err:?}");
        assert!(list_in(&other).0.is_empty());
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "precious").unwrap();
        let planted = dir.path().join("planted");
        std::fs::create_dir_all(&planted).unwrap();
        for i in 0..64 {
            let tmp = path_in(&planted, "tf").with_extension(format!("json.{}.{i}.tmp", std::process::id()));
            let _ = std::os::unix::fs::symlink(&victim, tmp);
        }
        let _ = write_in(&planted, &rec("tf", 1003, 3));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious", "a planted temp symlink is not written through");
    }

    /// (#3455 review) Ids that differ only in case get different files, so a
    /// case-insensitive disk (macOS's default) never merges them; and a file
    /// whose record names another endpoint than its own name is refused.
    #[test]
    fn ids_that_differ_in_case_never_share_a_file() {
        let dir = tempfile::tempdir().unwrap();
        write_in(dir.path(), &rec("TF", 1001, 1)).unwrap();
        assert_eq!(read_in(dir.path(), "tf").unwrap(), None, "`tf` is not `TF`");
        write_in(dir.path(), &rec("tf", 1002, 2)).unwrap();
        assert_eq!(read_in(dir.path(), "TF").unwrap().map(|r| r.pid), Some(1001));
        assert_eq!(read_in(dir.path(), "tf").unwrap().map(|r| r.pid), Some(1002));
        std::fs::copy(path_in(dir.path(), "tf"), dir.path().join("tf-backup.json")).unwrap();
        let (records, unreadable) = list_in(dir.path());
        assert_eq!(records.len(), 2, "the copy is not a third record: {records:?}");
        std::fs::copy(path_in(dir.path(), "tf"), path_in(dir.path(), "other")).unwrap();
        let err = read_in(dir.path(), "other").unwrap_err();
        assert!(err.why.contains("names endpoint `tf`"), "a copied record is not `other`'s: {err:?}");
        assert!(unreadable.iter().any(|u| u.why.contains("names endpoint")), "{unreadable:?}");
    }

    /// (#3455 review) A record naming pid 0 or 1 claims nothing, and an empty
    /// or over-long endpoint id is refused rather than written somewhere no
    /// listing finds.
    #[test]
    fn impossible_records_and_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for pid in [0, 1] {
            let bad = rec("tf", pid, 5);
            std::fs::create_dir_all(dir.path()).unwrap();
            std::fs::write(path_in(dir.path(), "tf"), serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(read_in(dir.path(), "tf").is_err(), "pid {pid}");
            assert!(write_in(dir.path(), &bad).is_err(), "pid {pid} is never written");
        }
        assert!(write_in(dir.path(), &rec("", 100, 1)).is_err(), "an empty id");
        assert!(write_in(dir.path(), &rec(&"é".repeat(60), 100, 1)).is_err(), "an id too long for a file name");
    }

    /// (#3455 review) An unreadable file is an error, never "no record"; a
    /// file that is not `.json` is not a record; and a stop compares the
    /// start stamp, not only the pid.
    #[test]
    fn io_errors_other_files_and_stamps_each_decide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(path_in(dir.path(), "tf")).unwrap();
        assert!(read_in(dir.path(), "tf").is_err(), "a directory where the record goes is not `None`");
        // A directory darkmux cannot look into: the record's state is unknown,
        // never "no record".
        let locked = tempfile::tempdir().unwrap();
        let inner = locked.path().join("engines");
        write_in(&inner, &rec("tf", 1001, 1)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o000)).unwrap();
        let state = read_in(&inner, "tf");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(state.is_err(), "an unreadable directory is not `None`: {state:?}");
        let dir = tempfile::tempdir().unwrap();
        write_in(dir.path(), &rec("tf", 1001, 1)).unwrap();
        std::fs::write(dir.path().join("tf.json.lock"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "{}").unwrap();
        assert_eq!(list_in(dir.path()), (vec![rec("tf", 1001, 1)], vec![]));
        let same_pid_older = rec("tf", 1001, 0);
        assert!(!remove_if_in(dir.path(), &same_pid_older).unwrap(), "same pid, another start: not this server");
        assert!(read_in(dir.path(), "tf").unwrap().is_some());
    }

    /// (#3455 re-review) The compare-and-remove holds the record lock, so it
    /// waits for a writer holding it; and a lock file planted as a symlink
    /// is refused rather than created through.
    #[test]
    fn remove_waits_for_the_record_lock_and_a_symlinked_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let r = rec("tf", 1001, 1);
        write_in(dir.path(), &r).unwrap();
        let held = record_lock(dir.path(), "tf").unwrap();
        let (d, r2) = (dir.path().to_path_buf(), r.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let remover = std::thread::spawn(move || tx.send(remove_if_in(&d, &r2).unwrap()).unwrap());
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "removal waits while the lock is held");
        drop(held);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), "and removes once it is released");
        remover.join().unwrap();
        let planted = tempfile::tempdir().unwrap();
        let victim = planted.path().join("victim");
        std::os::unix::fs::symlink(&victim, path_in(planted.path(), "tf").with_extension("json.lock")).unwrap();
        assert!(write_in(planted.path(), &rec("tf", 1001, 1)).is_err());
        assert!(!victim.exists(), "nothing is created through a planted lock symlink");
    }
}
