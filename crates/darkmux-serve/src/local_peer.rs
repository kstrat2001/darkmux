//! Whose process a loopback peer is: the operator's, or sandboxed work.
//!
//! A loopback address says only that the caller runs on this machine, and
//! sandboxed work does too. A sandbox on the host (Seatbelt, Landlock) is a
//! process here; a container's connection to `host.docker.internal` arrives
//! from the container engine's forwarder, which the listener also sees as
//! 127.0.0.1. Neither is the operator, and neither may read what only the
//! operator's side reads. So every listener that trusts a request from this
//! machine ([`crate::is_local_at`]) also asks who holds the other end of its
//! connection, once per connection ([`make_service`]):
//!
//! - **The operator** ([`PeerProcess::Operator`]): a process of this user
//!   outside every host sandbox, and not a container engine's forwarder
//!   (the browser, the CLI from a shell, an app). The no-token path, as
//!   before.
//! - **Sandboxed work** ([`PeerProcess::SandboxedWork`]): one of the
//!   processes of a host sandbox ([`darkmux_sandbox::host_sandbox_launchers`],
//!   then [`darkmux_platform::Platform::in_sandbox_of`]: the launcher, its
//!   group and session, and every descendant).
//! - **A container's** ([`PeerProcess::ContainerForwarder`]): the holder is
//!   a forwarder ([`darkmux_platform::CONTAINER_FORWARDERS`]).
//! - **Unknown** ([`PeerProcess::Unknown`]): no process this user can see
//!   holds it (another user's), or a lookup failed.
//!
//! Anything but the operator is not this machine: the viewer daemon answers
//! it as it answers a remote reader (redacted, and a token required where
//! read auth is on), and the client listener refuses it. Failing toward
//! showing less is the doctrine every other local check here follows.
//!
//! **Cost.** One socket-table read ([`darkmux_platform::Platform::tcp_peers_of`],
//! one `lsof` on macOS, about 75 ms; `/proc` on Linux) per round, and a
//! round answers every connection accepted before it began
//! ([`holders_of`]), so a burst of connections costs one read, not one
//! each. Then one process-table read, skipped when no host sandbox runs.
//! A connection is judged once, on its first request; its later requests
//! read the verdict. A peer address is the operator's only while its
//! connection lasts ([`Verdict`]'s drop), so a port another process reuses
//! later inherits nothing.
//!
//! **What it cannot tell.** A sandboxed process that started a session of
//! its own and whose parents back to the sandbox have all exited (a
//! double-fork daemon) has nothing marking it as the sandbox's
//! (`sandbox_members`), so it reads as the operator. So does the operator's
//! own browser, when sandboxed code has the system open a URL in it.

use axum::extract::connect_info::{Connected, IntoMakeServiceWithConnectInfo};
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::IncomingStream;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

/// Who holds the other end of a loopback connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerProcess {
    /// A process of this user outside every host sandbox, not a forwarder.
    Operator,
    /// One of the processes of a sandbox running on this host.
    SandboxedWork,
    /// A container engine's forwarder (its executable's name).
    ContainerForwarder(String),
    /// It could not be told, and why.
    Unknown(&'static str),
}

/// The verdict for a connection whose holders are `holders` (every process
/// holding its other end), given the launchers of this host's sandboxes
/// (`None`: their state could not be read). Anything that cannot be read is
/// [`PeerProcess::Unknown`], never the operator.
pub(crate) fn judge(
    platform: &dyn darkmux_platform::Platform,
    holders: &[u32],
    launchers: Option<&[u32]>,
) -> PeerProcess {
    if holders.is_empty() {
        return PeerProcess::Unknown(
            "no process this user can see holds the connection's other end",
        );
    }
    if let Some(kind) = holders.iter().find_map(|&pid| by_name(platform, pid)) {
        return kind;
    }
    match launchers {
        None => PeerProcess::Unknown("the host sandboxes' state could not be read"),
        Some([]) => PeerProcess::Operator,
        Some(leaders) => by_membership(platform, holders, leaders),
    }
}

/// `pid`'s verdict by its name alone: a forwarder's, or no name to read;
/// `None` when its name says nothing.
fn by_name(platform: &dyn darkmux_platform::Platform, pid: u32) -> Option<PeerProcess> {
    match platform.process_name(pid) {
        None => Some(PeerProcess::Unknown(
            "a process holding the connection has no name to read",
        )),
        Some(name) if darkmux_platform::is_container_forwarder(&name) => {
            Some(PeerProcess::ContainerForwarder(name))
        }
        Some(_) => None,
    }
}

/// Sandboxed work when any of `holders` is in one of `leaders`' sandboxes.
fn by_membership(
    platform: &dyn darkmux_platform::Platform,
    holders: &[u32],
    leaders: &[u32],
) -> PeerProcess {
    for &pid in holders {
        match platform.in_sandbox_of(pid, leaders) {
            None => return PeerProcess::Unknown("the process table could not be read"),
            Some(true) => return PeerProcess::SandboxedWork,
            Some(false) => {}
        }
    }
    PeerProcess::Operator
}

/// `addr` with an IPv4-mapped IPv6 address read as its IPv4 one, so a
/// connection to `[::ffff:127.0.0.1]` is the same peer however it is
/// spelled.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// The peer addresses whose live connections were judged the operator's,
/// each with how many such connections it has open.
static OPERATOR_PEERS: LazyLock<Mutex<HashMap<SocketAddr, u32>>> = LazyLock::new(Default::default);

/// Whether `peer`'s connection was judged the operator's and is still open.
/// A peer no connection judged (a request no listener here accepted) is not.
pub(crate) fn is_operator(peer: SocketAddr) -> bool {
    OPERATOR_PEERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&canonical(peer))
        .is_some_and(|n| *n > 0)
}

/// Counts one more open operator connection from `peer`.
fn hold(peer: SocketAddr) {
    *OPERATOR_PEERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(canonical(peer))
        .or_default() += 1;
}

/// Counts one fewer, forgetting `peer` at none.
fn release(peer: SocketAddr) {
    let mut peers = OPERATOR_PEERS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(n) = peers.get_mut(&canonical(peer)) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            peers.remove(&canonical(peer));
        }
    }
}

/// (Test support) States that a request from `peer` is the operator's, for
/// a test that hands a router a peer address with no connection behind it.
#[cfg(test)]
pub(crate) fn state_operator(peer: SocketAddr) {
    if !is_operator(peer) {
        hold(peer);
    }
}

/// (Test support) `peer`, stated the operator's ([`state_operator`]): a
/// peer a test hands a router or a predicate is this test's own process.
#[cfg(test)]
pub(crate) fn operator(peer: SocketAddr) -> SocketAddr {
    state_operator(peer);
    peer
}

/// One connection's verdict, reached once, on its first request. Dropped
/// with the connection (the per-connection service holds the only copy once
/// [`judge_peer`] has taken it off the request), which is when its peer
/// stops being the operator's.
struct Verdict {
    remote: SocketAddr,
    judged: tokio::sync::OnceCell<PeerProcess>,
}

impl Drop for Verdict {
    fn drop(&mut self) {
        if self.judged.get() == Some(&PeerProcess::Operator) {
            release(self.remote);
        }
    }
}

/// What a listener served through [`make_service`] knows of a connection:
/// both its ends, when it was accepted, and its verdict.
#[derive(Clone)]
pub(crate) struct PeerConn {
    remote: SocketAddr,
    local: Option<SocketAddr>,
    accepted: Instant,
    verdict: Arc<Verdict>,
}

impl Connected<IncomingStream<'_>> for PeerConn {
    fn connect_info(target: IncomingStream<'_>) -> Self {
        let remote = target.remote_addr();
        let verdict = Arc::new(Verdict {
            remote,
            judged: tokio::sync::OnceCell::new(),
        });
        PeerConn {
            remote,
            local: target.local_addr().ok(),
            accepted: Instant::now(),
            verdict,
        }
    }
}

/// `app` as every listener that trusts this machine serves it: each
/// request carries its peer's address (`ConnectInfo<SocketAddr>`, as the
/// handlers read it), and a loopback connection is judged
/// ([`judge_peer`]) before any of its requests reaches a route.
pub(crate) fn make_service(app: Router) -> IntoMakeServiceWithConnectInfo<Router, PeerConn> {
    app.layer(axum::middleware::from_fn(judge_peer))
        .into_make_service_with_connect_info::<PeerConn>()
}

/// Puts the peer's address on the request and, for a loopback peer, waits
/// for its connection's verdict: the operator's is recorded for
/// [`is_operator`]. A request a proxy forwarded is never this machine's
/// ([`crate::is_local_at`]), so it costs no lookup; a later request on the
/// same connection without one is judged then. The connection's own record is taken off the request,
/// so nothing a handler keeps outlives the connection with it.
async fn judge_peer(mut req: Request, next: Next) -> Response {
    if let Some(ConnectInfo(conn)) = req.extensions_mut().remove::<ConnectInfo<PeerConn>>() {
        req.extensions_mut().insert(ConnectInfo(conn.remote));
        if conn.remote.ip().to_canonical().is_loopback()
            && !crate::carries_proxy_header(req.headers())
        {
            conn.verdict.judged.get_or_init(|| verdict_of(&conn)).await;
        }
    }
    next.run(req).await
}

/// `conn`'s verdict from this machine's socket and process tables; the
/// operator's is held for as long as the connection lasts.
async fn verdict_of(conn: &PeerConn) -> PeerProcess {
    let kind = match conn.local {
        None => PeerProcess::Unknown("the connection's own address could not be read"),
        Some(local) => match holders_of(local, conn.remote, conn.accepted).await {
            None => PeerProcess::Unknown("this machine's connections could not be read"),
            Some(holders) => judged_now(holders).await,
        },
    };
    if kind == PeerProcess::Operator {
        hold(conn.remote);
    }
    kind
}

/// [`judge`] of `holders` against this host's tables, off the async workers.
async fn judged_now(holders: Vec<u32>) -> PeerProcess {
    tokio::task::spawn_blocking(move || {
        let launchers = darkmux_sandbox::host_sandbox_launchers();
        judge(darkmux_platform::current(), &holders, launchers.as_deref())
    })
    .await
    .unwrap_or(PeerProcess::Unknown("the lookup did not finish"))
}

/// One read of the connections to a listener: when it began, and every
/// `(peer, pid)` it found.
struct Round {
    started: Instant,
    peers: Vec<(SocketAddr, u32)>,
}

/// The latest round for each listener address; one round runs at a time.
static ROUNDS: LazyLock<tokio::sync::Mutex<HashMap<SocketAddr, Round>>> =
    LazyLock::new(Default::default);

/// Every process holding the other end of `remote`'s connection to the
/// listener at `local`, accepted at `accepted`: from the latest round when
/// it began after the connection was accepted (so the connection was in the
/// table it read), else from a new one. `None` when the table cannot be read.
async fn holders_of(local: SocketAddr, remote: SocketAddr, accepted: Instant) -> Option<Vec<u32>> {
    let local = canonical(local);
    let mut rounds = ROUNDS.lock().await;
    if !rounds.get(&local).is_some_and(|r| r.started >= accepted) {
        let started = Instant::now();
        let peers =
            tokio::task::spawn_blocking(move || darkmux_platform::current().tcp_peers_of(local))
                .await
                .ok()
                .flatten()?;
        rounds.insert(local, Round { started, peers });
    }
    Some(holders_in(&rounds.get(&local)?.peers, remote))
}

/// The pids holding `remote` among a round's `(peer, pid)` pairs, each once.
fn holders_in(peers: &[(SocketAddr, u32)], remote: SocketAddr) -> Vec<u32> {
    let remote = canonical(remote);
    let mut pids: Vec<u32> = peers
        .iter()
        .filter(|(p, _)| canonical(*p) == remote)
        .map(|(_, pid)| *pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
#[path = "local_peer_tests.rs"]
mod tests;
