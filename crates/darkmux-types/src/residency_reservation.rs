//! Memory a start has taken in the machine's budget before the OS shows it
//! as used: one record per start, apart from every residency lease.
//!
//! A start's memory pages in only after its server is spawned or its load
//! begins, so a decision made meanwhile reads free memory the start is
//! about to take ([`crate::residency_lock`]). A reservation is that start's
//! claim on it: every memory decision subtracts [`reserved`] from the free
//! figure it reads.
//!
//! A reservation is not a lease. A lease
//! ([`crate::residency_lease::LeaseGuard`]) says a running dispatch holds a
//! model, which pins it: it is never evicted, and its endpoint is `busy` for
//! another command. A reservation only subtracts from free memory: it never
//! pins a model, and never makes one read as held. The two end apart, too:
//! the lease when its dispatch ends, the reservation when the start's memory
//! has paged in.
//!
//! # One record per start
//!
//! `<darkmux-home>/run/reservations/<pid>-<start stamp>-<n>.json`, named for
//! the darkmux process that made the start (its owner) and a counter, so no
//! two starts ever share a record: several starts of one dispatch (a local
//! track's waves) each have their own, and ending or shrinking one never
//! touches another. Only the owner writes a record.
//!
//! # Whose memory, and when it ends
//!
//! - **An engine server's start** is bound to the server
//!   ([`Reserved::bind_server`]) once it is spawned: the memory is the
//!   server's, whichever darkmux process started it. The record's bytes are
//!   the model's whole need, and what it reserves is read at decision time:
//!   the need less the server's footprint now. It ends when the footprint
//!   reaches the need, when the server is gone, or at the record's deadline
//!   (a footprint that never reaches the need must not reserve forever). It
//!   does not end when its owner does: a dispatch interrupted mid-start
//!   leaves the server loading, and its memory stays counted.
//! - **Any other start** (an LM Studio load: LM Studio names no process per
//!   model) is bound to its owner: it ends when the owner drops it
//!   ([`Reserved`]'s `Drop`: the load failed, or settled), when the owner
//!   process is gone, or at its deadline. The owner shrinks it as the load
//!   pages in ([`Reserved::shrink_to`]), and only ever downward.
//!
//! A reader removes a record that has ended and that nobody will write
//! again: a server's, or one whose owner process is gone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The record shape this darkmux writes and reads. A record of another
/// shape claims nothing.
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    schema: u32,
    /// Bytes: the whole need of a server's start; what is left to page in
    /// of any other start.
    need_bytes: u64,
    /// The darkmux process that made the start.
    owner_pid: u32,
    /// Its start stamp ([`crate::residency_lease::process_start_stamp`]),
    /// when it could be read: tells the owner from a later process under
    /// its pid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_started: Option<u64>,
    /// The engine server whose memory this is, once it is spawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server: Option<ServerRef>,
    /// Epoch milliseconds after which it reserves nothing. `None` while an
    /// owner-bound start is in flight: its owner ends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ServerRef {
    pid: u32,
    start_stamp: u64,
}

/// What a record comes to at one reading: the bytes it reserves now, and
/// whether it has ended with nobody left to write it (it is then removed).
/// One that reserves nothing and has not ended is its live owner's to end.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    bytes: u64,
    ended: bool,
}

const ENDED: Reading = Reading { bytes: 0, ended: true };

impl Record {
    fn read(&self, now_ms: u64, stamp_of: &dyn Fn(u32) -> Option<u64>, servers: &Servers<'_>) -> Reading {
        let expired = self.expires_at_ms.is_some_and(|at| now_ms >= at);
        if let Some(server) = self.server {
            if expired || !(servers.alive)(server.pid, server.start_stamp) {
                return ENDED;
            }
            // An unreadable footprint keeps the whole need.
            let bytes = self.need_bytes.saturating_sub((servers.footprint)(server.pid).unwrap_or(0));
            return Reading { bytes, ended: bytes == 0 };
        }
        let owner_alive = match (self.owner_started, stamp_of(self.owner_pid)) {
            (Some(stamped), Some(now)) => stamped == now,
            // No stamp to compare: alive while the pid is, the direction
            // that never drops a live start's reservation.
            (None, _) => crate::residency_lease::process_alive(self.owner_pid),
            (Some(_), None) => false,
        };
        if !owner_alive {
            return ENDED;
        }
        Reading { bytes: if expired { 0 } else { self.need_bytes }, ended: false }
    }
}

/// `<darkmux-home>/run/reservations`.
pub fn dir() -> PathBuf {
    crate::paths::user_root_guarded().join("run").join("reservations")
}

/// Epoch milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How a reader asks about the engine servers reservations are bound to.
pub struct Servers<'a> {
    /// Whether the process `pid` is still the server that started with
    /// this start stamp, and has not exited. A caller that can tell an
    /// exited server its parent has not reaped yet (it still has its pid and
    /// stamp) says so here; [`same_process`] cannot.
    pub alive: &'a dyn Fn(u32, u64) -> bool,
    /// A server's memory footprint now, bytes; `None` when unreadable.
    pub footprint: &'a dyn Fn(u32) -> Option<u64>,
}

/// Whether `pid` is a process that started at `start_stamp`.
pub fn same_process(pid: u32, start_stamp: u64) -> bool {
    crate::residency_lease::process_start_stamp(pid) == Some(start_stamp)
}

/// The memory every start still paging in reserves now, bytes, on this
/// machine.
pub fn reserved(servers: &Servers<'_>) -> u64 {
    reserved_except(None, servers)
}

/// [`reserved`], less what the engine server `except` (a pid) still
/// reserves: for a decision that stops that server, whose memory not yet
/// paged in never will be. Its record is left as it is.
pub fn reserved_except(except: Option<u32>, servers: &Servers<'_>) -> u64 {
    sum(now_ms(), &crate::residency_lease::process_start_stamp, servers, except)
}

/// [`reserved`] at `now_ms`, with `stamp_of` reading an owner process's
/// start stamp (`None`: no such process), so a test sets the clock and
/// which owners are alive.
pub fn reserved_at(now_ms: u64, stamp_of: &dyn Fn(u32) -> Option<u64>, servers: &Servers<'_>) -> u64 {
    sum(now_ms, stamp_of, servers, None)
}

fn sum(now_ms: u64, stamp_of: &dyn Fn(u32) -> Option<u64>, servers: &Servers<'_>, except: Option<u32>) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir()) else { return 0 };
    let mut total = 0u64;
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Unreadable, or another shape: it claims nothing.
        let Some(rec) = read_record(&path) else { continue };
        if except.is_some() && rec.server.map(|s| s.pid) == except {
            continue;
        }
        let reading = rec.read(now_ms, stamp_of, servers);
        total = total.saturating_add(reading.bytes);
        if reading.ended {
            let _ = std::fs::remove_file(&path);
        }
    }
    total
}

fn read_record(path: &Path) -> Option<Record> {
    let rec: Record = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (rec.schema == SCHEMA).then_some(rec)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// One start's reservation, in its owner's hands. Dropping it ends it,
/// unless it was left to its server ([`Reserved::leave_to_server`]).
#[derive(Debug)]
pub struct Reserved {
    path: PathBuf,
    rec: Record,
    left: bool,
}

impl Reserved {
    /// Reserve `need_bytes` for a start this process is about to make.
    /// Written under the residency lock, before the start is spawned or the
    /// load begins ([`crate::residency_lock`]'s protocol).
    pub fn begin(need_bytes: u64) -> Result<Self> {
        let owner_pid = std::process::id();
        let owner_started = crate::residency_lease::process_start_stamp(owner_pid);
        let name = format!("{owner_pid}-{}-{}.json", owner_started.unwrap_or(0), NEXT.fetch_add(1, Ordering::SeqCst));
        let reserved = Reserved {
            path: dir().join(name),
            rec: Record { schema: SCHEMA, need_bytes, owner_pid, owner_started, server: None, expires_at_ms: None },
            left: false,
        };
        reserved.write()?;
        Ok(reserved)
    }

    /// The bytes it was last written with.
    pub fn need_bytes(&self) -> u64 {
        self.rec.need_bytes
    }

    /// The start's server is spawned: the memory is that process's from
    /// here, whatever becomes of this one, until `expires_at_ms`.
    pub fn bind_server(&mut self, pid: u32, start_stamp: u64, expires_at_ms: u64) -> Result<()> {
        self.rec.server = Some(ServerRef { pid, start_stamp });
        self.rec.expires_at_ms = Some(expires_at_ms);
        self.write()
    }

    /// The model answers: what has not paged in stays reserved until
    /// `expires_at_ms` at the latest.
    pub fn settle_by(&mut self, expires_at_ms: u64) -> Result<()> {
        self.rec.expires_at_ms = Some(expires_at_ms);
        self.write()
    }

    /// Reserve `bytes` from now, when that is less than it reserves: a
    /// reservation only shrinks, so a later, lower reading of what paged in
    /// (the model was unloaded) never brings it back.
    pub fn shrink_to(&mut self, bytes: u64) {
        if bytes < self.rec.need_bytes {
            self.rec.need_bytes = bytes;
            let _ = self.write();
        }
    }

    /// Leave the record to the server it is bound to: readers end it. One
    /// bound to no server ends here, as a drop ends it.
    pub fn leave_to_server(mut self) {
        self.left = self.rec.server.is_some();
    }

    fn write(&self) -> Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.rec).context("serializing a reservation")?;
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("renaming a reservation into place at {}", self.path.display()))
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        if !self.left {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// `DARKMUX_HOME` pointed at a fresh directory for the test.
    struct Home {
        _dir: tempfile::TempDir,
        prev: Option<String>,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var("DARKMUX_HOME").ok();
            unsafe { std::env::set_var("DARKMUX_HOME", dir.path()) };
            Home { _dir: dir, prev }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    fn records() -> usize {
        std::fs::read_dir(dir()).map(|d| d.count()).unwrap_or(0)
    }

    /// Servers alive as [`same_process`] reads them, with this footprint.
    fn fp(footprint: &dyn Fn(u32) -> Option<u64>) -> Servers<'_> {
        Servers { alive: &same_process, footprint }
    }

    /// Servers `alive` says are alive, their footprint unreadable.
    fn servers(alive: &dyn Fn(u32, u64) -> bool) -> Servers<'_> {
        Servers { alive, footprint: &|_| None }
    }

    /// Every process alive, under the stamp its pid doubles as.
    fn all_alive(pid: u32) -> Option<u64> {
        Some(u64::from(pid))
    }

    /// A record forged as another process's, so a test decides whether that
    /// owner is alive.
    fn forged(owner_pid: u32, need_bytes: u64, server: Option<ServerRef>, expires_at_ms: Option<u64>) -> PathBuf {
        let rec = Record { schema: SCHEMA, need_bytes, owner_pid, owner_started: Some(u64::from(owner_pid)), server, expires_at_ms };
        std::fs::create_dir_all(dir()).unwrap();
        let path = dir().join(format!("{owner_pid}-{owner_pid}-1.json"));
        std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
        path
    }

    /// A start in flight reserves its need until its owner drops it.
    #[test]
    #[serial_test::serial]
    fn a_start_reserves_until_its_owner_drops_it() {
        let _home = Home::new();
        assert_eq!(reserved(&fp(&|_| None)), 0, "no directory yet reserves nothing");
        let start = Reserved::begin(30 * GIB).unwrap();
        assert_eq!((start.need_bytes(), reserved(&fp(&|_| None))), (30 * GIB, 30 * GIB));
        drop(start);
        assert_eq!((reserved(&fp(&|_| None)), records()), (0, 0));
    }

    /// Each start has its own record: shrinking or ending one never touches
    /// another, though one dispatch made both.
    #[test]
    #[serial_test::serial]
    fn one_starts_reservation_never_touches_anothers() {
        let _home = Home::new();
        let mut first = Reserved::begin(20 * GIB).unwrap();
        let second = Reserved::begin(40 * GIB).unwrap();
        first.shrink_to(5 * GIB);
        assert_eq!(reserved(&fp(&|_| None)), 45 * GIB);
        drop(first);
        assert_eq!(reserved(&fp(&|_| None)), 40 * GIB, "the second start's is whole");
        drop(second);
    }

    /// A reservation only shrinks: a later, lower reading of what paged in
    /// never brings it back.
    #[test]
    #[serial_test::serial]
    fn a_reservation_only_shrinks() {
        let _home = Home::new();
        let mut start = Reserved::begin(20 * GIB).unwrap();
        start.shrink_to(5 * GIB);
        start.shrink_to(15 * GIB);
        assert_eq!((start.need_bytes(), reserved(&fp(&|_| None))), (5 * GIB, 5 * GIB));
    }

    /// An owner-bound start ends with its owner process, and is removed; one
    /// past its deadline reserves nothing, and is its live owner's to remove.
    #[test]
    #[serial_test::serial]
    fn an_owner_bound_start_ends_with_its_owner_or_at_its_deadline() {
        let _home = Home::new();
        let path = forged(41, 30 * GIB, None, Some(1_000));
        assert_eq!(reserved_at(999, &all_alive, &fp(&|_| None)), 30 * GIB);
        assert_eq!(reserved_at(1_000, &all_alive, &fp(&|_| None)), 0, "at its deadline");
        assert!(path.exists(), "its live owner may still write it");
        assert_eq!(reserved_at(999, &|_| None, &fp(&|_| None)), 0, "its owner is gone");
        assert!(!path.exists(), "removed");
        let reused = forged(41, 30 * GIB, None, None);
        assert_eq!(reserved_at(0, &|_| Some(7), &fp(&|_| None)), 0, "another process has its owner's pid");
        assert!(!reused.exists());
    }

    /// A server's start reserves the need less the server's footprint, read
    /// at decision time, for as long as the server lives: also after its
    /// owner is gone (a dispatch interrupted mid-start leaves the server
    /// loading). It ends when the footprint reaches the need, when the
    /// server is gone, or at its deadline.
    #[test]
    #[serial_test::serial]
    fn a_servers_start_reserves_what_has_not_paged_in_whatever_becomes_of_its_owner() {
        let _home = Home::new();
        let server = ServerRef { pid: 77, start_stamp: 77 };
        let owner_gone = |_: u32| None;
        let up = |pid: u32, stamp: u64| (pid, stamp) == (77, 77);
        let paged = |bytes: u64| move |_: u32| Some(bytes);
        let path = forged(41, 40 * GIB, Some(server), Some(10_000));
        assert_eq!(reserved_at(0, &owner_gone, &servers(&up)), 40 * GIB, "an unreadable footprint keeps the whole need");
        assert_eq!(reserved_at(0, &owner_gone, &Servers { alive: &up, footprint: &paged(10 * GIB) }), 30 * GIB);
        assert!(path.exists());
        assert_eq!(reserved_at(0, &owner_gone, &Servers { alive: &up, footprint: &paged(40 * GIB) }), 0, "paged in");
        assert!(!path.exists(), "removed once it ended");
        let path = forged(41, 40 * GIB, Some(server), Some(10_000));
        assert_eq!(reserved_at(10_000, &all_alive, &servers(&up)), 0, "at its deadline");
        assert!(!path.exists());
        let path = forged(41, 40 * GIB, Some(server), Some(10_000));
        assert_eq!(reserved_at(0, &all_alive, &servers(&|_, _| false)), 0, "the server is gone, though its owner lives");
        assert!(!path.exists());
        assert!(same_process(std::process::id(), crate::residency_lease::process_start_stamp(std::process::id()).unwrap()));
        assert!(!same_process(std::process::id(), 1), "another start stamp is another process");
    }

    /// A decision that stops a server leaves out what that server still
    /// reserves, and leaves its record alone.
    #[test]
    #[serial_test::serial]
    fn a_decision_that_stops_a_server_leaves_out_its_reservation() {
        let _home = Home::new();
        let me = std::process::id();
        let stamp = crate::residency_lease::process_start_stamp(me).expect("a start stamp");
        let mut start = Reserved::begin(30 * GIB).unwrap();
        start.bind_server(me, stamp, now_ms() + 60_000).unwrap();
        let other = Reserved::begin(5 * GIB).unwrap();
        assert_eq!(reserved_except(Some(me), &fp(&|_| None)), 5 * GIB);
        assert_eq!(reserved_except(Some(me + 1), &fp(&|_| None)), 35 * GIB, "another server's is counted");
        assert_eq!(reserved(&fp(&|_| None)), 35 * GIB, "its record is as it was");
        drop((start, other));
    }

    /// Binding a start to its server and leaving it there keeps the record
    /// past its owner's handle; one bound to no server ends when left.
    #[test]
    #[serial_test::serial]
    fn a_start_left_to_its_server_outlives_its_owners_handle() {
        let _home = Home::new();
        let me = std::process::id();
        let stamp = crate::residency_lease::process_start_stamp(me).expect("a start stamp");
        let mut start = Reserved::begin(30 * GIB).unwrap();
        start.bind_server(me, stamp, now_ms() + 60_000).unwrap();
        start.settle_by(now_ms() + 30_000).unwrap();
        start.leave_to_server();
        assert_eq!(reserved(&fp(&|_| Some(GIB))), 29 * GIB);
        Reserved::begin(30 * GIB).unwrap().leave_to_server();
        assert_eq!((reserved(&fp(&|_| Some(GIB))), records()), (29 * GIB, 1), "one bound to no server ended");
    }

    /// A record this darkmux cannot read claims nothing, and is left alone.
    #[test]
    #[serial_test::serial]
    fn an_unreadable_record_claims_nothing() {
        let _home = Home::new();
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(dir().join("1-1-1.json"), "not json").unwrap();
        std::fs::write(dir().join("1-1-2.json"), r#"{"schema":2,"need_bytes":5,"owner_pid":1}"#).unwrap();
        std::fs::write(dir().join("notes.txt"), "x").unwrap();
        assert_eq!((reserved_at(0, &all_alive, &fp(&|_| None)), records()), (0, 3));
    }
}
