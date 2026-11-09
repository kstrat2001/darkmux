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

/// The file an endpoint's record lives in. An endpoint id is the
/// operator's text, so every byte outside `[A-Za-z0-9._-]`, and a leading
/// `.`, is written `%XX`: no id can name a path outside `dir` or a hidden
/// file, and two ids never share a file.
pub fn path_in(dir: &Path, endpoint: &str) -> PathBuf {
    let mut name = String::new();
    for (i, b) in endpoint.bytes().enumerate() {
        let plain = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || (b == b'.' && i > 0);
        if plain {
            name.push(b as char);
        } else {
            name.push_str(&format!("%{b:02X}"));
        }
    }
    dir.join(format!("{name}.json"))
}

/// Write `rec` atomically (a temp file renamed over the record), creating
/// `dir` as needed.
pub fn write_in(dir: &Path, rec: &EngineRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = path_in(dir, &rec.endpoint);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(rec).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

/// The record for `endpoint`: `Ok(None)` when there is none, `Err` when the
/// file is there but this darkmux cannot use it.
pub fn read_in(dir: &Path, endpoint: &str) -> Result<Option<EngineRecord>, UnreadableRecord> {
    let path = path_in(dir, endpoint);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(UnreadableRecord { path, why: e.to_string() }),
        Ok(text) => parse(&text).map(Some).map_err(|why| UnreadableRecord { path, why }),
    }
}

fn parse(text: &str) -> Result<EngineRecord, String> {
    let doc: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match doc.get("schema").and_then(|s| s.as_u64()) {
        Some(s) if s == u64::from(ENGINE_RECORD_SCHEMA) => {}
        other => return Err(format!("record schema {other:?}; this darkmux reads {ENGINE_RECORD_SCHEMA}")),
    }
    serde_json::from_value(doc).map_err(|e| e.to_string())
}

/// Every record in `dir`, and every file there that could not be used.
/// A missing `dir` is no records.
pub fn list_in(dir: &Path) -> (Vec<EngineRecord>, Vec<UnreadableRecord>) {
    let (mut records, mut unreadable) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else { return (records, unreadable) };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Ok(rec) => records.push(rec),
            Err(why) => unreadable.push(UnreadableRecord { path, why }),
        }
    }
    (records, unreadable)
}

/// Remove `rec`'s file only while it still records the same server (pid and
/// start stamp), so a stop never deletes the record of a server another
/// darkmux process started on that endpoint since. `Ok(true)` when removed.
pub fn remove_if_in(dir: &Path, rec: &EngineRecord) -> std::io::Result<bool> {
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
        let names: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["tf.json"]);
        assert_eq!(read_in(dir.path(), "other").unwrap(), None, "no file is no record");
        assert_eq!(list_in(&dir.path().join("missing")), (vec![], vec![]), "no directory is no records");
    }

    /// An id cannot name a path outside the directory, a hidden file, or
    /// another id's file.
    #[test]
    fn an_endpoint_id_never_escapes_the_directory_or_collides() {
        let d = Path::new("/r");
        assert_eq!(path_in(d, "tensorfold-27b.v2"), PathBuf::from("/r/tensorfold-27b.v2.json"));
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
        write_in(dir.path(), &rec("good", 1, 1)).unwrap();
        let mut newer = serde_json::to_value(rec("newer", 2, 2)).unwrap();
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
}
