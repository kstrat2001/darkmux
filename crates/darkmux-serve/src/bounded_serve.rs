//! Bounded serving, for every listener darkmux runs: the fleet listener
//! (`fleet_listener::serve_bounded`, one request per connection), the
//! viewer and the client listener ([`serve_kept_alive`]).
//!
//! What they share is [`Admission`]: at most `max_conns` connections at
//! once, at most `per_ip` from one peer address, and one past either is
//! closed as it is accepted. A connection keeps its slots for as long as
//! its socket is open, a socket switched to another protocol (a WebSocket)
//! included ([`SlotHeld`]). Each listener therefore holds a bounded number
//! of file descriptors, whatever a peer opens, so a flood of idle
//! connections cannot take the descriptors the rest of the process needs.
//! Every connection must also send a complete request head within a
//! deadline (hyper's header read timeout), or it is closed.
//!
//! **A share this machine's other processes cannot take.** The operator's
//! browser and every request `tailscale serve` forwards arrive from
//! 127.0.0.1, and so does a container's (its engine's forwarder connects
//! for `host.docker.internal`) and any other local process's. So a
//! listener that serves this machine keeps part of its slots for
//! connections judged first ([`KeptAliveLimits::judged_share`]). Every
//! connection from this machine counts against one per-address count,
//! whatever source address it binds ([`from_this_machine`],
//! [`counted_as`]). Once this machine holds the rest (the open share), its
//! next connection is served only after [`crate::local_peer`] has judged
//! its process not to be contained work (a host sandbox's, or a container
//! forwarder's); contained work is closed. Two things keep a container
//! from holding the judged share while its connections wait for a round:
//!
//! - A forwarder a round has named is remembered with the ends of its
//!   connections here, and a later connection it holds is closed as it is
//!   accepted, before it holds a slot. The accept loop waits for that
//!   answer, so the container's next connections queue in the kernel's
//!   backlog, not in this listener's slots. A cached end is used once (a
//!   port reused later is another connection) and only while its process
//!   still runs that forwarder. A miss reads the forwarders' descriptors,
//!   off the async workers; that read answers every connection they hold
//!   then, and the next may begin only once the last has been over for as
//!   long as it took, so reading takes at most half of the accept loop's
//!   time and the connections queued meanwhile share it. One read grows
//!   with their descriptors (about 30 ms for 20,000 on an M5 Max). A
//!   connection already reset is closed without one.
//! - A connection waiting for its judgment gives its slots back as soon as
//!   its peer hangs up (a reset, or its end closed), so one opened and
//!   dropped at once, which no process holds by the time anything looks,
//!   holds nothing for long.
//!
//! Below the open share nothing is judged here and no other process is
//! read, so a listener under no pressure pays one `getsockname` per
//! connection for it.
//!
//! **What it does not cover.**
//! - A sandboxed process is judged by a round each time (it is not
//!   remembered: its pid could be reused by the operator's next process of
//!   the same name), so sandboxed work keeping connections open past the
//!   open share holds judged slots while each waits for its round; the
//!   sandbox's network profile keeps it off darkmux's ports (#3640).
//! - A peer this user cannot see (tailscaled, another account's process)
//!   and an engine whose forwarder is not in
//!   `darkmux_platform::CONTAINER_FORWARDERS` read as unknown and are
//!   served.
//! - A client that half-closes after sending its request, past the open
//!   share, reads as hung up and is closed while it waits.
//! - A forwarder this user cannot read (Linux's `docker-proxy`, as root)
//!   has no descriptors to read, so its connections each wait for a
//!   round.

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
    /// How many connections this machine holds before its next is judged
    /// first; `per_ip` judges nothing.
    open_share: usize,
    /// Whether it keeps a judged share at all ([`Self::with_judged_share`]).
    /// One that does not (the fleet listener) counts every peer address
    /// apart, this machine's included, and reads nothing more on accept.
    judging: bool,
}

/// A connection's places in its listener's caps, given back when dropped.
pub(crate) struct Slots {
    _conn: tokio::sync::OwnedSemaphorePermit,
    _ip: KeySlot<IpAddr>,
    /// On a listener that judges, the connection's record with its rounds
    /// ([`crate::local_peer::Rounds::holding`]), dropped with it.
    held: Option<crate::local_peer::HeldEnd>,
}

/// One admitted connection. Hold `slots` for as long as it is served.
pub(crate) struct Admitted {
    pub(crate) stream: tokio::net::TcpStream,
    pub(crate) peer: SocketAddr,
    pub(crate) slots: Slots,
    /// The listener's own end of it, when it could be read; read on
    /// accept only by a listener that judges.
    pub(crate) local: Option<SocketAddr>,
    /// Whether it must be judged before it is served: it comes from this
    /// machine ([`from_this_machine`]) and the open share is in use.
    pub(crate) judge_first: bool,
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
            open_share: per_ip,
            judging: false,
        }
    }

    /// The same, with `judged_share` of each address's `per_ip` slots kept
    /// for connections from this machine that are judged first.
    pub(crate) fn with_judged_share(mut self, judged_share: usize) -> Self {
        self.open_share = self.open_share.saturating_sub(judged_share);
        self.judging = judged_share > 0;
        self
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

    /// `stream` with its slots, or `None` (and the stream reset,
    /// [`refused`]) when its address or the listener is at its cap.
    fn admit(&self, stream: tokio::net::TcpStream, peer: SocketAddr) -> Option<Admitted> {
        let local = self.judging.then(|| stream.local_addr().ok()).flatten();
        let here = self.judging && from_this_machine(peer, local);
        let Some((ip, held)) = self.per_ip.try_take_counted(counted_as(peer, here)) else {
            return refused(stream);
        };
        let Ok(conn) = self.permits.clone().try_acquire_owned() else {
            return refused(stream);
        };
        Some(Admitted {
            stream,
            peer,
            slots: Slots {
                _conn: conn,
                _ip: ip,
                held: None,
            },
            local,
            judge_first: here && held >= self.open_share,
        })
    }

    /// Returns once every connection admitted so far has given its slots
    /// back.
    pub(crate) async fn drained(&self) {
        let all = u32::try_from(self.max_conns).unwrap_or(u32::MAX);
        let _ = self.permits.acquire_many(all).await;
    }
}

/// Whether a connection from `peer` to the listener's own end `local`
/// (`None`: unread) comes from this machine: every peer of a listener
/// bound to loopback, whatever source address it chose (this Mac's LAN
/// address, Linux's 127.0.0.2), a loopback peer, and a peer at the
/// listener's own address.
pub(crate) fn from_this_machine(peer: SocketAddr, local: Option<SocketAddr>) -> bool {
    let ip = peer.ip().to_canonical();
    ip.is_loopback()
        || local.is_some_and(|l| {
            let l = l.ip().to_canonical();
            l.is_loopback() || l == ip
        })
}

/// The address a connection from `peer` counts against for `per_ip`: one
/// for all of this machine (`here`), so a process cannot add an open share
/// for each source address it binds.
pub(crate) fn counted_as(peer: SocketAddr, here: bool) -> IpAddr {
    if here {
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        peer.ip().to_canonical()
    }
}

/// Closes a connection this listener refuses with a reset (`SO_LINGER` 0),
/// never an ordinary close: the side that closes first keeps the
/// connection in TIME_WAIT for a minute or so, and while it does, that
/// connection's source port cannot be used for a new connection to this
/// same listener. A peer that opens connections as fast as they are
/// refused could otherwise tie up the ports this machine's clients, the
/// operator's browser among them, need to reach this listener. The reset
/// is immediate; nothing blocks.
pub(crate) fn refused<T>(stream: tokio::net::TcpStream) -> Option<T> {
    let _ = stream.set_zero_linger();
    None
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
    /// Of `per_ip`, the slots a connection from this machine takes only
    /// once judged not to be contained work (a host sandbox's process or a
    /// container forwarder): the rest, the open share, is served unjudged.
    /// Contained work therefore holds at most the open share of this
    /// machine's connections, and this share stays for the operator's
    /// processes and for peers this user cannot see (tailscaled, which runs
    /// as root).
    pub judged_share: usize,
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
/// request. Half of this machine's slots are judged, so a container or a
/// sandbox holds at most 64, and a known forwarder's connections past those
/// are closed as they are accepted.
pub(crate) const VIEWER: KeptAliveLimits = KeptAliveLimits {
    label: "darkmux serve",
    max_conns: 256,
    per_ip: 128,
    judged_share: 64,
    header_read_timeout: Duration::from_secs(10),
};

/// The client listener (`darkmux client serve`). It binds loopback only, so
/// every connection, a device's through the tailnet entry included, comes
/// from 127.0.0.1 and `per_ip` is the whole. Under launchd it runs with a
/// soft limit of 256 open files, and a proxied preview request holds two
/// (the browser's connection and the preview's), so the cap keeps every
/// connection's descriptors within half of that. Half of the slots are
/// judged, as the viewer's are.
pub(crate) const CLIENT: KeptAliveLimits = KeptAliveLimits {
    label: "darkmux client serve",
    max_conns: 64,
    per_ip: 64,
    judged_share: 32,
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
/// a route; past the open share of an address of this machine, a
/// connection is judged before it is served
/// ([`KeptAliveLimits::judged_share`]). Once `shutdown` says stop, nothing
/// more is accepted, each open connection finishes the answer it is
/// giving and closes (an idle one at once), and this returns when the last
/// has closed: an upgraded one (a WebSocket) once its handler has let its
/// stream go.
pub(crate) async fn serve_kept_alive(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: KeptAliveLimits,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let rounds = crate::local_peer::this_machine();
    serve_kept_alive_reading(listener, app, limits, rounds, shutdown).await;
}

/// [`serve_kept_alive`], each connection judged from `rounds`' reads of
/// this machine's connections.
pub(crate) async fn serve_kept_alive_reading(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: KeptAliveLimits,
    rounds: Arc<crate::local_peer::Rounds>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let app = crate::local_peer::judged(app);
    let admission = Admission::new(limits.label, limits.max_conns, limits.per_ip)
        .with_judged_share(limits.judged_share);
    while let Some(conn) = admission.next(&listener, &mut shutdown).await {
        if refused_on_accept(&rounds, darkmux_platform::current(), &conn).await {
            refused::<()>(conn.stream);
            continue;
        }
        let mut conn = conn;
        conn.slots.held = conn.local.map(|local| rounds.holding(conn.peer, local));
        tokio::spawn(kept_alive(
            conn,
            app.clone(),
            rounds.clone(),
            limits.header_read_timeout,
            shutdown.clone(),
        ));
    }
    drop(listener);
    admission.drained().await;
}

/// Whether `conn` is closed as it is accepted: one judged first that has
/// already been reset (it is gone; nothing to read for it), or one a known
/// container forwarder holds
/// ([`crate::local_peer::Rounds::held_by_a_known_forwarder`], read through
/// `platform`). Every connection takes its own end out of the forwarders'
/// cache, the reset ones and the refused ones included, since a port it
/// reuses later is another connection.
///
/// Asked as the connection is accepted, so a container's is closed before
/// it holds a slot while a round runs. The accept loop waits for the
/// answer, and that wait is the point: a container's next connections
/// queue in the kernel's accept backlog behind it, in arrival order with
/// everyone else's, instead of in this listener's slots.
async fn refused_on_accept(
    rounds: &Arc<crate::local_peer::Rounds>,
    platform: &'static dyn darkmux_platform::Platform,
    conn: &Admitted,
) -> bool {
    let Some(local) = conn.local else {
        return false;
    };
    let reset = matches!(conn.stream.take_error(), Ok(Some(_)));
    if !conn.judge_first || reset {
        rounds.take_end(conn.peer, local);
        return conn.judge_first;
    }
    rounds
        .held_by_a_known_forwarder(platform, conn.peer, local)
        .await
}

/// Serves one admitted connection until it closes, or, once `shutdown`
/// says stop, until it has finished the answer it is giving. One that must
/// be judged first and is contained work is closed unserved.
async fn kept_alive(
    conn: Admitted,
    app: Router,
    rounds: Arc<crate::local_peer::Rounds>,
    header_read_timeout: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let Admitted {
        stream,
        peer,
        slots,
        local,
        judge_first,
    } = conn;
    let local = local.or_else(|| stream.local_addr().ok());
    let this_conn = crate::local_peer::PeerConn::new(peer, local, rounds);
    if judge_first && !judged_in_time(&this_conn, &stream, &mut shutdown).await {
        refused::<()>(stream);
        return;
    }
    let served = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .serve_connection(
            TokioIo::new(SlotHeld {
                io: stream,
                _slots: slots,
            }),
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

/// Whether `conn` may take a judged slot
/// ([`crate::local_peer::may_take_a_judged_slot`]); not once `shutdown`
/// says stop while it waits, so a stop never waits on a lookup, and not
/// once its peer has hung up (`stream`, [`hung_up`]), so a connection
/// opened and dropped at once gives its slots back then, not after its
/// judgment.
async fn judged_in_time(
    conn: &crate::local_peer::PeerConn,
    stream: &tokio::net::TcpStream,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        may = crate::local_peer::may_take_a_judged_slot(conn) => may,
        () = stopped(shutdown) => false,
        () = hung_up(stream) => false,
    }
}

/// How often a connection with unread data is looked at again for its
/// peer's hang-up while it waits for its judgment.
const HANG_UP_CHECK: Duration = Duration::from_millis(20);

/// Returns once `stream`'s peer has hung up: a reset, or its end closed
/// (noticed behind unread data too: the reactor's read-closed readiness,
/// kqueue's EOF or epoll's RDHUP), never while it is open. One with
/// unread data stays readable, so it is looked at again every
/// [`HANG_UP_CHECK`]; one with none waits for the reactor. Nothing is read:
/// the request stays where hyper reads it. A peer that half-closes after
/// sending its request reads as hung up too, so such a client past the
/// open share is closed while it waits.
async fn hung_up(stream: &tokio::net::TcpStream) {
    loop {
        match stream.ready(tokio::io::Interest::READABLE).await {
            Ok(r) if !(r.is_read_closed() || r.is_error()) => {
                tokio::time::sleep(HANG_UP_CHECK).await
            }
            _ => return,
        }
    }
}

/// A connection's socket together with its slots, so they are given back
/// only when the socket is dropped. Hyper hands this same IO to an upgrade
/// (a WebSocket's handler holds it as `hyper::upgrade::Upgraded`), so an
/// upgraded connection keeps counting toward the caps, and holds up the
/// drain, until its handler lets the stream go, not when hyper's own
/// connection future ends.
struct SlotHeld {
    io: tokio::net::TcpStream,
    _slots: Slots,
}

impl tokio::io::AsyncRead for SlotHeld {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for SlotHeld {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
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
