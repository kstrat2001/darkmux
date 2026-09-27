//! (#2928) The LIVE channel: sub-second model state from a running
//! execution to the local daemon's viewers, never written to the flow log.
//!
//! Two channels, like a trading chart's ticks and candles:
//!
//! - **Durable** (unchanged): `dispatch.turn.heartbeat` flow records,
//!   coalesced to one per 2 s by the host tailer. They reach the day file,
//!   Redis and the audit chain, and they are what history and playback read.
//! - **Live** (this module): samples at `runtime.live_sample_ms` (250 ms by
//!   default), sent from the dispatch process to the local `darkmux serve`
//!   daemon over a unix datagram socket, fanned out to that daemon's SSE
//!   viewers as `event: live`, and dropped. Nothing here writes a record,
//!   a file (other than the socket itself) or a Redis entry.
//!
//! The transport is chosen for the dispatch's sake. A datagram send is one
//! non-blocking syscall: a slow daemon fills its receive buffer and the send
//! fails with "would block", which counts as a drop; an absent daemon fails
//! with "not found"/"refused", which also drops and backs the sender off for
//! [`ABSENT_BACKOFF`] so a dispatch with no daemon pays nothing per sample.
//! The dispatch is never slowed and never waits.
//!
//! **Local only in 4.0.** The socket is a file under the darkmux home; only
//! a process on this machine can reach it, and the daemon forwards samples
//! only to its own SSE viewers (which may themselves be remote, e.g. the
//! operator's phone over the tailnet). A viewer attached to machine A's
//! daemon sees machine B's executions through B's durable heartbeats only.
//! Carrying live samples across machines would need a fleet transport, and
//! the only fleet transport today is the Redis stream, which is exactly what
//! this channel exists to keep samples out of.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Version of the live wire shape. A viewer ignores a sample whose `v` it
/// does not know.
pub const LIVE_WIRE_VERSION: u8 = 1;

/// The largest datagram a sender will send and a daemon will accept. macOS
/// caps a unix datagram at 2048 bytes by default (`net.local.dgram.maxdgram`);
/// a model sample is ~300 bytes.
pub const MAX_LIVE_DATAGRAM: usize = 2048;

/// How long a sender stops trying after finding no daemon (no socket, or a
/// stale socket nobody reads). Samples in that window are counted as drops
/// without a syscall.
pub const ABSENT_BACKOFF: Duration = Duration::from_secs(2);

/// The most bytes a string field may carry: ids, model names, a tool name.
const MAX_FIELD_CHARS: usize = 256;

/// What a sample describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    /// One execution's model state, heartbeat-shaped (`fields` carries the
    /// same keys a `dispatch.turn.heartbeat` payload does).
    Model,
    /// A utility job's start or end (`fields.event` is `start` | `end`).
    Utility,
}

/// One live sample, as it crosses the socket and the SSE stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSample {
    pub v: u8,
    pub kind: LiveKind,
    /// The execution the sample is about (a model sample), or the execution
    /// a utility job serves (a compaction). Absent for a routing job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix ms the sampled state was observed (the runtime's own event time
    /// for a model sample).
    pub at_ms: u64,
    /// The cadence the producer sampled at, so a reader never assumes one.
    pub cadence_ms: u64,
    /// Kind-specific fields: numbers, short strings and booleans only.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl LiveSample {
    pub fn new(kind: LiveKind, at_ms: u64, cadence_ms: u64) -> Self {
        LiveSample {
            v: LIVE_WIRE_VERSION,
            kind,
            session_id: None,
            role: None,
            model: None,
            at_ms,
            cadence_ms,
            fields: serde_json::Map::new(),
        }
    }

    /// Parse and bound a datagram. `None` for anything that is not a
    /// well-formed sample of a known version: oversize, not JSON, an unknown
    /// kind, a nested value, or a string field over [`MAX_FIELD_CHARS`].
    /// The daemon forwards only what this accepts, re-serialized, so a
    /// local process cannot push arbitrary bytes into a viewer.
    pub fn from_datagram(bytes: &[u8]) -> Option<LiveSample> {
        if bytes.len() > MAX_LIVE_DATAGRAM {
            return None;
        }
        let s: LiveSample = serde_json::from_slice(bytes).ok()?;
        if s.v != LIVE_WIRE_VERSION {
            return None;
        }
        let short = |o: &Option<String>| {
            o.as_ref()
                .is_none_or(|v| v.chars().count() <= MAX_FIELD_CHARS)
        };
        if !short(&s.session_id) || !short(&s.role) || !short(&s.model) {
            return None;
        }
        for v in s.fields.values() {
            match v {
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_) => {}
                serde_json::Value::String(t) if t.chars().count() <= MAX_FIELD_CHARS => {}
                _ => return None,
            }
        }
        Some(s)
    }

    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(self)
            .ok()
            .filter(|b| b.len() <= MAX_LIVE_DATAGRAM)
    }
}

/// Where the local daemon listens for live samples on port `port`:
/// `<darkmux-home>/run/live-<port>.sock`. Keyed by the daemon's port so a
/// second daemon (a preview on another port) never receives another's
/// samples, and a dispatch sends to the daemon its own config names.
///
/// A unix socket path is limited to ~104 bytes. When the home-based path is
/// longer (a deep test or scratch home), the socket lives in a private
/// directory [`fallback_dir`] instead. Pure: nothing is created here (only
/// the daemon's [`bind_ingest`] creates a directory, 0700). An EXISTING
/// fallback directory is used only if it is a real directory (not a
/// symlink), 0700, and owned by the home's owner; otherwise `None` (no
/// channel), never a socket another user could plant or read. Both ends
/// compute the same path.
pub fn socket_path_for(home: &Path, port: u16) -> Option<PathBuf> {
    let primary = home.join("run").join(format!("live-{port}.sock"));
    if primary.as_os_str().len() <= 100 {
        return Some(primary);
    }
    let dir = fallback_dir(home);
    if std::fs::symlink_metadata(&dir).is_ok() && !is_private_dir(&dir, home) {
        return None;
    }
    Some(dir.join(format!("live-{port}.sock"))).filter(|p| p.as_os_str().len() <= 103)
}

/// The private directory a long home's socket lives in:
/// `darkmux-live-<hash of home>/` under `$XDG_RUNTIME_DIR` when set, else the
/// system temp dir (shared `/tmp` on Linux, hence the privacy checks).
pub fn fallback_dir(home: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!(
        "darkmux-live-{:016x}",
        fnv64(home.as_os_str().as_encoded_bytes())
    ))
}

/// A real directory (not a symlink), 0700, owned by `home`'s owner (the
/// user darkmux runs as).
fn is_private_dir(dir: &Path, home: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (Ok(meta), Ok(owner)) = (
        std::fs::symlink_metadata(dir),
        std::fs::metadata(home).map(|m| m.uid()),
    ) else {
        return false;
    };
    meta.file_type().is_dir() && meta.permissions().mode() & 0o777 == 0o700 && meta.uid() == owner
}

/// (#2928 review, MF-B) Every live socket a daemon on this machine could
/// have bound for `home`, whatever port it was started on: `(port, path)`
/// for each `live-<port>.sock` in the home's `run/` and in its fallback
/// directory. `darkmux doctor` probes each, so a daemon started with a
/// `--port` that differs from `serve.port` is found.
pub fn sockets_for_home(home: &Path) -> Vec<(u16, PathBuf)> {
    let mut out = Vec::new();
    for dir in [home.join("run"), fallback_dir(home)] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(port) = name
                .to_str()
                .and_then(|n| n.strip_prefix("live-"))
                .and_then(|n| n.strip_suffix(".sock"))
                .and_then(|n| n.parse::<u16>().ok())
            else {
                continue;
            };
            out.push((port, e.path()));
        }
    }
    out.sort();
    out
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A short, stable fingerprint of a socket path, for `/health` to say WHICH
/// socket a daemon bound without publishing a filesystem path (`/health` is
/// open to the tailnet). `darkmux doctor` compares it with its own.
pub fn socket_fingerprint(path: &Path) -> String {
    format!("{:016x}", fnv64(path.as_os_str().as_encoded_bytes()))
}

/// The darkmux home the socket path derives from: the liveness dir's parent,
/// the same resolution the dispatch and the daemon both use.
pub fn live_home() -> PathBuf {
    let liveness = darkmux_types::dispatch_liveness::liveness_dir();
    liveness.parent().map(Path::to_path_buf).unwrap_or(liveness)
}

/// The socket this machine's daemon uses, from the resolved home and the
/// resolved serve port (`env > config.serve.port > 8765`, the same
/// resolution `darkmux serve` binds with). `None` when no private path is
/// available (see [`socket_path_for`]).
pub fn local_socket_path() -> Option<PathBuf> {
    socket_path_for(&live_home(), crate::daemon_probe::daemon_port())
}

/// What is at a socket path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketState {
    /// No file: no daemon has bound it.
    Absent,
    /// A socket file nobody is reading: a daemon that died without
    /// removing it (SIGKILL). Senders drop into it as "no receiver".
    Stale,
    /// A daemon is bound and receiving.
    Listening,
}

/// Probe a socket path without sending anything (a datagram `connect`).
pub fn probe_socket(path: &Path) -> SocketState {
    let Ok(s) = std::os::unix::net::UnixDatagram::unbound() else {
        return SocketState::Absent;
    };
    match s.connect(path) {
        Ok(()) => SocketState::Listening,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SocketState::Absent,
        Err(_) => SocketState::Stale,
    }
}

/// A sender's own cost and outcome, stamped into the dispatch's summary so
/// "the observer was negligible" is a number in the artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LiveSendStats {
    pub sent: u64,
    pub dropped: u64,
    /// Of `dropped`: no daemon (no socket, a stale one, or the backoff
    /// that follows), or no usable socket at all.
    pub dropped_no_receiver: u64,
    /// Of `dropped`: the daemon's receive buffer was full (a slow daemon).
    pub dropped_full: u64,
    /// Total time spent in `send` (serialization and the syscall), in ns.
    pub send_ns: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Ok,
    NoReceiver,
    Full,
    /// Refused before any syscall (oversize).
    Refused,
}

/// The dispatch side of the channel. Never blocks, never errors.
pub struct LiveSender {
    sock: Option<std::os::unix::net::UnixDatagram>,
    path: PathBuf,
    backoff_until: Option<Instant>,
    stats: LiveSendStats,
}

impl LiveSender {
    /// A sender to the socket at `path`. The socket itself is unbound and
    /// non-blocking; if it cannot be created every send counts as a drop.
    pub fn to_path(path: PathBuf) -> Self {
        let sock = std::os::unix::net::UnixDatagram::unbound()
            .ok()
            .filter(|s| s.set_nonblocking(true).is_ok());
        LiveSender {
            sock,
            path,
            backoff_until: None,
            stats: LiveSendStats::default(),
        }
    }

    /// A sender to this machine's daemon, or `None` when the live channel is
    /// off (`runtime.live_sample_ms: 0`).
    pub fn for_local_daemon() -> Option<Self> {
        if !darkmux_types::config_access::live_cadence().enabled() {
            return None;
        }
        local_socket_path().map(Self::to_path)
    }

    /// Send one sample. Returns whether it was handed to the daemon's socket.
    /// A full receive buffer, a missing daemon or an oversize sample is a
    /// drop, counted; the caller carries on either way.
    pub fn send(&mut self, sample: &LiveSample) -> bool {
        let t0 = Instant::now();
        let outcome = self.send_inner(sample, t0);
        self.stats.send_ns = self
            .stats
            .send_ns
            .saturating_add(t0.elapsed().as_nanos() as u64);
        match outcome {
            Sent::Ok => self.stats.sent += 1,
            Sent::NoReceiver => {
                self.stats.dropped += 1;
                self.stats.dropped_no_receiver += 1;
            }
            Sent::Full => {
                self.stats.dropped += 1;
                self.stats.dropped_full += 1;
            }
            Sent::Refused => self.stats.dropped += 1,
        }
        outcome == Sent::Ok
    }

    fn send_inner(&mut self, sample: &LiveSample, now: Instant) -> Sent {
        if self.backoff_until.is_some_and(|t| now < t) {
            return Sent::NoReceiver;
        }
        let Some(sock) = self.sock.as_ref() else {
            return Sent::NoReceiver;
        };
        let Some(bytes) = sample.to_bytes() else {
            return Sent::Refused;
        };
        match sock.send_to(&bytes, &self.path) {
            Ok(_) => {
                self.backoff_until = None;
                self.stats.bytes = self.stats.bytes.saturating_add(bytes.len() as u64);
                Sent::Ok
            }
            // No daemon (no socket, or a stale one): back off.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                self.backoff_until = Some(now + ABSENT_BACKOFF);
                Sent::NoReceiver
            }
            // A full buffer (a slow daemon, EAGAIN/ENOBUFS): just this sample
            // is lost; the next may fit.
            Err(_) => Sent::Full,
        }
    }

    pub fn stats(&self) -> LiveSendStats {
        self.stats
    }
}

/// The daemon side: bind the ingest socket at `path`, replacing a stale one.
/// Call only after the daemon's TCP port is bound, so the socket being
/// replaced can only be a dead daemon's (a live one would still hold the
/// port). Owner-only permissions.
pub fn bind_ingest(path: &Path) -> std::io::Result<std::os::unix::net::UnixDatagram> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        // Owner-only: the fallback directory may sit in a shared /tmp.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let sock = std::os::unix::net::UnixDatagram::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_sample(at: u64) -> LiveSample {
        let mut s = LiveSample::new(LiveKind::Model, at, 250);
        s.session_id = Some("sess-1".into());
        s.fields
            .insert("generated_chars".into(), serde_json::json!(120));
        s
    }

    #[test]
    fn a_sample_round_trips_and_malformed_input_is_refused() {
        let s = model_sample(1_000);
        let bytes = s.to_bytes().unwrap();
        assert_eq!(LiveSample::from_datagram(&bytes), Some(s.clone()));
        assert_eq!(LiveSample::from_datagram(b"not json"), None);
        let mut bad = s.clone();
        bad.v = 9;
        assert_eq!(
            LiveSample::from_datagram(&bad.to_bytes().unwrap()),
            None,
            "an unknown version"
        );
        let mut nested = s.clone();
        nested
            .fields
            .insert("x".into(), serde_json::json!({"deep": 1}));
        assert_eq!(
            LiveSample::from_datagram(&serde_json::to_vec(&nested).unwrap()),
            None,
            "no nested values"
        );
        let mut long = s.clone();
        long.model = Some("m".repeat(MAX_FIELD_CHARS + 1));
        assert_eq!(
            LiveSample::from_datagram(&serde_json::to_vec(&long).unwrap()),
            None,
            "bounded strings"
        );
        let raw =
            serde_json::json!({"v":1,"kind":"exfiltrate","at_ms":1,"cadence_ms":250,"fields":{}});
        assert_eq!(
            LiveSample::from_datagram(raw.to_string().as_bytes()),
            None,
            "an unknown kind"
        );
        assert_eq!(
            LiveSample::from_datagram(&vec![b' '; MAX_LIVE_DATAGRAM + 1]),
            None,
            "oversize"
        );
    }

    #[test]
    fn a_long_home_moves_the_socket_to_a_private_dir_deterministically() {
        let short = socket_path_for(Path::new("/Users/x/.darkmux"), 8765);
        assert_eq!(
            short,
            Some(PathBuf::from("/Users/x/.darkmux/run/live-8765.sock"))
        );
        // A short base, as a real runtime or temp dir is (a socket path is
        // limited to ~104 bytes).
        let base = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let home_root = tempfile::tempdir().unwrap();
        let deep = home_root.path().join("d".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", base.path()) };
        let a = socket_path_for(&deep, 8765).expect("a private path");
        let created_early = a.parent().unwrap().exists();
        let _bound = bind_ingest(&a).expect("the daemon creates the private dir");
        let again = socket_path_for(&deep, 8765);
        let listed = sockets_for_home(&deep);
        let other_port = socket_path_for(&deep, 8766);
        // A directory someone else prepared at the name a second home hashes
        // to: world-readable, so refused.
        let deep2 = home_root.path().join("e".repeat(120));
        std::fs::create_dir_all(&deep2).unwrap();
        let planted = base.path().join(format!(
            "darkmux-live-{:016x}",
            fnv64(deep2.as_os_str().as_encoded_bytes())
        ));
        std::fs::create_dir(&planted).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        let refused = socket_path_for(&deep2, 8765);
        // A symlink planted at the name is refused too.
        let deep3 = home_root.path().join("f".repeat(120));
        std::fs::create_dir_all(&deep3).unwrap();
        let link = base.path().join(format!(
            "darkmux-live-{:016x}",
            fnv64(deep3.as_os_str().as_encoded_bytes())
        ));
        let target = tempfile::tempdir().unwrap();
        std::fs::set_permissions(target.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        let via_link = socket_path_for(&deep3, 8765);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
        assert!(!created_early, "computing the path creates nothing");
        assert_eq!(
            listed,
            vec![(8765, a.clone())],
            "the daemon's socket is found by listing"
        );
        assert!(a.starts_with(base.path()), "under XDG_RUNTIME_DIR: {a:?}");
        let dir_mode = std::fs::metadata(a.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "a private directory");
        assert_eq!(again, Some(a.clone()), "both ends compute the same path");
        assert_ne!(other_port, Some(a), "keyed by port");
        assert_eq!(refused, None, "a directory that is not 0700 is never used");
        assert_eq!(via_link, None, "a symlink is never followed");
    }

    #[test]
    fn a_socket_nobody_reads_probes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        assert_eq!(probe_socket(&path), SocketState::Absent);
        let rx = bind_ingest(&path).unwrap();
        assert_eq!(probe_socket(&path), SocketState::Listening);
        drop(rx); // a SIGKILLed daemon leaves its socket file behind
        assert!(path.exists());
        assert_eq!(probe_socket(&path), SocketState::Stale);
        let mut tx = LiveSender::to_path(path);
        assert!(!tx.send(&model_sample(1)));
        assert_eq!(
            tx.stats().dropped_no_receiver,
            1,
            "a stale socket is a no-receiver drop"
        );
    }

    #[test]
    fn samples_reach_a_bound_ingest_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let rx = bind_ingest(&path).unwrap();
        let mut tx = LiveSender::to_path(path.clone());
        assert!(tx.send(&model_sample(1)));
        let mut buf = [0u8; MAX_LIVE_DATAGRAM];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(LiveSample::from_datagram(&buf[..n]), Some(model_sample(1)));
        let st = tx.stats();
        assert_eq!((st.sent, st.dropped), (1, 0));
        assert!(st.bytes > 0 && st.send_ns > 0);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// A daemon that never reads: the sender drops rather than blocks.
    #[test]
    fn a_full_receiver_drops_samples_and_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let _rx = bind_ingest(&path).unwrap();
        let mut tx = LiveSender::to_path(path);
        let t0 = Instant::now();
        for i in 0..5_000 {
            tx.send(&model_sample(i));
        }
        let elapsed = t0.elapsed();
        let st = tx.stats();
        assert!(
            st.dropped_full > 0 && st.dropped_no_receiver == 0,
            "a full buffer is its own cause: {st:?}"
        );
        assert!(
            st.dropped > 0,
            "the buffer filled and later samples were dropped: {st:?}"
        );
        assert_eq!(st.sent + st.dropped, 5_000);
        assert!(
            elapsed < Duration::from_secs(2),
            "5000 sends into a full buffer returned promptly: {elapsed:?}"
        );
    }

    /// No daemon: one failed syscall, then a backoff window of free drops.
    #[test]
    fn an_absent_daemon_backs_the_sender_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut tx = LiveSender::to_path(dir.path().join("nobody.sock"));
        assert!(!tx.send(&model_sample(1)));
        assert!(
            tx.backoff_until.is_some(),
            "a missing socket starts the backoff"
        );
        for i in 0..100 {
            assert!(!tx.send(&model_sample(i)));
        }
        assert_eq!(tx.stats().dropped, 101);
        assert_eq!(
            tx.stats().dropped_no_receiver,
            101,
            "no daemon, during the backoff too"
        );
        // A daemon appearing is picked up once the window passes.
        let path = dir.path().join("nobody.sock");
        let rx = bind_ingest(&path).unwrap();
        tx.backoff_until = Some(Instant::now() - Duration::from_millis(1));
        assert!(tx.send(&model_sample(7)));
        let mut buf = [0u8; MAX_LIVE_DATAGRAM];
        assert!(rx.recv(&mut buf).is_ok());
    }

    #[test]
    fn an_oversize_sample_is_a_drop_not_a_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let _rx = bind_ingest(&path).unwrap();
        let mut tx = LiveSender::to_path(path);
        let mut big = model_sample(1);
        for i in 0..40 {
            big.fields
                .insert(format!("k{i}"), serde_json::json!("x".repeat(200)));
        }
        assert!(
            big.to_bytes().is_none(),
            "refused before any syscall, on every platform"
        );
        assert!(!tx.send(&big));
        assert_eq!(tx.stats().dropped, 1);
    }
}
