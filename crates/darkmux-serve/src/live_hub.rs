//! (#2928) The daemon's half of the live channel: receive samples from this
//! machine's dispatch processes and fan them out to every open SSE viewer.
//!
//! Receive: one std thread blocks on the unix datagram socket
//! (`darkmux_flow::live::local_socket_path`), accepts only what
//! `LiveSample::from_datagram` accepts, and re-serializes it, so a local
//! process can never push arbitrary bytes into a viewer.
//!
//! Fan-out: a bounded `tokio::sync::broadcast` channel. A viewer that falls
//! behind skips the samples it missed (`Lagged`) instead of holding the
//! others back or growing a queue; a live sample is worth nothing late.
//!
//! **No persistence, by construction.** Nothing in this module calls
//! `darkmux_flow::record`, opens a flow file, touches Redis or the audit
//! chain, or keeps a history. A sample exists in the socket buffer, in the
//! broadcast ring (at most [`HUB_CAPACITY`] of them, overwritten), and on the
//! wire to viewers that are connected at that moment. A viewer that connects
//! later, and playback, see only durable heartbeats.

use axum::response::sse::Event;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

/// Samples held for slow viewers before the oldest are overwritten.
pub(crate) const HUB_CAPACITY: usize = 1024;

/// The ingest's own counters, for `/health` and the cost report.
#[derive(Default)]
pub(crate) struct IngestStats {
    pub received: AtomicU64,
    pub rejected: AtomicU64,
    /// Total time spent validating + publishing accepted samples, in ns.
    pub handle_ns: AtomicU64,
}

pub(crate) fn hub() -> &'static broadcast::Sender<Arc<str>> {
    static HUB: OnceLock<broadcast::Sender<Arc<str>>> = OnceLock::new();
    HUB.get_or_init(|| broadcast::channel(HUB_CAPACITY).0)
}

pub(crate) fn stats() -> &'static IngestStats {
    static STATS: OnceLock<IngestStats> = OnceLock::new();
    STATS.get_or_init(IngestStats::default)
}

/// (#2928 review, C4) How far a sample's `at_ms` may sit from the daemon's
/// own clock. The producer is on this machine (the same clock, give or take
/// a container's drift); anything further off is not live.
pub(crate) const MAX_SKEW_MS: u64 = 5_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Validate one datagram and publish it. Returns whether it was accepted.
pub(crate) fn accept(bytes: &[u8]) -> bool {
    accept_at(bytes, now_ms())
}

/// [`accept`] against an explicit clock `now_ms` (tests).
pub(crate) fn accept_at(bytes: &[u8], now_ms: u64) -> bool {
    let t0 = std::time::Instant::now();
    let st = stats();
    st.received.fetch_add(1, Ordering::Relaxed);
    let Some(sample) = darkmux_flow::live::LiveSample::from_datagram(bytes)
        .filter(|s| s.at_ms.abs_diff(now_ms) <= MAX_SKEW_MS)
    else {
        st.rejected.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let Ok(json) = serde_json::to_string(&sample) else {
        st.rejected.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    // No subscribers is not an error: nobody is watching right now.
    let _ = hub().send(Arc::from(json));
    st.handle_ns
        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    true
}

/// (#2928 review, C3/C5) The ingest socket as this daemon bound it: what
/// `/health` reports (a fingerprint and the port, never the path), and
/// whether it is still this daemon's.
pub(crate) struct IngestState {
    path: PathBuf,
    port: u16,
    /// The bound socket file's inode: how "is it still ours?" is answered.
    ino: u64,
    bound: AtomicBool,
}

impl IngestState {
    pub(crate) fn bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    pub(crate) fn health_json(&self) -> serde_json::Value {
        serde_json::json!({
            "socket_id": darkmux_flow::live::socket_fingerprint(&self.path),
            "socket_port": self.port,
            "bound": self.bound(),
        })
    }

    fn still_ours(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&self.path)
            .map(|m| m.ino() == self.ino)
            .unwrap_or(false)
    }

    /// Remove the socket on shutdown, but only if it is still this daemon's
    /// file: a second daemon that replaced it keeps its own.
    pub(crate) fn remove_socket_if_ours(&self) {
        if self.still_ours() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

static INGEST: OnceLock<Arc<IngestState>> = OnceLock::new();

/// This daemon's ingest, once bound (for `/health`).
pub(crate) fn ingest_state() -> Option<&'static Arc<IngestState>> {
    INGEST.get()
}

/// How often the ingest checks that its socket file is still its own.
const OWNERSHIP_CHECK: std::time::Duration = std::time::Duration::from_secs(5);

/// Bind the ingest socket at `path` (keyed by `port`, the port this daemon
/// bound) and serve it on a dedicated thread for the daemon's lifetime.
/// Bound whatever `runtime.live_sample_ms` says: the knob is the
/// producers' cadence, and a daemon always accepts. `None` (with one stderr
/// line) when the socket cannot be bound: the daemon serves on without a
/// live channel.
pub(crate) fn spawn_ingest(path: PathBuf, port: u16) -> Option<std::thread::JoinHandle<()>> {
    let (handle, state) = spawn_ingest_checking(path, port, OWNERSHIP_CHECK)?;
    let _ = INGEST.set(state);
    Some(handle)
}

/// [`spawn_ingest`] with the ownership-check interval as a parameter
/// (tests), returning the state instead of registering it.
pub(crate) fn spawn_ingest_checking(
    path: PathBuf,
    port: u16,
    check_every: std::time::Duration,
) -> Option<(std::thread::JoinHandle<()>, Arc<IngestState>)> {
    use std::os::unix::fs::MetadataExt;
    let sock = match darkmux_flow::live::bind_ingest(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "darkmux serve: live channel off: could not bind its socket ({e}). Viewers \
                 fall back to 2 s heartbeats. (#2928)"
            );
            return None;
        }
    };
    let ino = std::fs::symlink_metadata(&path)
        .map(|m| m.ino())
        .unwrap_or(0);
    let state = Arc::new(IngestState {
        path,
        port,
        ino,
        bound: AtomicBool::new(true),
    });
    let _ = sock.set_read_timeout(Some(check_every));
    let st = Arc::clone(&state);
    let handle = std::thread::Builder::new()
        .name("darkmux-live-ingest".into())
        .spawn(move || {
            let mut buf = vec![0u8; darkmux_flow::live::MAX_LIVE_DATAGRAM + 1];
            loop {
                match sock.recv(&mut buf) {
                    Ok(n) => {
                        accept(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                        // A quiet interval: is the socket file still ours? A
                        // second daemon on the same port and home (bound to
                        // another address) replaces it; this one then
                        // receives nothing. Report it rather than fight over
                        // the path.
                        if !st.still_ours() {
                            st.bound.store(false, Ordering::Relaxed);
                            eprintln!(
                                "darkmux serve: live channel lost: another process replaced this \
                                 daemon's socket (port {}); this daemon receives no live samples. \
                                 `darkmux doctor` names the socket in use. (#2928)",
                                st.port
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        st.bound.store(false, Ordering::Relaxed);
                        eprintln!("darkmux serve: live channel stopped: {e}. Viewers fall back to 2 s heartbeats. (#2928)");
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some((handle, state))
}

/// Every sample published from now on, as SSE `event: live` frames, for one
/// viewer. Tail-from-now like the flow tail: nothing is replayed.
pub(crate) fn live_events(
) -> futures::stream::BoxStream<'static, Result<Event, std::convert::Infallible>> {
    let rx = hub().subscribe();
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(s) => return Some((Ok(Event::default().event("live").data(&*s)), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }))
}
