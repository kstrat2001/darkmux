//! The work-submission listener (#2916, stage 1).
//!
//! A second listener beside the viewer port, bound ONLY to the overlay
//! address the identity provider reports for this machine (never `0.0.0.0`,
//! never a LAN address, never loopback), so every connection it accepts has a
//! real peer address the provider can name. It is deliberately not behind
//! `tailscale serve`: that front proxies to loopback, so every peer would
//! arrive as 127.0.0.1 and could not be told apart.
//!
//! One route, `POST /fleet/work`, and every request on this listener (any
//! path) passes the gate first: the fleet token, then the connecting node
//! (the provider's answer for the socket's peer address), then the
//! allow-list. The gate is [`darkmux_fleet::admit`]; the scope check after
//! parsing is [`darkmux_fleet::check_scope`]. Both are pure and table-tested
//! in `darkmux-fleet`; this module is the wiring, tested over real HTTP with
//! a fake provider.
//!
//! **Cost (#2916 self-QA).** The identity lookup runs the provider's tool
//! once per request that carries the right token (~25 ms measured on the
//! laptop, 2026-09-27), with no cache: a submitted job occupies the machine
//! for minutes, so the lookup is noise next to it, and without a cache an
//! `untrust` or a node leaving the network takes effect on the very next
//! request. The allow-list is likewise read from `config.json` on every
//! request (a few KB), so `machine trust` / `untrust` need no daemon
//! restart. A caller without the token costs one constant-time compare and
//! never makes this machine spawn anything.

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json, Router,
};
use darkmux_crew::dispatch::DispatchResult;
use darkmux_fleet::{
    Admitted, IdentityProvider, ProfileResolution, Refusal, SubmissionReply, TokenCheck, WorkJob,
    WorkSubmission,
};
use darkmux_types::config::AcceptWorkEntry;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

type AllowList = BTreeMap<String, AcceptWorkEntry>;
type ResolveProfile = dyn Fn(&str, Option<&str>) -> ProfileResolution + Send + Sync;
type ExecuteJob = dyn Fn(WorkJob, String, String) -> anyhow::Result<DispatchResult> + Send + Sync;

/// Everything the listener needs, injectable so tests drive the real router
/// over a real socket with a fake provider and a fake executor.
#[derive(Clone)]
pub(crate) struct FleetListenerState {
    /// This machine's name (its `machine_id`).
    pub receiver: String,
    pub provider: Arc<dyn IdentityProvider>,
    /// This machine's own node id: a request from it is refused.
    pub local_node_id: Option<String>,
    /// The expected fleet token, read per request (`None` = not configured).
    pub token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// The allow-list, read per request. An error refuses everything.
    pub allow_list: Arc<dyn Fn() -> Result<AllowList, String> + Send + Sync>,
    /// (role, requested profile) → what this machine would run.
    pub resolve_profile: Arc<ResolveProfile>,
    /// Runs an admitted, in-scope job on the resolved profile. Blocking.
    pub execute: Arc<ExecuteJob>,
    /// The one submitted job running now, by session id.
    pub busy: Arc<Mutex<Option<String>>>,
    /// Per-peer throttle on refusal log lines.
    pub refusal_log: Arc<RefusalLog>,
    /// (#2916 round 3 C5) Requests in flight per admitted NODE, so a node
    /// that spreads connections over several addresses (a subnet router)
    /// is still capped once it is identified.
    pub node_slots: Arc<KeySlots<String>>,
}

impl FleetListenerState {
    /// Production wiring: the configured provider, the serve token, the
    /// allow-list from `config.json`, this machine's registry, and
    /// `darkmux_fleet::execute_job`.
    pub(crate) fn production(
        receiver: String,
        provider: Arc<dyn IdentityProvider>,
        local_node_id: Option<String>,
    ) -> Self {
        let resolve_receiver = receiver.clone();
        Self {
            receiver,
            provider,
            local_node_id,
            token: Arc::new(|| darkmux_flow::serve_token().map(|t| t.expose_for_compare().to_string())),
            allow_list: Arc::new(darkmux_fleet::read_user_allow_list),
            resolve_profile: Arc::new(move |role, requested| {
                darkmux_fleet::resolve_work_profile(role, requested, &resolve_receiver)
            }),
            execute: Arc::new(darkmux_fleet::execute_job),
            busy: Arc::new(Mutex::new(None)),
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
        }
    }
}

/// The listener's router: the one route, behind the gate.
pub(crate) fn router(state: FleetListenerState) -> Router {
    Router::new()
        .route(darkmux_fleet::SUBMISSION_PATH, axum::routing::post(submit_handler))
        .layer(axum::middleware::from_fn_with_state(state.clone(), gate))
        .with_state(state)
}

/// (#2916 re-review MUST 2) Refusals are logged at most
/// [`RefusalLog::PER_WINDOW`] times per peer address per minute; the rest
/// are counted, and the count is logged once the next minute begins. A
/// peer hammering the listener could otherwise grow the daemon's log
/// (under `brew services`, an unrotated file) without bound.
pub(crate) struct RefusalLog {
    peers: Mutex<std::collections::HashMap<std::net::IpAddr, (std::time::Instant, u32, u64)>>,
    /// (#2916 round 3 C6) Refusals from addresses the full table could not
    /// track: counted here, never dropped silently.
    overflow: std::sync::atomic::AtomicU64,
    /// Lines actually written (for tests).
    pub written: std::sync::atomic::AtomicU64,
}

/// What to do with one refusal's log line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LogDecision {
    /// Write it; `Some(k)`: first say `k` lines were suppressed last window.
    Write(Option<u64>),
    Suppress,
}

impl RefusalLog {
    pub(crate) const PER_WINDOW: u32 = 5;
    pub(crate) const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    pub(crate) const MAX_PEERS: usize = 4096;

    pub(crate) fn new() -> Self {
        Self {
            peers: Mutex::new(Default::default()),
            overflow: std::sync::atomic::AtomicU64::new(0),
            written: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn decide(&self, ip: std::net::IpAddr, now: std::time::Instant) -> LogDecision {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        if peers.len() >= Self::MAX_PEERS && !peers.contains_key(&ip) {
            peers.retain(|_, (start, _, sup)| now.duration_since(*start) < Self::WINDOW || *sup > 0);
            if peers.len() >= Self::MAX_PEERS {
                self.overflow.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return LogDecision::Suppress;
            }
        }
        let e = peers.entry(ip).or_insert((now, 0, 0));
        if now.duration_since(e.0) >= Self::WINDOW {
            let suppressed = e.2;
            *e = (now, 1, 0);
            return LogDecision::Write((suppressed > 0).then_some(suppressed));
        }
        if e.1 < Self::PER_WINDOW {
            e.1 += 1;
            LogDecision::Write(None)
        } else {
            e.2 += 1;
            LogDecision::Suppress
        }
    }

    /// (#2916 round 3 C6) Report counts nobody has reported yet, on a
    /// timer rather than only on the next refusal: every address whose
    /// minute has ended with suppressed lines, and the overflow counter.
    /// Returns the lines it wrote (for tests).
    pub(crate) fn flush(&self, now: std::time::Instant) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
            peers.retain(|ip, (start, _, sup)| {
                if now.duration_since(*start) >= Self::WINDOW {
                    if *sup > 0 {
                        lines.push(format!("darkmux fleet: suppressed {sup} refusal log line(s) from {ip} in the last minute"));
                    }
                    false
                } else {
                    true
                }
            });
        }
        let over = self.overflow.swap(0, std::sync::atomic::Ordering::SeqCst);
        if over > 0 {
            lines.push(format!(
                "darkmux fleet: suppressed {over} refusal log line(s) from addresses beyond the {} tracked",
                Self::MAX_PEERS
            ));
        }
        for l in &lines {
            eprintln!("{l}");
        }
        lines
    }

    fn log(&self, ip: Option<std::net::IpAddr>, line: &str) {
        let decision = match ip {
            Some(ip) => self.decide(ip, std::time::Instant::now()),
            None => LogDecision::Write(None),
        };
        if let LogDecision::Write(prev) = decision {
            if let (Some(k), Some(ip)) = (prev, ip) {
                eprintln!("darkmux fleet: suppressed {k} refusal log line(s) from {ip} in the last minute");
            }
            eprintln!("{line}");
            self.written.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn refuse(state: &FleetListenerState, peer: Option<std::net::IpAddr>, r: &Refusal) -> Response {
    let receiver = &state.receiver;
    let code = StatusCode::from_u16(r.http_status()).unwrap_or(StatusCode::FORBIDDEN);
    state.refusal_log.log(peer, &format!("darkmux fleet: refused — {}", r.reason(receiver)));
    (code, Json(r.reply(receiver))).into_response()
}

/// Constant-time-ish comparison (same shape as the viewer's `tokens_match`).
fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    presented.iter().zip(expected).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

fn check_token(headers: &axum::http::HeaderMap, expected: Option<String>) -> TokenCheck {
    let Some(expected) = expected.filter(|t| !t.is_empty()) else {
        return TokenCheck::NotConfigured;
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(char::is_whitespace))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string());
    match presented {
        Some(p) if tokens_match(p.as_bytes(), expected.as_bytes()) => TokenCheck::Match,
        _ => TokenCheck::Mismatch,
    }
}

/// The gate on EVERY request: token, network identity, allow-list. A request
/// with no peer address (no `ConnectInfo`) is refused: absence of evidence
/// is not a peer.
async fn gate(State(state): State<FleetListenerState>, mut req: Request, next: Next) -> Response {
    let Some(peer) = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()) else {
        return refuse(
            &state,
            None,
            &Refusal::IdentityUnavailable {
                provider: state.provider.provider_name().to_string(),
                detail: "the connection carried no peer address".into(),
            },
        );
    };
    let token = check_token(req.headers(), (state.token)());
    // A caller without the token is answered before this machine reads its
    // allow-list or runs the provider's tool (`admit` checks it again).
    match token {
        TokenCheck::Match => {}
        TokenCheck::Mismatch => return refuse(&state, Some(peer), &Refusal::Token),
        TokenCheck::NotConfigured => return refuse(&state, Some(peer), &Refusal::NoTokenConfigured),
    }
    let allow = match (state.allow_list)() {
        Ok(a) => a,
        Err(e) => {
            return refuse(
                &state,
                Some(peer),
                &Refusal::BadRequest(format!("this machine's allow-list cannot be read ({e}); refusing everything")),
            )
        }
    };
    // The identity lookup is blocking (a subprocess) and runs only after the
    // token matched: `admit` calls this closure after its token check.
    let provider = state.provider.clone();
    let provider_name = provider.provider_name().to_string();
    let local_id = state.local_node_id.clone();
    let decision = tokio::task::spawn_blocking(move || {
        darkmux_fleet::admit(
            token,
            || provider.identify(peer).map_err(|e| format!("{e:#}")),
            &provider_name,
            peer,
            local_id.as_deref(),
            &allow,
        )
    })
    .await;
    match decision {
        Ok(Ok(admitted)) => {
            let Some(_node_slot) = state.node_slots.try_take(admitted.peer_name.clone()) else {
                return refuse(&state, Some(peer), &Refusal::TooManyAtOnce { peer: admitted.peer_name.clone() });
            };
            req.extensions_mut().insert(admitted);
            next.run(req).await
        }
        Ok(Err(refusal)) => refuse(&state, Some(peer), &refusal),
        Err(e) => refuse(
            &state,
            Some(peer),
            &Refusal::IdentityUnavailable {
                provider: state.provider.provider_name().to_string(),
                detail: format!("the identity check did not finish: {e}"),
            },
        ),
    }
}

/// Clears the busy slot when the job's worker ends, however it ends.
struct BusyGuard(Arc<Mutex<Option<String>>>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

async fn submit_handler(
    State(state): State<FleetListenerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Extension(admitted): Extension<Admitted>,
    body: Bytes,
) -> Response {
    let receiver = state.receiver.clone();
    let sub = match WorkSubmission::parse(&body) {
        Ok(s) => s,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };
    let mut job = sub.job;
    let resolve = state.resolve_profile.clone();
    let (role, requested) = (job.role_id.clone(), job.profile.clone());
    let resolution = match tokio::task::spawn_blocking(move || resolve(&role, requested.as_deref())).await {
        Ok(r) => r,
        Err(e) => ProfileResolution::Unresolved(format!("profile resolution did not finish: {e}")),
    };
    let profile = match darkmux_fleet::check_scope(&receiver, &admitted, &job, resolution) {
        Ok(p) => p,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };

    // (#2916 review C2) The receiver's own id for this run, never the
    // sender's verbatim.
    job.session_id = darkmux_fleet::receiver_session_id(&job.session_id, &admitted.peer_name);
    // (#2916 re-review C4) A submitted job is never attributed to one of
    // THIS machine's own missions: no allow-list scope grants that, so any
    // `phase_id` the sender set is dropped here.
    job.phase_id = None;

    // One submitted job at a time; a second is answered "busy" at once.
    {
        let mut slot = state.busy.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(running) = slot.as_ref() {
            return refuse(&state, Some(peer_addr.ip()), &Refusal::Busy { session_id: running.clone() });
        }
        *slot = Some(job.session_id.clone());
    }
    let guard = BusyGuard(state.busy.clone());
    let session_id = job.session_id.clone();
    eprintln!(
        "darkmux fleet: accepted {session_id} from {} (role {}, profile {profile})",
        admitted.peer_name, job.role_id
    );

    let (tx, rx) = tokio::sync::oneshot::channel();
    let execute = state.execute.clone();
    let run_profile = profile.clone();
    let origin = admitted.peer_name.clone();
    // A dedicated OS thread, not the async runtime: a dispatch blocks for
    // minutes. The guard moves in, so the slot frees when the work ends,
    // even if the sender stopped waiting.
    let spawned = std::thread::Builder::new().name("darkmux-fleet-job".into()).spawn(move || {
        let _guard = guard;
        let result = execute(job, run_profile, origin);
        let _ = tx.send(result);
    });
    if let Err(e) = spawned {
        return refuse(&state, Some(peer_addr.ip()), &Refusal::BadRequest(format!("could not start the job: {e}")));
    }

    let base = SubmissionReply {
        machine: Some(receiver.clone()),
        session_id: Some(session_id.clone()),
        profile: Some(profile),
        ..Default::default()
    };
    if !sub.wait {
        let reply = SubmissionReply { status: "accepted".into(), ..base };
        return (StatusCode::ACCEPTED, Json(reply)).into_response();
    }
    match rx.await {
        Ok(Ok(r)) => {
            let reply = SubmissionReply {
                status: "completed".into(),
                exit_code: Some(r.exit_code),
                stdout: Some(r.stdout),
                stderr: Some(r.stderr),
                ..base
            };
            (StatusCode::OK, Json(reply)).into_response()
        }
        Ok(Err(e)) => {
            let reply = SubmissionReply { status: "error".into(), reason: Some(format!("{e:#}")), ..base };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(reply)).into_response()
        }
        Err(_) => {
            let reply = SubmissionReply {
                status: "error".into(),
                reason: Some("the job's worker ended without a result".into()),
                ..base
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(reply)).into_response()
        }
    }
}

/// The address the listener binds: the provider's report of this machine's
/// own overlay address (IPv4 preferred), on `port`. Refuses anything that is
/// not a specific, non-loopback address — the listener must never answer on
/// `0.0.0.0` or on loopback (where every caller looks like this machine).
pub(crate) fn listen_addr(local: &darkmux_fleet::NodeIdentity, port: u16) -> Result<SocketAddr, String> {
    let ip = local
        .addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| local.addresses.first())
        .copied()
        .ok_or_else(|| "the identity provider reports no overlay address for this machine".to_string())?;
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return Err(format!("refusing to bind the fleet listener to {ip}: not a specific overlay address"));
    }
    Ok(SocketAddr::new(ip, port))
}

/// What the listener is doing, for `/health` and so for `darkmux doctor`
/// (#2916 review C8): a daemon started by launchd can fail where a shell
/// succeeds, and the reason used to live only in the daemon's log. Coarse
/// phrases only: no provider output, no ids.
static LISTENER_STATE: std::sync::Mutex<Option<(&'static str, String)>> = std::sync::Mutex::new(None);

/// `coarse` is one of `starting` / `waiting` / `listening` / `not started`;
/// `detail` may name the address and the reason.
fn set_state(coarse: &'static str, detail: impl Into<String>) {
    if let Ok(mut g) = LISTENER_STATE.lock() {
        *g = Some((coarse, detail.into()));
    }
}

/// The listener's state for `/health` (`None` = the listener is off):
/// the full detail for a loopback caller (this machine, e.g. `darkmux
/// doctor`), only the coarse word for anyone else (#2916 re-review C9), so
/// a peer learns neither the address nor why the listener is down.
pub(crate) fn listener_state(loopback_caller: bool) -> Option<String> {
    let g = LISTENER_STATE.lock().ok()?;
    let (coarse, detail) = g.as_ref()?;
    Some(if loopback_caller { detail.clone() } else { (*coarse).to_string() })
}

/// Bounds on the listener's connections (#2916 review M1). A tokenless
/// peer could open hundreds of half-sent requests and exhaust the daemon's
/// file descriptors, taking the VIEWER port down with the listener.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnLimits {
    /// Connections served at once; one more is closed on accept.
    pub max_conns: usize,
    /// Connections from ONE peer address at once (#2916 re-review MUST 1):
    /// one node cannot take every slot, and cannot run more than this many
    /// identity lookups at a time either.
    pub per_ip: usize,
    /// Time allowed to send the request headers.
    pub header_read_timeout: std::time::Duration,
    /// Longest a connection may live: a waiting submission holds its
    /// connection for the whole job, so this is the job cap plus slack.
    pub conn_deadline: std::time::Duration,
}

impl ConnLimits {
    pub(crate) const PRODUCTION: ConnLimits = ConnLimits {
        max_conns: 32,
        per_ip: 3,
        header_read_timeout: std::time::Duration::from_secs(3),
        conn_deadline: std::time::Duration::from_secs(60 * 60 + 300),
    };
}

/// Requests one admitted node may have in flight at once (#2916 round 3 C5).
pub(crate) const NODE_CAP: usize = 4;

/// In-flight counts per key (a peer address, or an admitted node); a slot
/// is released when its guard drops (the connection's task, or the
/// request, ends).
pub(crate) struct KeySlots<K: std::hash::Hash + Eq + Clone> {
    max: usize,
    counts: Mutex<std::collections::HashMap<K, usize>>,
}

pub(crate) struct KeySlot<K: std::hash::Hash + Eq + Clone> {
    owner: Arc<KeySlots<K>>,
    key: K,
}

impl<K: std::hash::Hash + Eq + Clone> Drop for KeySlot<K> {
    fn drop(&mut self) {
        let mut c = self.owner.counts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = c.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                c.remove(&self.key);
            }
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone> KeySlots<K> {
    pub(crate) fn new(max: usize) -> Self {
        Self { max, counts: Mutex::new(Default::default()) }
    }

    pub(crate) fn try_take(self: &Arc<Self>, key: K) -> Option<KeySlot<K>> {
        let mut c = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let n = c.entry(key.clone()).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(KeySlot { owner: Arc::clone(self), key })
    }
}

/// Serve `app` on `listener` with [`ConnLimits`]: a connection past the cap
/// is closed at once (so the listener never holds more than `max_conns`
/// descriptors), headers must arrive within `header_read_timeout`, one
/// request per connection, and no connection outlives `conn_deadline`. The
/// peer address is attached as `ConnectInfo` for the gate.
pub(crate) async fn serve_bounded(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: ConnLimits,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;
    let permits = Arc::new(tokio::sync::Semaphore::new(limits.max_conns));
    let per_ip = Arc::new(KeySlots::<std::net::IpAddr>::new(limits.per_ip));
    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("darkmux fleet: accept failed ({e}); pausing 100ms");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = async { let _ = shutdown.wait_for(|v| *v).await; } => return,
        };
        let Some(ip_slot) = per_ip.try_take(peer.ip().to_canonical()) else {
            drop(stream);
            continue;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let svc = TowerToHyperService::new(app.clone().layer(Extension(ConnectInfo(peer))));
        tokio::spawn(async move {
            let _permit = permit;
            let _ip_slot = ip_slot;
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc);
            let _ = tokio::time::timeout(limits.conn_deadline, conn).await;
        });
    }
}

/// Start the listener if `fleet.listener.enabled`. Never fails the daemon:
/// each reason it cannot start is logged, recorded for `/health`, and
/// reported by `darkmux doctor`. Waits for the provider to come up (it may
/// start after the daemon at boot), retrying every 30 s.
pub(crate) fn spawn_if_enabled(shutdown: tokio::sync::watch::Receiver<bool>) {
    if !darkmux_types::config_access::fleet_listener_enabled() {
        return;
    }
    set_state("starting", "starting");
    tokio::spawn(async move {
        if let Err(e) = run(shutdown).await {
            set_state("not started", format!("not started: {e}"));
            eprintln!("{}", darkmux_types::style::warn(&format!("darkmux fleet: listener not started: {e}")));
        }
    });
}

async fn run(mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<(), String> {
    if !darkmux_flow::serve_token_present() {
        return Err("no fleet token (the serve token: Keychain item `darkmux-serve-token` or \
                    DARKMUX_SERVE_TOKEN); a listener that cannot check a token takes no work"
            .into());
    }
    let provider: Arc<dyn IdentityProvider> =
        Arc::from(darkmux_fleet::configured_provider().map_err(|e| format!("{e:#}"))?);
    let receiver = darkmux_flow::resolve_machine_id()
        .ok_or_else(|| "this machine has no machine_id, so it cannot tell a misaddressed job".to_string())?;
    let port = darkmux_types::config_access::fleet_listener_port();
    let (addr, local_id) = loop {
        let p = provider.clone();
        match tokio::task::spawn_blocking(move || p.local_node()).await {
            Ok(Ok(local)) => break (listen_addr(&local, port)?, local.node_id.clone()),
            Ok(Err(e)) => {
                set_state("waiting", format!("waiting for the {} network to answer (retrying every 30s)", provider.provider_name()));
                eprintln!(
                    "darkmux fleet: the {} network is not answering ({e:#}); retrying in 30s",
                    provider.provider_name()
                )
            }
            Err(e) => eprintln!("darkmux fleet: identity check failed ({e}); retrying in 30s"),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            _ = async { let _ = shutdown.wait_for(|v| *v).await; } => return Ok(()),
        }
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("binding {addr}: {e} (another process on `fleet.listener.port`?)"))?;
    set_state("listening", format!("listening on {addr}"));
    println!("  fleet listener: {addr} (work submission; identity: {})", provider.provider_name());
    let state = FleetListenerState::production(receiver, provider, Some(local_id));
    // (#2916 round 3 C6) Suppressed refusal counts are reported every minute,
    // not only when the next refusal arrives.
    let log = state.refusal_log.clone();
    let mut flush_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RefusalLog::WINDOW);
        loop {
            tokio::select! {
                _ = tick.tick() => { log.flush(std::time::Instant::now()); }
                _ = async { let _ = flush_shutdown.wait_for(|v| *v).await; } => return,
            }
        }
    });
    let app = router(state);
    serve_bounded(listener, app, ConnLimits::PRODUCTION, shutdown).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_fleet::{test_node, StaticIdentityProvider};
    use std::time::Duration;

    const TOKEN: &str = "fleet-test-token";

    fn allow() -> AllowList {
        let mut m = AllowList::new();
        m.insert(
            "macbook-pro".into(),
            AcceptWorkEntry {
                node_id: Some("nLAPTOP".into()),
                profiles: Some(vec!["host".into()]),
                roles: Some(vec!["radio-host".into()]),
                images: None,
                workspace: Some(false),
                extras: Default::default(),
            },
        );
        m
    }

    /// A real listener on 127.0.0.1 whose fake provider says the loopback
    /// peer is `peer` (or nobody, or is down). Returns the URL and a counter
    /// of executed jobs.
    struct Harness {
        refusal_log: Arc<RefusalLog>,
        url: String,
        ran: Arc<Mutex<Vec<(String, String)>>>,
        busy: Arc<Mutex<Option<String>>>,
    }

    fn start(peer: Option<darkmux_fleet::NodeIdentity>, down: bool, job_ms: u64) -> Harness {
        let local = test_node("nSTUDIO", "studio", "100.64.0.2");
        let provider = StaticIdentityProvider {
            local,
            peers: peer.into_iter().collect(),
            down: down.then(|| "daemon not running".to_string()),
        };
        let ran = Arc::new(Mutex::new(Vec::new()));
        let ran_c = ran.clone();
        let busy = Arc::new(Mutex::new(None));
        let refusal_log = Arc::new(RefusalLog::new());
        let state = FleetListenerState {
            receiver: "studio".into(),
            provider: Arc::new(provider),
            local_node_id: Some("nSTUDIO".into()),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_role, requested| match requested {
                Some("utility") => ProfileResolution::UtilityOnly("utility".into()),
                Some(p) => ProfileResolution::Work(p.to_string()),
                None => ProfileResolution::Work("host".into()),
            }),
            execute: Arc::new(move |job: WorkJob, profile: String, _origin: String| {
                std::thread::sleep(Duration::from_millis(job_ms));
                assert!(job.phase_id.is_none(), "a submitted job's phase_id must be dropped (#2916 re-review C4)");
                ran_c.lock().unwrap().push((job.session_id.clone(), profile.clone()));
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: format!("ran {} on {profile}", job.role_id),
                    stderr: String::new(),
                    session_id: job.session_id,
                    out_dir: None,
                })
            }),
            busy: busy.clone(),
            refusal_log: refusal_log.clone(),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
        };
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                let (_tx, rx) = tokio::sync::watch::channel(false);
                serve_bounded(l, router(state), ConnLimits::PRODUCTION, rx).await;
            });
        });
        Harness { refusal_log, url: format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH), ran, busy }
    }

    fn laptop() -> darkmux_fleet::NodeIdentity {
        test_node("nLAPTOP", "macbook-pro", "127.0.0.1")
    }

    fn job(session: &str, profile: Option<&str>) -> WorkJob {
        darkmux_fleet::build_work_job(
            "studio".into(),
            "radio-host".into(),
            "hello".into(),
            session.into(),
            profile.map(str::to_string),
            None,
            Some("receivers-own-phase".into()),
            None,
            60,
            Some("macbook-pro".into()),
        )
    }

    fn post(h: &Harness, token: &str, j: WorkJob, wait: bool) -> (u16, SubmissionReply) {
        darkmux_fleet::post_submission(&h.url, token, &WorkSubmission::new(j, wait), Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn an_allowed_peer_with_the_token_runs_in_scope_work_and_gets_the_result() {
        let h = start(Some(laptop()), false, 0);
        let (code, reply) = post(&h, TOKEN, job("s-ok", None), true);
        assert_eq!(code, 200, "{reply:?}");
        assert_eq!(reply.status, "completed");
        assert_eq!(reply.exit_code, Some(0));
        assert_eq!(reply.stdout.as_deref(), Some("ran radio-host on host"));
        assert_eq!(reply.profile.as_deref(), Some("host"));
        assert_eq!(reply.session_id.as_deref(), Some("s-ok-from-macbook-pro"), "the receiver's own session id");
        assert_eq!(h.ran.lock().unwrap().as_slice(), &[("s-ok-from-macbook-pro".to_string(), "host".to_string())]);
    }

    /// Every refusal over real HTTP: immediate, with the reason, and nothing
    /// runs.
    #[test]
    fn refusals_come_back_at_once_with_the_reason() {
        let h = start(Some(laptop()), false, 0);
        let cases: Vec<(&str, WorkJob, u16, &str)> = vec![
            ("wrong-token", job("s1", None), 401, "fleet token is missing or does not match"),
            (TOKEN, job("s2", Some("coder-big")), 403, "not in the allow-list scope: profile coder-big"),
            (TOKEN, job("s3", Some("utility")), 403, "utility model"),
            (TOKEN, { let mut j = job("s4", None); j.workdir = Some("/x".into()); j }, 403, "working directory"),
            (TOKEN, { let mut j = job("s5", None); j.target_machine = "mini".into(); j }, 421, "this is studio, not mini"),
            (TOKEN, { let mut j = job("s6", None); j.role_id = "coder".into(); j }, 403, "not in the allow-list scope: role coder"),
            (TOKEN, { let mut j = job("s7", None); j.image = Some("evil.example/x".into()); j }, 403, "not in the allow-list scope: image"),
        ];
        for (token, j, want_code, want_reason) in cases {
            let (code, reply) = post(&h, token, j, true);
            assert_eq!(code, want_code, "{reply:?}");
            assert_eq!(reply.status, "refused");
            assert!(reply.reason.as_deref().unwrap_or("").contains(want_reason), "{reply:?}");
        }
        assert!(h.ran.lock().unwrap().is_empty(), "a refused request must never run");
    }

    #[test]
    fn a_node_not_on_the_allow_list_is_refused_by_name() {
        let h = start(Some(test_node("nPHONE", "phone", "127.0.0.1")), false, 0);
        let (code, reply) = post(&h, TOKEN, job("s", None), true);
        assert_eq!(code, 403);
        assert!(reply.reason.unwrap().starts_with("studio does not accept work from phone"));
        assert!(h.ran.lock().unwrap().is_empty());
    }

    #[test]
    fn a_connection_the_provider_cannot_place_is_refused() {
        let h = start(None, false, 0);
        let (code, reply) = post(&h, TOKEN, job("s", None), true);
        assert_eq!(code, 403);
        assert!(reply.reason.unwrap().contains("did not come from a node"), "not on the overlay");
        let h = start(Some(laptop()), true, 0);
        let (code, reply) = post(&h, TOKEN, job("s", None), true);
        assert_eq!(code, 503);
        assert!(reply.reason.unwrap().contains("cannot tell which machine sent this request"));
        assert!(h.ran.lock().unwrap().is_empty());
    }

    /// (#2916 review C4) A request from this machine's OWN node is refused,
    /// even when a (mistaken) allow-list entry names it.
    #[test]
    fn a_request_from_this_machines_own_node_is_refused() {
        let h = start(Some(test_node("nSTUDIO", "studio", "127.0.0.1")), false, 0);
        let (code, reply) = post(&h, TOKEN, job("s", None), true);
        // `start`'s allow-list does not list nSTUDIO; a hand-edited one
        // might, so check the refusal names self regardless of the list.
        assert_eq!(code, 403, "{reply:?}");
        assert!(reply.reason.unwrap().contains("does not take fleet work from itself"));
    }

    /// Every path on this listener is gated, not only the submission route.
    #[test]
    fn the_gate_covers_every_path() {
        let h = start(None, false, 0);
        let other = h.url.replace(darkmux_fleet::SUBMISSION_PATH, "/health");
        let resp = ureq::get(&other).set("Authorization", &format!("Bearer {TOKEN}")).call();
        match resp {
            Err(ureq::Error::Status(code, _)) => assert_eq!(code, 403),
            other => panic!("an ungated path answered: {other:?}"),
        }
    }

    #[test]
    fn a_busy_machine_says_so_at_once_and_frees_the_slot_when_done() {
        let h = start(Some(laptop()), false, 600);
        let (code, reply) = post(&h, TOKEN, job("s-long", None), false);
        assert_eq!(code, 202, "{reply:?}");
        assert_eq!(reply.status, "accepted");
        let started = std::time::Instant::now();
        let (code, reply) = post(&h, TOKEN, job("s-second", None), true);
        assert_eq!(code, 503);
        assert!(reply.reason.unwrap().contains("busy running s-long-from-macbook-pro"));
        assert!(started.elapsed() < Duration::from_millis(500), "busy is answered at once, not queued");
        // The slot frees when the first job ends.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.busy.lock().unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "the busy slot never freed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (code, _) = post(&h, TOKEN, job("s-third", None), true);
        assert_eq!(code, 200);
    }

    /// A caller without the token is answered before the allow-list is
    /// read or the provider runs.
    #[test]
    fn a_caller_without_the_token_reads_nothing() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = reads.clone();
        let provider = StaticIdentityProvider {
            local: test_node("nSTUDIO", "studio", "100.64.0.2"),
            peers: vec![laptop()],
            down: None,
        };
        let state = FleetListenerState {
            receiver: "studio".into(),
            provider: Arc::new(provider),
            local_node_id: Some("nSTUDIO".into()),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(move || {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allow())
            }),
            resolve_profile: Arc::new(|_, _| ProfileResolution::Work("host".into())),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            busy: Arc::new(Mutex::new(None)),
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let resp = rt.block_on(async {
            use tower::ServiceExt;
            let body = serde_json::to_vec(&WorkSubmission::new(job("s", None), true)).unwrap();
            let mut req = axum::http::Request::post(darkmux_fleet::SUBMISSION_PATH)
                .header("Authorization", "Bearer wrong")
                .body(axum::body::Body::from(body))
                .unwrap();
            req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
            router(state).oneshot(req).await.unwrap()
        });
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0, "the allow-list was read for a caller without the token");
    }

    /// A request with no peer address is refused (fail closed), even with
    /// the right token.
    #[test]
    fn a_request_without_a_peer_address_is_refused() {
        let h_state = FleetListenerState {
            receiver: "studio".into(),
            local_node_id: Some("nSTUDIO".into()),
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| ProfileResolution::Work("host".into())),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            busy: Arc::new(Mutex::new(None)),
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let resp = rt.block_on(async {
            use tower::ServiceExt;
            let body = serde_json::to_vec(&WorkSubmission::new(job("s", None), true)).unwrap();
            let req = axum::http::Request::post(darkmux_fleet::SUBMISSION_PATH)
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(axum::body::Body::from(body))
                .unwrap();
            router(h_state).oneshot(req).await.unwrap()
        });
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// (#2916 review M1) Bounded connections: a half-sent request is closed
    /// once the header deadline passes, connections past the cap are closed
    /// on accept, and once the slots free a real request is served.
    #[test]
    fn half_open_connections_are_bounded_and_timed_out() {
        use std::io::{Read, Write};
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let limits = ConnLimits {
            max_conns: 4,
            per_ip: 8,
            header_read_timeout: Duration::from_millis(800),
            conn_deadline: Duration::from_secs(30),
        };
        let app = Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                let (_tx, rx) = tokio::sync::watch::channel(false);
                serve_bounded(l, app, limits, rx).await;
            });
        });
        let closed_within = |s: &mut std::net::TcpStream, d: Duration| -> bool {
            s.set_read_timeout(Some(d)).unwrap();
            let mut b = [0u8; 256];
            loop {
                match s.read(&mut b) {
                    Ok(0) => return true,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return true,
                    Err(_) => return false,
                }
            }
        };
        let mut held = Vec::new();
        for _ in 0..4 {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.write_all(b"POST /fleet/work HTTP/1.1\r\nHost: x\r\n").unwrap();
            held.push(s);
        }
        std::thread::sleep(Duration::from_millis(100));
        // A fifth connection is past the cap: closed at once.
        let mut extra = std::net::TcpStream::connect(addr).unwrap();
        // A half-sent request: served, it would be held until the 800 ms
        // header deadline; past the cap, it is closed at once.
        let _ = extra.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n");
        assert!(closed_within(&mut extra, Duration::from_millis(400)), "a connection past the cap must be closed on accept");
        // The half-sent ones are closed by the header deadline.
        for s in held.iter_mut() {
            assert!(closed_within(s, Duration::from_secs(3)), "a half-sent request must be closed after the header deadline");
        }
        // Slots are free again: a real request is served.
        std::thread::sleep(Duration::from_millis(100));
        let body = ureq::get(&format!("http://{addr}/ok")).call().unwrap().into_string().unwrap();
        assert_eq!(body, "ok");
    }

    /// (#2916 re-review MUST 1) One address holding its per-IP cap cannot
    /// block a request from another address.
    #[test]
    fn one_address_at_its_cap_cannot_block_another() {
        use std::io::{Read, Write};
        let Ok(std_listener) = std::net::TcpListener::bind("[::]:0") else { return };
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        let limits = ConnLimits {
            max_conns: 8,
            per_ip: 2,
            header_read_timeout: Duration::from_secs(5),
            conn_deadline: Duration::from_secs(30),
        };
        let app = Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                let (_tx, rx) = tokio::sync::watch::channel(false);
                serve_bounded(l, app, limits, rx).await;
            });
        });
        let v4: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let v6: SocketAddr = format!("[::1]:{port}").parse().unwrap();
        let mut held = Vec::new();
        for _ in 0..2 {
            let mut s = std::net::TcpStream::connect(v4).unwrap();
            s.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n").unwrap();
            held.push(s);
        }
        std::thread::sleep(Duration::from_millis(150));
        // A third from the same address is closed at once.
        let mut third = std::net::TcpStream::connect(v4).unwrap();
        let _ = third.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n");
        third.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
        let mut b = [0u8; 64];
        let closed = match third.read(&mut b) {
            Ok(0) => true,
            Err(e) => e.kind() == std::io::ErrorKind::ConnectionReset,
            Ok(_) => false,
        };
        assert!(closed, "a connection past the per-address cap is closed on accept");
        // Another address is served.
        let Ok(other) = std::net::TcpStream::connect(v6) else { return };
        drop(other);
        let body = ureq::get(&format!("http://[::1]:{port}/ok")).call().unwrap().into_string().unwrap();
        assert_eq!(body, "ok");
    }

    /// (#2916 re-review MUST 2) Refusal logging: the first few per address
    /// per minute, then counted, and the count reported once.
    #[test]
    fn refusal_logging_is_throttled_per_address() {
        let log = RefusalLog::new();
        let a: std::net::IpAddr = "100.64.0.7".parse().unwrap();
        let b: std::net::IpAddr = "100.64.0.8".parse().unwrap();
        let t0 = std::time::Instant::now();
        for _ in 0..RefusalLog::PER_WINDOW {
            assert_eq!(log.decide(a, t0), LogDecision::Write(None));
        }
        for _ in 0..10 {
            assert_eq!(log.decide(a, t0), LogDecision::Suppress);
        }
        assert_eq!(log.decide(b, t0), LogDecision::Write(None), "another address has its own budget");
        assert_eq!(log.decide(a, t0 + RefusalLog::WINDOW), LogDecision::Write(Some(10)));
        // Wired into the listener: 20 tokenless requests, 5 lines.
        let h = start(Some(laptop()), false, 0);
        for i in 0..20 {
            let (code, _) = post(&h, "wrong", job(&format!("s{i}"), None), true);
            assert_eq!(code, 401);
        }
        assert_eq!(h.refusal_log.written.load(std::sync::atomic::Ordering::SeqCst), u64::from(RefusalLog::PER_WINDOW));
    }

    /// (#2916 re-review C5) The per-address cap also bounds how many
    /// identity lookups one address (holding the token) runs at once.
    #[test]
    fn one_address_runs_at_most_its_cap_of_identity_lookups() {
        struct Slow {
            inner: StaticIdentityProvider,
            now: std::sync::atomic::AtomicUsize,
            max: std::sync::atomic::AtomicUsize,
        }
        impl IdentityProvider for Slow {
            fn provider_name(&self) -> &str {
                "slow"
            }
            fn identify(&self, peer: std::net::IpAddr) -> anyhow::Result<Option<darkmux_fleet::NodeIdentity>> {
                use std::sync::atomic::Ordering::SeqCst;
                let n = self.now.fetch_add(1, SeqCst) + 1;
                self.max.fetch_max(n, SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                self.now.fetch_sub(1, SeqCst);
                self.inner.identify(peer)
            }
            fn local_node(&self) -> anyhow::Result<darkmux_fleet::NodeIdentity> {
                self.inner.local_node()
            }
            fn nodes(&self) -> anyhow::Result<Vec<darkmux_fleet::NodeIdentity>> {
                self.inner.nodes()
            }
        }
        let slow = Arc::new(Slow {
            inner: StaticIdentityProvider { local: test_node("nSTUDIO", "studio", "100.64.0.2"), peers: vec![], down: None },
            now: Default::default(),
            max: Default::default(),
        });
        let state = FleetListenerState {
            receiver: "studio".into(),
            local_node_id: Some("nSTUDIO".into()),
            provider: slow.clone(),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| ProfileResolution::Work("host".into())),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            busy: Arc::new(Mutex::new(None)),
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
        };
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                let (_tx, rx) = tokio::sync::watch::channel(false);
                serve_bounded(l, router(state), ConnLimits::PRODUCTION, rx).await;
            });
        });
        let url = format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH);
        let threads: Vec<_> = (0..10)
            .map(|i| {
                let url = url.clone();
                std::thread::spawn(move || {
                    let _ = darkmux_fleet::post_submission(&url, TOKEN, &WorkSubmission::new(job(&format!("s{i}"), None), true), Duration::from_secs(10));
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
        let max = slow.max.load(std::sync::atomic::Ordering::SeqCst);
        assert!((1..=ConnLimits::PRODUCTION.per_ip).contains(&max), "concurrent lookups from one address: {max}");
    }

    /// (#2916 round 3 C5) Once identified, one node has at most its cap of
    /// requests in flight, whatever addresses they came from.
    #[test]
    fn one_node_has_at_most_its_cap_of_requests_in_flight() {
        let state = FleetListenerState {
            receiver: "studio".into(),
            local_node_id: Some("nSTUDIO".into()),
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| {
                std::thread::sleep(Duration::from_millis(600));
                ProfileResolution::Work("host".into())
            }),
            execute: Arc::new(|j: WorkJob, _, _| {
                Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: j.session_id, out_dir: None })
            }),
            busy: Arc::new(Mutex::new(None)),
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(1)),
        };
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                let (_tx, rx) = tokio::sync::watch::channel(false);
                serve_bounded(l, router(state), ConnLimits::PRODUCTION, rx).await;
            });
        });
        let url = format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH);
        let u2 = url.clone();
        let first = std::thread::spawn(move || {
            darkmux_fleet::post_submission(&u2, TOKEN, &WorkSubmission::new(job("s1", None), true), Duration::from_secs(10)).unwrap()
        });
        std::thread::sleep(Duration::from_millis(200));
        let (code, reply) =
            darkmux_fleet::post_submission(&url, TOKEN, &WorkSubmission::new(job("s2", None), true), Duration::from_secs(10)).unwrap();
        assert_eq!(code, 503, "{reply:?}");
        assert!(reply.reason.unwrap().contains("as many requests from macbook-pro as it takes at once"));
        assert_eq!(first.join().unwrap().0, 200);
    }

    /// (#2916 round 3 C6) Suppressed counts are flushed on a timer, and
    /// refusals from addresses past the table's size are counted, not lost.
    #[test]
    fn suppressed_counts_are_flushed_and_overflow_is_counted() {
        let log = RefusalLog::new();
        let a: std::net::IpAddr = "100.64.0.7".parse().unwrap();
        let t0 = std::time::Instant::now();
        for _ in 0..(RefusalLog::PER_WINDOW + 3) {
            log.decide(a, t0);
        }
        assert!(log.flush(t0).is_empty(), "nothing to report before the minute ends");
        let lines = log.flush(t0 + RefusalLog::WINDOW);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("suppressed 3"), "{lines:?}");
        assert!(log.flush(t0 + RefusalLog::WINDOW * 2).is_empty(), "reported once");
        let full = RefusalLog::new();
        for i in 0..RefusalLog::MAX_PEERS as u32 {
            full.decide(std::net::IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + i)), t0);
        }
        assert_eq!(full.decide("100.64.9.9".parse().unwrap(), t0), LogDecision::Suppress);
        let lines = full.flush(t0);
        assert!(lines.iter().any(|l| l.contains("suppressed 1") && l.contains("beyond")), "{lines:?}");
    }

    #[test]
    fn a_peer_sees_only_the_coarse_listener_state() {
        set_state("listening", "listening on 100.64.0.2:8766");
        assert_eq!(listener_state(false).as_deref(), Some("listening"));
        assert_eq!(listener_state(true).as_deref(), Some("listening on 100.64.0.2:8766"));
    }

    #[test]
    fn the_open_file_limit_is_raised_never_lowered() {
        let (old, new) = crate::raise_open_file_limit(1).expect("getrlimit works");
        assert_eq!(new, old, "a target below the current soft limit changes nothing");
    }

    #[test]
    fn the_listener_binds_only_a_specific_overlay_address() {
        let local = test_node("n", "studio", "100.64.0.2");
        assert_eq!(listen_addr(&local, 8766).unwrap().to_string(), "100.64.0.2:8766");
        for bad in ["0.0.0.0", "127.0.0.1", "::"] {
            let n = test_node("n", "studio", bad);
            assert!(listen_addr(&n, 8766).is_err(), "{bad}");
        }
        let mut none = local.clone();
        none.addresses.clear();
        assert!(listen_addr(&none, 8766).is_err());
    }
}
