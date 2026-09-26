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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Validate one datagram and publish it. Returns whether it was accepted.
pub(crate) fn accept(bytes: &[u8]) -> bool {
    let t0 = std::time::Instant::now();
    let st = stats();
    st.received.fetch_add(1, Ordering::Relaxed);
    let Some(sample) = darkmux_flow::live::LiveSample::from_datagram(bytes) else {
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

/// Bind the ingest socket at `path` and serve it on a dedicated thread for
/// the daemon's lifetime. `None` (with one stderr line) when the socket
/// cannot be bound: the daemon serves on without a live channel.
pub(crate) fn spawn_ingest(path: PathBuf) -> Option<std::thread::JoinHandle<()>> {
    let sock = match darkmux_flow::live::bind_ingest(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "darkmux serve: live channel off: could not bind {} ({e}). Viewers fall back to \
                 2 s heartbeats. (#2928)",
                path.display()
            );
            return None;
        }
    };
    std::thread::Builder::new()
        .name("darkmux-live-ingest".into())
        .spawn(move || {
            let mut buf = vec![0u8; darkmux_flow::live::MAX_LIVE_DATAGRAM + 1];
            while let Ok(n) = sock.recv(&mut buf) {
                accept(&buf[..n]);
            }
        })
        .ok()
}

/// Remove the ingest socket on shutdown (best effort; the next daemon on
/// this port replaces a stale one anyway).
pub(crate) fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
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
