//! Bounded serving, for every listener darkmux runs: the fleet listener
//! (`fleet_listener::serve_bounded`, one request per connection), the
//! viewer and the client listener ([`serve_kept_alive`]).
//!
//! What they share is [`Admission`]: at most `max_conns` connections at
//! once, at most `per_ip` from one peer address, and one past either is
//! closed as it is accepted. Each listener therefore holds a bounded number
//! of file descriptors, whatever a peer opens, so a flood of idle
//! connections cannot take the descriptors the rest of the process needs.
//! Every connection must also send a complete request head within a
//! deadline (hyper's header read timeout), or it is closed.

use crate::fleet_listener::{KeySlot, KeySlots};
use axum::extract::ConnectInfo;
use axum::Router;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// Who may hold a connection at once ([`Admission::next`]).
pub(crate) struct Admission {
    /// The listener as its log lines name it.
    label: &'static str,
    max_conns: usize,
    permits: Arc<tokio::sync::Semaphore>,
    per_ip: Arc<KeySlots<IpAddr>>,
}

/// A connection's places in its listener's caps, given back when dropped.
pub(crate) struct Slots {
    _conn: tokio::sync::OwnedSemaphorePermit,
    _ip: KeySlot<IpAddr>,
}

/// One admitted connection. Hold `slots` for as long as it is served.
pub(crate) struct Admitted {
    pub(crate) stream: tokio::net::TcpStream,
    pub(crate) peer: SocketAddr,
    pub(crate) slots: Slots,
}

/// Returns once `shutdown` says stop, or once its sender is gone.
pub(crate) async fn stopped(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|v| *v).await;
}

impl Admission {
    /// `max_conns` connections in all, `per_ip` from one peer address;
    /// `label` names the listener in its log lines.
    pub(crate) fn new(label: &'static str, max_conns: usize, per_ip: usize) -> Self {
        Admission {
            label,
            max_conns,
            permits: Arc::new(tokio::sync::Semaphore::new(max_conns)),
            per_ip: Arc::new(KeySlots::new(per_ip)),
        }
    }

    /// The next connection both caps admit, or `None` once `shutdown` says
    /// stop. A connection past a cap is closed at once. An accept that
    /// fails (no descriptor left, say) is logged and tried again after
    /// 100 ms.
    pub(crate) async fn next(
        &self,
        listener: &tokio::net::TcpListener,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Option<Admitted> {
        loop {
            let (stream, peer) = tokio::select! {
                r = listener.accept() => match r {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("{}: accept failed ({e}); pausing 100ms", self.label);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
                () = stopped(shutdown) => return None,
            };
            if let Some(admitted) = self.admit(stream, peer) {
                return Some(admitted);
            }
        }
    }

    /// `stream` with its slots, or `None` (and the stream dropped, which
    /// closes it) when its address or the listener is at its cap.
    fn admit(&self, stream: tokio::net::TcpStream, peer: SocketAddr) -> Option<Admitted> {
        let ip = self.per_ip.try_take(peer.ip().to_canonical())?;
        let conn = self.permits.clone().try_acquire_owned().ok()?;
        Some(Admitted {
            stream,
            peer,
            slots: Slots {
                _conn: conn,
                _ip: ip,
            },
        })
    }

    /// Returns once every connection admitted so far has given its slots
    /// back.
    pub(crate) async fn drained(&self) {
        let all = u32::try_from(self.max_conns).unwrap_or(u32::MAX);
        let _ = self.permits.acquire_many(all).await;
    }
}

/// The bounds of a listener served by [`serve_kept_alive`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeptAliveLimits {
    /// The listener as its log lines name it.
    pub label: &'static str,
    /// Connections served at once; one more is closed on accept.
    pub max_conns: usize,
    /// Connections from one peer address at once.
    pub per_ip: usize,
    /// Time allowed for each request's head: from the connection's start
    /// for its first request, and from the end of the last answer for each
    /// one after it, so a connection held open idle closes after this.
    pub header_read_timeout: Duration,
}

/// The viewer daemon (`darkmux serve`). Its own process raises its soft
/// open-file limit to 10240 at start, so the cap is well inside it. Every
/// request `tailscale serve` forwards arrives from 127.0.0.1, as does the
/// operator's own browser, so `per_ip` is half the whole: a peer that
/// binds the viewer to its tailnet address directly leaves room for the
/// rest. A browser holds at most six connections to one host, and an idle
/// one is closed within the header deadline and opened again on its next
/// request.
pub(crate) const VIEWER: KeptAliveLimits = KeptAliveLimits {
    label: "darkmux serve",
    max_conns: 256,
    per_ip: 128,
    header_read_timeout: Duration::from_secs(10),
};

/// The client listener (`darkmux client serve`). It binds loopback only, so
/// every connection, a device's through the tailnet entry included, comes
/// from 127.0.0.1 and `per_ip` is the whole. Under launchd it runs with a
/// soft limit of 256 open files, and a proxied preview request holds two
/// (the browser's connection and the preview's), so the cap keeps every
/// connection's descriptors within half of that.
pub(crate) const CLIENT: KeptAliveLimits = KeptAliveLimits {
    label: "darkmux client serve",
    max_conns: 64,
    per_ip: 64,
    header_read_timeout: Duration::from_secs(10),
};

/// Serves `app` on `listener` within `limits`, the way a browser or an app
/// expects: connections kept alive between requests, WebSocket upgrades,
/// and answers that stream (SSE) for as long as they last. What bounds a
/// connection is [`Admission`] and the header deadline: one that sends no
/// complete request head in time, an idle one between requests included,
/// is closed. Each connection gets one [`crate::local_peer::PeerConn`], and
/// the app is served behind [`crate::local_peer::judged`] (applied once
/// here), so every request carries its peer's address as
/// `ConnectInfo<SocketAddr>` and a loopback connection's process is judged
/// (sandboxed work is not this machine) before any of its requests reaches
/// a route. Once `shutdown` says stop, nothing more is accepted, each
/// open connection finishes the answer it is giving and closes (an idle one
/// at once), and this returns when the last has closed.
pub(crate) async fn serve_kept_alive(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: KeptAliveLimits,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let app = crate::local_peer::judged(app);
    let admission = Admission::new(limits.label, limits.max_conns, limits.per_ip);
    while let Some(conn) = admission.next(&listener, &mut shutdown).await {
        tokio::spawn(kept_alive(
            conn,
            app.clone(),
            limits.header_read_timeout,
            shutdown.clone(),
        ));
    }
    drop(listener);
    admission.drained().await;
}

/// Serves one admitted connection until it closes, or, once `shutdown`
/// says stop, until it has finished the answer it is giving.
async fn kept_alive(
    conn: Admitted,
    app: Router,
    header_read_timeout: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let Admitted {
        stream,
        peer,
        slots: _slots,
    } = conn;
    let this_conn = crate::local_peer::PeerConn::new(peer, stream.local_addr().ok());
    let served = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .serve_connection(
            TokioIo::new(stream),
            connection_service(app, ConnectInfo(this_conn)),
        )
        .with_upgrades();
    tokio::pin!(served);
    tokio::select! {
        _ = served.as_mut() => return,
        () = stopped(&mut shutdown) => served.as_mut().graceful_shutdown(),
    }
    let _ = served.await;
}

/// `app` as it serves one connection: each request carries `conn` (what
/// the listener knows of the connection: `ConnectInfo<SocketAddr>` for the
/// fleet listener, `ConnectInfo<PeerConn>` for a listener behind
/// [`crate::local_peer::judged`]) as an extension, set before the router
/// sees it. Constant time per connection; layering the router for each one
/// instead rebuilds every route (about 95 µs on the viewer's).
pub(crate) fn connection_service<C: Clone + Send + Sync + 'static>(
    app: Router,
    conn: C,
) -> impl hyper::service::Service<
    axum::http::Request<hyper::body::Incoming>,
    Response = axum::response::Response,
    Error = std::convert::Infallible,
    Future = impl std::future::Future<
        Output = Result<axum::response::Response, std::convert::Infallible>,
    > + Send,
> + Send {
    let router = hyper_util::service::TowerToHyperService::new(app);
    hyper::service::service_fn(move |mut req: axum::http::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(conn.clone());
        hyper::service::Service::call(&router, req)
    })
}

#[cfg(test)]
#[path = "bounded_serve_tests.rs"]
mod tests;
