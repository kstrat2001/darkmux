//! Where the running `darkmux serve` daemon actually bound (#3007).
//!
//! The daemon's address can differ from what config resolves: `darkmux serve
//! --port 8766` beats `serve.port`, and only the daemon knows it. So the
//! daemon writes ONE small record, `<darkmux-home>/run/daemon.json`
//! (`{"pid", "host", "port"}`), once it has bound, and removes it on clean
//! shutdown. Every client on this machine resolves the daemon's address
//! through `config_access::serve_client_endpoint`, which prefers a live
//! record over env, config and the built-in default.
//!
//! A record whose pid is not alive (the daemon was SIGKILLed) is ignored, and
//! a daemon only ever removes a record it wrote itself (matching pid), so a
//! second daemon started on another port is not unrecorded by the first one
//! exiting.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The bound address of a running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub pid: u32,
    /// The IP the daemon bound (a wildcard stays a wildcard; clients collapse
    /// it to loopback via `config_access::format_client_addr`).
    pub host: String,
    pub port: u16,
}

/// `<darkmux-home>/run/daemon.json`.
pub fn record_path() -> PathBuf {
    record_path_in(&crate::paths::user_root_guarded())
}

fn record_path_in(home: &Path) -> PathBuf {
    home.join("run").join("daemon.json")
}

/// The record of a daemon that is still alive, or `None` (no file, an
/// unreadable one, or a pid that is gone).
pub fn live() -> Option<DaemonRecord> {
    live_in(&crate::paths::user_root_guarded())
}

fn live_in(home: &Path) -> Option<DaemonRecord> {
    let text = std::fs::read_to_string(record_path_in(home)).ok()?;
    let rec: DaemonRecord = serde_json::from_str(&text).ok()?;
    crate::residency_lease::process_alive(rec.pid).then_some(rec)
}

/// Removes this process's record when dropped (clean shutdown or unwind).
#[derive(Debug)]
pub struct DaemonRecordGuard {
    home: PathBuf,
    pid: u32,
}

/// Record that this process is the daemon, bound at `host:port`. Returns a
/// guard that removes the record on drop. Best-effort: a home that cannot be
/// written leaves clients on the env/config/default tiers, as before.
pub fn publish(host: &str, port: u16) -> DaemonRecordGuard {
    publish_in(&crate::paths::user_root_guarded(), host, port)
}

fn publish_in(home: &Path, host: &str, port: u16) -> DaemonRecordGuard {
    let pid = std::process::id();
    let rec = DaemonRecord { pid, host: host.to_string(), port };
    let path = record_path_in(home);
    let written = path.parent().map(std::fs::create_dir_all).transpose().is_ok()
        && serde_json::to_vec(&rec).is_ok_and(|bytes| {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path)).is_ok()
        });
    if !written {
        eprintln!("darkmux serve: could not record the bound address at {}; clients will resolve it from env/config", path.display());
    }
    DaemonRecordGuard { home: home.to_path_buf(), pid }
}

impl Drop for DaemonRecordGuard {
    fn drop(&mut self) {
        let path = record_path_in(&self.home);
        let ours = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<DaemonRecord>(&t).ok())
            .is_some_and(|r| r.pid == self.pid);
        if ours {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_raw(home: &Path, rec: &DaemonRecord) {
        let p = record_path_in(home);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, serde_json::to_vec(rec).unwrap()).unwrap();
    }

    #[test]
    fn a_published_record_is_read_back_and_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = publish_in(tmp.path(), "127.0.0.1", 8766);
        let rec = live_in(tmp.path()).expect("a live record");
        assert_eq!((rec.host.as_str(), rec.port, rec.pid), ("127.0.0.1", 8766, std::process::id()));
        drop(guard);
        assert_eq!(live_in(tmp.path()), None, "clean shutdown removes the record");
    }

    #[test]
    fn a_record_with_a_dead_pid_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        // A child that has exited and been reaped: its pid is not alive.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        write_raw(tmp.path(), &DaemonRecord { pid: dead, host: "127.0.0.1".into(), port: 8766 });
        assert_eq!(live_in(tmp.path()), None);
    }

    #[test]
    fn no_record_and_a_garbled_record_read_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(live_in(tmp.path()), None);
        let p = record_path_in(tmp.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "not json").unwrap();
        assert_eq!(live_in(tmp.path()), None);
    }

    #[test]
    fn a_guard_never_removes_a_record_another_process_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = publish_in(tmp.path(), "127.0.0.1", 8766);
        // A second daemon (another pid) took over the record.
        write_raw(tmp.path(), &DaemonRecord { pid: std::process::id() + 1, host: "127.0.0.1".into(), port: 9000 });
        drop(guard);
        assert!(record_path_in(tmp.path()).exists(), "the newer daemon's record survives");
    }
}
