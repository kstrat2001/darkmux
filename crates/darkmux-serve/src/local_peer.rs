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
//! **A second line, not the barrier.** This check catches sandboxed work
//! that stays where darkmux put it. It does not stop code trying to get
//! past it: the barrier is the sandbox's own network profile, which keeps
//! a sandbox off darkmux's ports. Read every residual below as reachable
//! unless that profile refuses it.
//!
//! **Cost.** One socket-table read ([`darkmux_platform::Platform::tcp_peers_of`],
//! one `lsof` on macOS, about 75 ms; `/proc` on Linux) per round. At most
//! one round runs per listener, on a task of its own and outside every
//! lock, and it answers every connection accepted before it began
//! ([`Rounds`]), so a burst of connections costs one read, not one each.
//! A read that hangs is no answer: `lsof` is bounded at 5 s as `netstat`
//! is, a connection waits at most [`JUDGE_BOUND`] for its round, and one
//! that times out reads as remote. Then one process-table read and one
//! `ps` per Seatbelt sandbox recorded, both skipped when no host sandbox
//! runs. A request a proxy forwarded costs no lookup. A connection is
//! judged once, on its first request; its later requests read the verdict.
//! A peer address is the operator's only while its connection lasts
//! ([`Verdict`]'s drop), so a port another process reuses later inherits
//! nothing.
//!
//! **What it cannot tell.**
//! - A sandboxed process that double-forks and starts a session of its own
//!   (`setsid`), with every parent back to the sandbox gone, has nothing
//!   marking it as the sandbox's (`sandbox_members`), so it reads as the
//!   operator. This was proven under a real `sandbox-exec`: three lines of
//!   perl, then `curl` to `/health`, got this machine's answer. The
//!   kernel's own mark (`sandbox_check`) survives `setsid`, but it marks
//!   browsers' network helpers and other apps' sandboxed processes too, so
//!   it cannot decide alone.
//! - The operator's own browser, when sandboxed code has the system open a
//!   URL in it, is the operator's browser.
//! - An app the sandbox wrote and had the system open (LaunchServices'
//!   `open` of a `.app`) runs outside the sandbox, as this user, and reads
//!   as the operator.
//! - Another user's process on this machine is never the operator: the
//!   socket lookup cannot see it, so it reads as remote.

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
        Some(local) => match ROUNDS.holders_of(local, conn.remote, conn.accepted).await {
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

/// What reads every connection to the listener at an address, `(peer,
/// pid)` ([`darkmux_platform::Platform::tcp_peers_of`]); `None`: no answer.
pub(crate) type PeersReader =
    Arc<dyn Fn(SocketAddr) -> Option<Vec<(SocketAddr, u32)>> + Send + Sync>;

/// How long a connection waits for its round: as long as one socket-table
/// read may take (the platform bounds `lsof` and `netstat` at 5 s). One
/// that has not answered by then is no answer, so its caller is remote.
const JUDGE_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// One read of the connections to a listener: when it began, and every
/// `(peer, pid)` it found (`None`: the read gave no answer).
struct Round {
    started: Instant,
    peers: Option<Vec<(SocketAddr, u32)>>,
}

/// One listener's rounds: the latest that finished (each waiter is told
/// when the next one does) and when the one running now began.
struct ListenerRounds {
    latest: tokio::sync::watch::Sender<Option<Arc<Round>>>,
    running: Option<Instant>,
}

/// The rounds of every listener: at most one read runs per listener, off
/// the async workers and outside every lock, and each connection waits for
/// the first round that began after it was accepted, at most `bound`.
pub(crate) struct Rounds {
    read: PeersReader,
    bound: std::time::Duration,
    listeners: Mutex<HashMap<SocketAddr, ListenerRounds>>,
}

/// What a connection does next: read a round's answer, or wait for the
/// next round to finish.
enum Step {
    Ready(Arc<Round>),
    Wait(tokio::sync::watch::Receiver<Option<Arc<Round>>>),
}

impl Rounds {
    pub(crate) fn new(read: PeersReader, bound: std::time::Duration) -> Arc<Self> {
        Arc::new(Rounds {
            read,
            bound,
            listeners: Mutex::new(HashMap::new()),
        })
    }

    /// Every process holding the other end of `remote`'s connection to the
    /// listener at `local`, accepted at `accepted`, from the first round
    /// that began after it was accepted (so the connection was in the table
    /// it read). `None` when that round gives no answer or does not finish
    /// within the bound.
    pub(crate) async fn holders_of(
        self: &Arc<Self>,
        local: SocketAddr,
        remote: SocketAddr,
        accepted: Instant,
    ) -> Option<Vec<u32>> {
        let round = tokio::time::timeout(self.bound, self.round_after(canonical(local), accepted))
            .await
            .ok()?;
        Some(holders_in(round.peers.as_ref()?, remote))
    }

    /// The first round at `local` that began at or after `accepted`.
    async fn round_after(self: &Arc<Self>, local: SocketAddr, accepted: Instant) -> Arc<Round> {
        loop {
            let mut finished = match self.next_step(local, accepted) {
                Step::Ready(round) => return round,
                Step::Wait(finished) => finished,
            };
            let _ = finished.changed().await;
        }
    }

    /// The latest round when it began after `accepted`; else a wait for the
    /// next, starting one when none runs.
    fn next_step(self: &Arc<Self>, local: SocketAddr, accepted: Instant) -> Step {
        let mut listeners = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        let at = listeners.entry(local).or_insert_with(|| ListenerRounds {
            latest: tokio::sync::watch::channel(None).0,
            running: None,
        });
        if let Some(round) = at.latest.borrow().clone().filter(|r| r.started >= accepted) {
            return Step::Ready(round);
        }
        let finished = at.latest.subscribe();
        if at.running.is_none() {
            let started = Instant::now();
            at.running = Some(started);
            self.start_round(local, started);
        }
        Step::Wait(finished)
    }

    /// Reads `local`'s connections on a task of its own, so it ends and
    /// frees the listener's next round whether or not anyone still waits.
    fn start_round(self: &Arc<Self>, local: SocketAddr, started: Instant) {
        let rounds = self.clone();
        tokio::spawn(async move {
            let read = rounds.read.clone();
            let peers = tokio::task::spawn_blocking(move || read(local))
                .await
                .ok()
                .flatten();
            let mut listeners = rounds.listeners.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(at) = listeners.get_mut(&local) {
                at.running = None;
                at.latest
                    .send_replace(Some(Arc::new(Round { started, peers })));
            }
        });
    }

    /// (Test support) Whether a round has run or runs at `local`.
    #[cfg(test)]
    pub(crate) fn has_read(&self, local: SocketAddr) -> bool {
        let listeners = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        listeners
            .get(&canonical(local))
            .is_some_and(|at| at.running.is_some() || at.latest.borrow().is_some())
    }
}

/// This process's rounds, read from this machine's socket table.
static ROUNDS: LazyLock<Arc<Rounds>> = LazyLock::new(|| {
    Rounds::new(
        Arc::new(|local| darkmux_platform::current().tcp_peers_of(local)),
        JUDGE_BOUND,
    )
});

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
