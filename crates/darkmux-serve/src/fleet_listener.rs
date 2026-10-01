//! The work-submission listener (#2916, stage 1).
//!
//! A second listener beside the viewer port, bound ONLY to the overlay
//! address the identity provider reports for this machine (never `0.0.0.0`,
//! never a LAN address, never loopback), so every connection it accepts has a
//! real peer address the provider can name. It is deliberately not behind
//! `tailscale serve`: that front proxies to loopback, so every peer would
//! arrive as 127.0.0.1 and could not be told apart.
//!
//! Two routes, `POST /fleet/work` (a job, or a check of one: `mode: check`
//! answers what a run would meet and runs nothing) and `GET /fleet/card` (this
//! machine's card, and what this machine lets the caller do), and every
//! request on this listener (any path) is AUTHENTICATED first
//! ([`darkmux_fleet::authenticate`]): the fleet token, then the connecting
//! node (the provider's answer for the socket's peer address), then "not this
//! machine". That is all a card read needs: a machine is visible to every
//! fleet node that authenticates, whether or not it lets that node run work,
//! and the grant is reported as data ([`crate::machine_card::CardGrant`]).
//! A job is then AUTHORIZED ([`darkmux_fleet::authorize`], the allow-list) by
//! the submission handler; the scope check after parsing is
//! [`darkmux_fleet::check_scope`]. All are pure and table-tested in
//! `darkmux-fleet`; this module is the wiring, tested over real HTTP with a
//! fake provider.
//!
//! **Cost (#2916 self-QA, #3004).** The identity lookup runs the provider's
//! tool (~25 ms measured on the laptop, 2026-09-27). For a JOB it runs on
//! every request, uncached: a submitted job occupies the machine for minutes,
//! and without a cache a node leaving the network takes effect on the very
//! next request. For a CARD read, which every peer's fleet view repeats about
//! every five seconds, the answer is kept per address for
//! [`IdentityCache::TTL`], and the card itself is served from a
//! [`crate::machine_card::CardCache`], so a card read costs neither a
//! provider call nor a specs gather while the caches are warm. The allow-list
//! is read from `config.json` on every authorization (a few KB), so `machine
//! trust` / `untrust` need no daemon restart. A caller without the token costs
//! one constant-time compare and never makes this machine spawn anything.
//! Refusal log lines are throttled per address ([`RefusalLog`]: five a minute,
//! the rest counted), so a caller with the wrong token cannot grow this
//! machine's log.

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
    Admitted, CheckReport, EndpointClass, IdentityProvider, NodeIdentity, Occupied, ProfileResolution, Refusal,
    ReplyStatus, ScopedJob, SeatBook, SeatGuard, SeatOutlook, SubmissionMode, SubmissionReply, TokenCheck, Waited,
    WorkJob, WorkSeat, WorkSubmission,
};
use darkmux_types::config::{AcceptWorkEntry, BusyPolicy};
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
    /// This machine's hardware uid, when it has one (#3028): a job that
    /// carries its target's uid is checked against this, not the name.
    pub receiver_uid: Option<String>,
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
    /// (#2916 stage 2) The submitted jobs running now, by the seat each
    /// holds: one per local model, hosted ones up to `remote.concurrent_cap`.
    pub seats: Arc<SeatBook>,
    /// (#2916 stage 2) What a job whose seat is busy gets: `refuse` or
    /// `queue` (`fleet.busy_policy`, read once at start).
    pub busy_policy: BusyPolicy,
    /// Queued jobs per sending node, capped like its requests in flight
    /// ([`NODE_CAP`]), so a queue cannot grow without bound.
    pub queue_slots: Arc<KeySlots<String>>,
    /// How often a waiting sender hears that its job is still queued.
    pub queue_heartbeat: std::time::Duration,
    /// How long a queued job may wait for its seat ([`queue_deadline`]).
    pub queue_limits: QueueLimits,
    /// Per-peer throttle on refusal log lines.
    pub refusal_log: Arc<RefusalLog>,
    /// (#2916 round 3 C5) Requests in flight per admitted NODE, so a node
    /// that spreads connections over several addresses (a subnet router)
    /// is still capped once it is identified.
    pub node_slots: Arc<KeySlots<String>>,
    /// Card reads in flight per admitted NODE ([`CARD_NODE_CAP`]). A card read
    /// is not a job: it takes a slot of its own, so a sender whose jobs fill
    /// its [`node_slots`](Self::node_slots) still gets its card, and a poller
    /// still cannot ask for more than a few at a time.
    pub card_slots: Arc<KeySlots<String>>,
    /// (#2947) This machine's dispatch-scope config preflight, run per
    /// submission before the job is accepted. `Err` carries the refusal
    /// text. Production: `darkmux_crew::user_files::preflight(Scope::Dispatch)`.
    pub config_preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// (#3004) This machine's gathered card, kept for its TTL so a burst of
    /// card reads costs one gather.
    pub card_cache: Arc<crate::machine_card::CardCache>,
    /// (#3004) Identity lookups for card reads, kept per address for a few
    /// seconds. A job's admission never reads it.
    pub identity_cache: Arc<IdentityCache>,
}

/// (#2947) The production config preflight a submission runs.
pub(crate) fn dispatch_config_preflight() -> Result<(), String> {
    darkmux_crew::user_files::preflight(darkmux_types::config_enum::Scope::Dispatch).map_err(|e| e.to_string())
}

impl FleetListenerState {
    /// The listener's admission inputs, as [`Admission`] takes them.
    pub(crate) fn admission(&self) -> Admission {
        let local = self.local_node_id.clone();
        Admission {
            provider: self.provider.clone(),
            local_node_id: Arc::new(move || local.clone()),
            token: self.token.clone(),
            allow_list: self.allow_list.clone(),
            identity_cache: self.identity_cache.clone(),
        }
    }

    /// Production wiring: the configured provider, the serve token, the
    /// allow-list from `config.json`, this machine's registry, and
    /// `darkmux_fleet::execute_job`.
    pub(crate) fn production(
        receiver: String,
        provider: Arc<dyn IdentityProvider>,
        local_node_id: Option<String>,
        busy_policy: BusyPolicy,
    ) -> Self {
        let resolve_receiver = receiver.clone();
        Self {
            receiver,
            receiver_uid: darkmux_hardware::machine_uid().map(str::to_string),
            provider,
            local_node_id,
            token: Arc::new(|| darkmux_flow::serve_token().map(|t| t.expose_for_compare().to_string())),
            allow_list: Arc::new(darkmux_fleet::read_user_allow_list),
            resolve_profile: Arc::new(move |role, requested| {
                darkmux_fleet::resolve_work_profile(role, requested, &resolve_receiver)
            }),
            execute: Arc::new(darkmux_fleet::execute_job),
            seats: Arc::new(SeatBook::new(darkmux_types::config_access::remote_concurrent_cap())),
            busy_policy,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            config_preflight: Arc::new(dispatch_config_preflight),
            card_cache: crate::machine_card::process_card_cache(),
            identity_cache: Arc::new(IdentityCache::new()),
        }
    }
}

/// The listener's router: work submission and the card read, both behind the
/// gate.
pub(crate) fn router(state: FleetListenerState) -> Router {
    Router::new()
        .route(darkmux_fleet::SUBMISSION_PATH, axum::routing::post(submit_route))
        .route(darkmux_fleet::CARD_PATH, axum::routing::get(card_handler))
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
                        lines.push(format!("darkmux serve: fleet listener: suppressed {sup} refusal log line(s) from {ip} in the last minute"));
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
                "darkmux serve: fleet listener: suppressed {over} refusal log line(s) from addresses beyond the {} tracked",
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
                eprintln!("darkmux serve: fleet listener: suppressed {k} refusal log line(s) from {ip} in the last minute");
            }
            eprintln!("{line}");
            self.written.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn refuse(state: &FleetListenerState, peer: Option<std::net::IpAddr>, r: &Refusal) -> Response {
    let receiver = &state.receiver;
    let code = StatusCode::from_u16(r.http_status()).unwrap_or(StatusCode::FORBIDDEN);
    state.refusal_log.log(peer, &format!("darkmux serve: fleet listener: refused — {}", r.reason(receiver)));
    (code, Json(r.reply(receiver))).into_response()
}

/// (#2916 stage 2 review F1) A hash of the fleet token a request was
/// admitted with. Kept with a queued job so the token can be checked again
/// when its seat frees, without keeping the token itself.
#[derive(Clone, Copy)]
pub(crate) struct TokenFingerprint([u8; 32]);

impl TokenFingerprint {
    pub(crate) fn of(token: &str) -> Self {
        Self(*blake3::hash(token.as_bytes()).as_bytes())
    }

    /// The token gate against the token in force NOW: the same token the
    /// request was admitted with, a different one (rotated), or none.
    fn check_now(&self, now: Option<String>) -> TokenCheck {
        match now.filter(|t| !t.is_empty()) {
            None => TokenCheck::NotConfigured,
            Some(t) if tokens_match(&Self::of(&t).0, &self.0) => TokenCheck::Match,
            Some(_) => TokenCheck::Mismatch,
        }
    }
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

/// Why a request is being admitted: a card read may be served from a recent
/// identity lookup, a job may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Card,
    Work,
}

/// Identity lookups kept per peer address, for card reads only. Only a lookup
/// that named a node is kept: an outage or an unplaced address is asked again
/// the next time, so a recovered provider is seen at once.
pub(crate) struct IdentityCache {
    slots: Mutex<std::collections::HashMap<std::net::IpAddr, (std::time::Instant, NodeIdentity)>>,
}

impl IdentityCache {
    /// How long a lookup is served to card reads. A card read states a card
    /// and a grant to a caller that already holds the fleet token; a job never
    /// reads this cache.
    pub(crate) const TTL: std::time::Duration = std::time::Duration::from_secs(10);
    /// The most addresses kept: a full table stops caching, it does not evict.
    const MAX: usize = 256;

    pub(crate) fn new() -> Self {
        Self { slots: Mutex::new(Default::default()) }
    }

    fn get(&self, ip: std::net::IpAddr, now: std::time::Instant) -> Option<NodeIdentity> {
        let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        slots.get(&ip).filter(|(at, _)| now.duration_since(*at) < Self::TTL).map(|(_, n)| n.clone())
    }

    fn put(&self, ip: std::net::IpAddr, node: NodeIdentity, now: std::time::Instant) {
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        if slots.len() >= Self::MAX && !slots.contains_key(&ip) {
            slots.retain(|_, (at, _)| now.duration_since(*at) < Self::TTL);
            if slots.len() >= Self::MAX {
                return;
            }
        }
        slots.insert(ip, (now, node));
    }
}

/// The two steps that make a caller a fleet peer this machine works for:
/// [`Admission::authenticate`] (the fleet token, the connecting node as the
/// overlay network names it, not this machine) and [`Admission::authorize`]
/// (the allow-list). The gate runs the first for every request; a job runs the
/// second as well, and a card read reports its result as data.
#[derive(Clone)]
pub(crate) struct Admission {
    pub provider: Arc<dyn IdentityProvider>,
    /// This machine's own node id, read per request: a request from it is
    /// refused.
    pub local_node_id: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// The expected fleet token, read per request (`None` = not configured).
    pub token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// The allow-list, read per request. An error refuses everything.
    pub allow_list: Arc<dyn Fn() -> Result<AllowList, String> + Send + Sync>,
    pub identity_cache: Arc<IdentityCache>,
}

impl Admission {
    /// The identity lookup for `peer`: the provider's answer, or for a card
    /// read a recent one.
    fn identify(&self, peer: std::net::IpAddr, purpose: Purpose) -> Result<Option<NodeIdentity>, String> {
        let now = std::time::Instant::now();
        if purpose == Purpose::Card {
            if let Some(node) = self.identity_cache.get(peer, now) {
                return Ok(Some(node));
            }
        }
        let answer = self.provider.identify(peer).map_err(|e| format!("{e:#}"));
        if let (Purpose::Card, Ok(Some(node))) = (purpose, &answer) {
            self.identity_cache.put(peer, node.clone(), now);
        }
        answer
    }

    /// Token, then the connecting node, then "not this machine", in that
    /// order. A caller without the token is answered before this machine runs
    /// the provider's tool. On success the fingerprint of the token in force
    /// comes back with the node, so a queued job can be checked against it
    /// again later.
    pub(crate) async fn authenticate(
        &self,
        peer: std::net::IpAddr,
        headers: &axum::http::HeaderMap,
        purpose: Purpose,
    ) -> Result<(NodeIdentity, TokenFingerprint), Refusal> {
        let expected = (self.token)();
        let token = check_token(headers, expected.clone());
        let fingerprint = match (token, expected.as_deref()) {
            (TokenCheck::Match, Some(t)) => TokenFingerprint::of(t),
            (TokenCheck::Mismatch, _) => return Err(Refusal::Token),
            (TokenCheck::Match, None) | (TokenCheck::NotConfigured, _) => return Err(Refusal::NoTokenConfigured),
        };
        // The identity lookup is blocking (a subprocess) and runs only after
        // the token matched: `authenticate` calls this closure after its own
        // token check.
        let this = self.clone();
        let provider_name = self.provider.provider_name().to_string();
        let decision = tokio::task::spawn_blocking(move || {
            let local_id = (this.local_node_id)();
            let name = this.provider.provider_name().to_string();
            darkmux_fleet::authenticate(token, || this.identify(peer, purpose), &name, peer, local_id.as_deref())
        })
        .await;
        match decision {
            Ok(Ok(node)) => Ok((node, fingerprint)),
            Ok(Err(refusal)) => Err(refusal),
            Err(e) => Err(Refusal::IdentityUnavailable {
                provider: provider_name,
                detail: format!("the identity check did not finish: {e}"),
            }),
        }
    }

    /// What an authenticated node may do here: its allow-list entry. Read
    /// after the identity lookup, so an `untrust` during it is seen.
    pub(crate) async fn authorize(&self, node: NodeIdentity) -> Result<Admitted, Refusal> {
        let allow = self.allow_list.clone();
        match tokio::task::spawn_blocking(move || darkmux_fleet::authorize(&node, || allow())).await {
            Ok(result) => result,
            Err(e) => Err(Refusal::BadRequest(format!("the allow-list check did not finish: {e}"))),
        }
    }
}

/// A caller that passed [`Admission::authenticate`]: the node the overlay
/// network named. It carries no grant; a handler that needs one authorizes.
#[derive(Clone)]
pub(crate) struct Authenticated {
    pub node: NodeIdentity,
}

/// Which slot table a request counts against once its caller is admitted.
fn slots_for<'a>(state: &'a FleetListenerState, req: &Request) -> &'a Arc<KeySlots<String>> {
    if req.method() == axum::http::Method::GET && req.uri().path() == darkmux_fleet::CARD_PATH {
        &state.card_slots
    } else {
        &state.node_slots
    }
}

/// The gate on EVERY request: token, network identity, not this machine
/// ([`Admission::authenticate`]). The allow-list is not the gate's: a job
/// authorizes in its handler, a card read reports the grant. A request with no
/// peer address (no `ConnectInfo`) is refused: absence of evidence is not a
/// peer.
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
    let is_card = req.method() == axum::http::Method::GET && req.uri().path() == darkmux_fleet::CARD_PATH;
    let purpose = if is_card { Purpose::Card } else { Purpose::Work };
    match state.admission().authenticate(peer, req.headers(), purpose).await {
        Ok((node, fingerprint)) => {
            let Some(_node_slot) = slots_for(&state, &req).try_take(node.node_id.clone()) else {
                return refuse(&state, Some(peer), &Refusal::TooManyAtOnce { peer: node.name.clone() });
            };
            // (#2916 stage 2 review F1) Which token was in force, so a job
            // queued now can be checked against the token in force when its
            // seat frees. A fingerprint, never the token.
            req.extensions_mut().insert(fingerprint);
            req.extensions_mut().insert(Authenticated { node });
            next.run(req).await
        }
        Err(refusal) => refuse(&state, Some(peer), &refusal),
    }
}

/// How an admitted, in-scope job gets its seat.
enum Start {
    /// The seat was free: the job holds it now.
    Now(SeatGuard),
    /// The seat is busy and this machine queues (`fleet.busy_policy =
    /// queue`): the job waits for it until `deadline`, holding one of its
    /// sender's queue slots while it waits.
    Queued { what: String, queue_slot: KeySlot<String>, deadline: std::time::Instant },
}

/// One line of a reply body. Every body is newline-delimited JSON: one line
/// for every answer except a waited-on queued job, whose body carries a
/// `queued` line (again every heartbeat) before the final one.
fn reply_line(reply: &SubmissionReply) -> String {
    let mut s = serde_json::to_string(reply).unwrap_or_else(|_| "{\"status\":\"error\"}".into());
    s.push('\n');
    s
}

/// The final reply for a finished job, and its HTTP status (for a body that
/// is not streamed).
fn finished_reply(
    base: &SubmissionReply,
    result: Option<anyhow::Result<DispatchResult>>,
) -> (StatusCode, SubmissionReply) {
    match result {
        Some(Ok(r)) => (
            StatusCode::OK,
            SubmissionReply {
                exit_code: Some(r.exit_code),
                stdout: Some(r.stdout),
                stderr: Some(r.stderr),
                ..with_status(base, ReplyStatus::Completed)
            },
        ),
        Some(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            SubmissionReply { reason: Some(format!("{e:#}")), ..with_status(base, ReplyStatus::Error) },
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            SubmissionReply {
                reason: Some("the job's worker ended without a result".into()),
                ..with_status(base, ReplyStatus::Error)
            },
        ),
    }
}

/// `base` (machine, session, profile) with `status`.
fn with_status(base: &SubmissionReply, status: ReplyStatus) -> SubmissionReply {
    SubmissionReply { status, ..base.clone() }
}

/// The `queued` line a waiting sender reads.
fn queued_reply(base: &SubmissionReply, receiver: &str, what: &str) -> SubmissionReply {
    SubmissionReply {
        reason: Some(format!("{receiver} is busy ({what}); the job is queued and runs when its seat frees")),
        ..with_status(base, ReplyStatus::Queued)
    }
}

/// (#2916 stage 2 review C1) How long a job may wait for its seat, or
/// `None` when it cannot wait at all. A job the sender waits on must finish
/// inside its connection's lifetime, so its queue time is that lifetime
/// less the job's own timeout and [`QUEUE_SLACK`]; a job queued without
/// `--wait` may wait [`QueueLimits::no_wait_max_age`].
///
/// The job's `timeout_seconds` bounds a tool-less (hosted single-shot)
/// call; a container-agentic dispatch runs on its inactivity budget
/// instead and can outlast it. Such a run can outlive the connection: the
/// job keeps running here, and the sender reports that the answer was lost
/// and the job may still be running, naming the session to follow.
fn queue_deadline(limits: &QueueLimits, wait: bool, job_timeout_seconds: u32) -> Option<std::time::Instant> {
    let window = if wait {
        limits
            .connection_lifetime
            .saturating_sub(std::time::Duration::from_secs(u64::from(job_timeout_seconds)))
            .saturating_sub(QUEUE_SLACK)
    } else {
        limits.no_wait_max_age
    };
    (!window.is_zero()).then(|| std::time::Instant::now() + window)
}

/// What a busy seat means for a job under this machine's busy policy: how
/// long it may wait for the seat, or the refusal it gets instead. The one
/// reading of `fleet.busy_policy`, shared by a real start and a check.
fn wait_or_refuse(
    state: &FleetListenerState,
    sub: &WorkSubmission,
    occupied: &Occupied,
) -> Result<std::time::Instant, Refusal> {
    match state.busy_policy {
        BusyPolicy::Refuse => Err(Refusal::Busy { what: occupied.what.clone() }),
        BusyPolicy::Queue => queue_deadline(&state.queue_limits, sub.wait, sub.job.timeout_seconds).ok_or_else(|| {
            Refusal::Busy {
                what: format!(
                    "{}; a job with a {}s timeout has no time left to wait inside one connection",
                    occupied.what, sub.job.timeout_seconds
                ),
            }
        }),
    }
}

/// Decide how a scoped job starts: its seat now, a place in the queue, or a
/// busy refusal.
fn start_or_refuse(
    state: &FleetListenerState,
    admitted: &Admitted,
    scoped: &ScopedJob,
    session_id: &darkmux_types::session_id::SessionId,
    sub: &WorkSubmission,
) -> Result<Start, Refusal> {
    let occupied = match state.seats.try_claim(&scoped.seat, &session_id.wire()) {
        Ok(guard) => return Ok(Start::Now(guard)),
        Err(occupied) => occupied,
    };
    let deadline = wait_or_refuse(state, sub, &occupied)?;
    match state.queue_slots.try_take(admitted.peer_name.clone()) {
        Some(queue_slot) => Ok(Start::Queued { what: occupied.what, queue_slot, deadline }),
        None => Err(Refusal::QueueFull { peer: admitted.peer_name.clone(), what: occupied.what }),
    }
}

/// What [`start_or_refuse`] would decide for a scoped job right now, without
/// taking a seat or a queue slot: the seat it meets, or the refusal a run
/// would get.
fn seat_outlook(
    state: &FleetListenerState,
    admitted: &Admitted,
    scoped: &ScopedJob,
    sub: &WorkSubmission,
) -> Result<SeatOutlook, Refusal> {
    let Some(occupied) = state.seats.peek(&scoped.seat) else { return Ok(SeatOutlook::Free) };
    wait_or_refuse(state, sub, &occupied)?;
    if state.queue_slots.has_room(&admitted.peer_name) {
        Ok(SeatOutlook::WouldQueue)
    } else {
        Err(Refusal::QueueFull { peer: admitted.peer_name.clone(), what: occupied.what })
    }
}

/// The `checked` reply for a scoped job: the profile it would run on and
/// what it meets. Nothing ran, and no seat or queue slot was held.
fn checked_reply(
    state: &FleetListenerState,
    admitted: &Admitted,
    scoped: &ScopedJob,
    sub: &WorkSubmission,
) -> Result<SubmissionReply, Refusal> {
    let seat = seat_outlook(state, admitted, scoped, sub)?;
    let endpoint = match scoped.seat {
        WorkSeat::Local { .. } => EndpointClass::Managed,
        WorkSeat::Hosted { .. } => EndpointClass::Unmanaged,
    };
    let receiver = &state.receiver;
    Ok(SubmissionReply {
        machine: Some(receiver.clone()),
        profile: Some(scoped.profile.clone()),
        reason: Some(format!(
            "{receiver} would take this job on profile {}; {}",
            scoped.profile,
            match seat {
                SeatOutlook::Free => "its seat is free",
                SeatOutlook::WouldQueue => "its seat is busy, so the job would queue",
                SeatOutlook::Unknown => "its seat is unknown",
            }
        )),
        check: Some(CheckReport { endpoint, seat }),
        ..SubmissionReply::of(ReplyStatus::Checked)
    })
}

/// Everything a submitted job's worker thread needs.
struct Worker {
    state: FleetListenerState,
    admitted: Admitted,
    /// The fleet token the request was admitted with.
    token: TokenFingerprint,
    peer: std::net::IpAddr,
    job: WorkJob,
    scoped: ScopedJob,
    base: SubmissionReply,
    /// A waited-on queued job's reply stream. Closed = the sender hung up.
    /// A sender that closes its connection is noticed within moments (the
    /// next heartbeat write fails). One that vanishes without closing it
    /// (a machine that sleeps, a dropped network) is noticed only when TCP
    /// gives up on the connection, and its job may run if its seat frees
    /// first.
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// The result, for a job that is not streamed.
    result_tx: tokio::sync::oneshot::Sender<anyhow::Result<DispatchResult>>,
}

impl Worker {
    fn sender_gone(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.is_closed())
    }

    /// A refusal decided after the job was answered `queued`: to a waiting
    /// sender as the last line of its reply, and to the log either way.
    fn refuse_late(&self, r: &Refusal) {
        let receiver = &self.state.receiver;
        self.state.refusal_log.log(
            Some(self.peer),
            &format!("darkmux serve: fleet listener: refused queued {} — {}", self.job.session_id, r.reason(receiver)),
        );
        if let Some(p) = &self.progress {
            let reply = SubmissionReply { session_id: self.base.session_id.clone(), ..r.reply(receiver) };
            let _ = p.send(reply_line(&reply));
        }
    }

    /// (#2916 stage 2 review M1, F1) A queued job is checked again when its
    /// seat frees, against the state of THIS moment, and still runs on the
    /// seat it waited for, or not at all.
    ///
    /// Every gate the request passed is passed again, in the gate's own
    /// order, through the one `darkmux_fleet::admit`: the fleet token (the
    /// one `state.token` gives now, against the one the request was
    /// admitted with), the network identity of the same peer address (still
    /// a node, still this node, never this machine's own), then the
    /// allow-list (readable, read AFTER the lookup so an `untrust` during it
    /// is seen) and the entry for that node; then the config preflight and
    /// the scope. The per-node request cap is not a trust gate and is not
    /// re-applied. Returns the entry as it stands now and the profile.
    ///
    /// In production the daemon reads the fleet token once
    /// (`serve_token`'s Keychain tier and `serve.token_keychain` are cached
    /// for the process), so a rotated or removed token is seen only after a
    /// restart, which drops the queue anyway; the token check here holds
    /// for whatever `state.token` returns.
    fn recheck(&self) -> Result<(Admitted, String), Refusal> {
        let state = &self.state;
        let token = self.token.check_now((state.token)());
        let provider = &state.provider;
        // `admit` reads the allow-list only after the identity lookup, so an
        // `untrust` that lands while the provider answers is still seen.
        let admitted = darkmux_fleet::admit(
            token,
            || provider.identify(self.peer).map_err(|e| format!("{e:#}")),
            provider.provider_name(),
            self.peer,
            state.local_node_id.as_deref(),
            || (state.allow_list)(),
        )?;
        // The address now belongs to another node (one the allow-list also
        // trusts): not the machine that queued this job.
        if admitted.node_id != self.admitted.node_id {
            return Err(Refusal::NotOnOverlay {
                provider: provider.provider_name().to_string(),
                addr: self.peer.to_canonical().to_string(),
            });
        }
        (state.config_preflight)().map_err(|detail| Refusal::BadConfig { detail })?;
        let resolution = (state.resolve_profile)(&self.job.role_id, self.job.profile.as_deref());
        let now = darkmux_fleet::check_scope(&state.receiver, state.receiver_uid.as_deref(), &admitted, &self.job, resolution)?;
        if now.seat != self.scoped.seat {
            return Err(Refusal::SeatChanged { profile: now.profile });
        }
        Ok((admitted, now.profile))
    }

    /// Wait for the seat, then check again; the seat, the allow-list entry
    /// as it stands now and the profile to run on, or `None` when the job
    /// ends here (its end already reported).
    fn wait_for_seat(
        &self,
        queue_slot: KeySlot<String>,
        deadline: std::time::Instant,
    ) -> Option<(SeatGuard, Admitted, String)> {
        let state = &self.state;
        let receiver = &state.receiver;
        let sid = self.job.session_id.wire();
        let waited = state.seats.claim_waiting(&self.scoped.seat, &sid, state.queue_heartbeat, deadline, &|| self.sender_gone(), |o| {
            if let Some(p) = &self.progress {
                let _ = p.send(reply_line(&queued_reply(&self.base, receiver, &o.what)));
            }
        });
        // Waiting is over, however it ended: the sender's queue slot frees.
        drop(queue_slot);
        match waited {
            Waited::Cancelled => {
                eprintln!("darkmux serve: fleet listener: dropped queued {sid}: its sender stopped waiting");
                None
            }
            Waited::TimedOut(o) => {
                self.refuse_late(&Refusal::Busy { what: format!("{}; the job waited as long as it may", o.what) });
                None
            }
            Waited::Seat(guard) => match self.recheck() {
                Ok((admitted, profile)) => Some((guard, admitted, profile)),
                Err(r) => {
                    drop(guard);
                    self.refuse_late(&r);
                    None
                }
            },
        }
    }

    /// The worker thread's body.
    fn run(self, start: Start) {
        let (seat, admitted, profile) = match start {
            Start::Now(guard) => (guard, self.admitted.clone(), self.scoped.profile.clone()),
            Start::Queued { queue_slot, deadline, .. } => match self.wait_for_seat(queue_slot, deadline) {
                Some(got) => got,
                None => return,
            },
        };
        // A sender that hung up between its seat freeing and now: nothing ran.
        if self.sender_gone() {
            return;
        }
        // (#2916 stage 2 review C4) Attributed to the entry as it stands now,
        // so an entry renamed while the job waited is named correctly.
        let result = (self.state.execute)(self.job.clone(), profile, admitted.peer_name);
        drop(seat);
        match &self.progress {
            Some(p) => {
                let _ = p.send(reply_line(&finished_reply(&self.base, Some(result)).1));
            }
            None => {
                let _ = self.result_tx.send(result);
            }
        }
    }
}

/// `GET /fleet/card`: this machine's card and what it lets the caller do.
/// The gate has authenticated the caller (token, the node the socket's
/// address belongs to, not this machine); the allow-list is read here and its
/// answer is stated as the grant: the caller's own entry and no other, or "not
/// listed", or "could not say". A caller with no entry still gets the card:
/// visibility is not an execution grant. The card comes from the card cache;
/// a gather runs off the async threads.
async fn card_handler(State(state): State<FleetListenerState>, Extension(auth): Extension<Authenticated>) -> Response {
    let grant = crate::machine_card::CardGrant::from_authorization(&state.admission().authorize(auth.node).await);
    let cache = state.card_cache.clone();
    match tokio::task::spawn_blocking(move || cache.get(crate::machine_card::gather_local_card)).await {
        Ok(card) => Json(crate::machine_card::ListenerCard { card, grant }).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "darkmux serve: machine card gather panicked\n").into_response(),
    }
}

/// A caller the gate authenticated, and what the allow-list said of it: what
/// a job needs and a card read does not. The gate does not require the
/// allow-list entry. This extractor reads it and leaves the refusal to the
/// handler, which has the body: an unlisted sender's refusal names the trust
/// command for the job it posted.
struct Authorization {
    node: NodeIdentity,
    result: Result<Admitted, Refusal>,
}

#[axum::async_trait]
impl axum::extract::FromRequestParts<FleetListenerState> for Authorization {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &FleetListenerState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        let Some(auth) = parts.extensions.get::<Authenticated>().cloned() else {
            let detail = "the request did not pass the gate".to_string();
            let provider = state.provider.provider_name().to_string();
            return Err(refuse(state, peer, &Refusal::IdentityUnavailable { provider, detail }));
        };
        let result = state.admission().authorize(auth.node.clone()).await;
        Ok(Self { node: auth.node, result })
    }
}

impl Authorization {
    /// The admitted caller, or the refusal. An unlisted caller's refusal
    /// carries the `machine trust` command that admits it, resolved against
    /// this machine's roster and the job in `body`.
    fn admitted(self, body: &[u8]) -> Result<Admitted, Refusal> {
        self.result.map_err(|r| match r {
            Refusal::NotAllowed { .. } => {
                let roster = darkmux_fleet::load_roster().unwrap_or_default();
                r.with_trust_ask(darkmux_fleet::TrustAsk::for_sender(&self.node, &roster, body))
            }
            other => other,
        })
    }
}

/// Parse a request body and check the job against the admitted peer's scope:
/// the version, the shape, the profile as this machine resolves it, the
/// allow-list scope and the boundary. The one path a job (or a check) takes
/// to a [`ScopedJob`].
async fn scope_submission(
    state: &FleetListenerState,
    admitted: &Admitted,
    body: &[u8],
) -> Result<(WorkSubmission, ScopedJob), Refusal> {
    let sub = WorkSubmission::parse(body)?;
    let resolve = state.resolve_profile.clone();
    let (role, requested) = (sub.job.role_id.clone(), sub.job.profile.clone());
    let resolution = match tokio::task::spawn_blocking(move || resolve(&role, requested.as_deref())).await {
        Ok(r) => r,
        Err(e) => ProfileResolution::Unresolved(format!("profile resolution did not finish: {e}")),
    };
    let scoped = darkmux_fleet::check_scope(&state.receiver, state.receiver_uid.as_deref(), admitted, &sub.job, resolution)?;
    Ok((sub, scoped))
}

/// `POST` a job or a check: refuses a caller the allow-list does not list,
/// with the remedy for the job it posted, else hands the admitted job on.
async fn submit_route(
    State(state): State<FleetListenerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    authorization: Authorization,
    Extension(token): Extension<TokenFingerprint>,
    body: Bytes,
) -> Response {
    match authorization.admitted(&body) {
        Ok(admitted) => submit_handler(state, peer_addr, admitted, token, body).await,
        Err(refusal) => refuse(&state, Some(peer_addr.ip()), &refusal),
    }
}

async fn submit_handler(
    state: FleetListenerState,
    peer_addr: SocketAddr,
    admitted: Admitted,
    token: TokenFingerprint,
    body: Bytes,
) -> Response {
    // (#2947 review M1) A job this machine would refuse at its dispatch
    // preflight (a bad enum config value) is refused HERE, synchronously,
    // before a seat is taken or the job is accepted: otherwise the sender
    // reads "accepted" and the failure arrives later, after the runner has
    // already reconciled residency.
    if let Err(detail) = (state.config_preflight)() {
        return refuse(&state, Some(peer_addr.ip()), &Refusal::BadConfig { detail });
    }
    let receiver = state.receiver.clone();
    let (mut sub, scoped) = match scope_submission(&state, &admitted, &body).await {
        Ok(got) => got,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };

    // A check answers here: every gate above has run, and nothing below (a
    // relay session, a seat, a worker) is started for it.
    if sub.job.mode == SubmissionMode::Check {
        return match checked_reply(&state, &admitted, &scoped, &sub) {
            Ok(reply) => (StatusCode::OK, Json(reply)).into_response(),
            Err(r) => refuse(&state, Some(peer_addr.ip()), &r),
        };
    }

    // (#2916 review C2) The receiver's own session for this job, never the
    // sender's verbatim: a relay of it, in a standalone run, so a peer can
    // neither reuse one of this machine's sessions nor name one of its
    // missions.
    sub.job.session_id = darkmux_types::session_id::SessionId::relay(sub.job.session_id.clone(), &admitted.peer_name);
    let session_id = sub.job.session_id.clone();

    // (#2916 stage 2) Busy is per seat: one job per local model, hosted jobs
    // up to `remote.concurrent_cap`. Past that, `fleet.busy_policy`.
    let start = match start_or_refuse(&state, &admitted, &scoped, &session_id, &sub) {
        Ok(s) => s,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };
    let queued_what = match &start {
        Start::Queued { what, .. } => Some(what.clone()),
        Start::Now(_) => None,
    };
    eprintln!(
        "darkmux serve: fleet listener: {} {session_id} from {} (role {}, profile {})",
        if queued_what.is_some() { "queued" } else { "accepted" },
        admitted.peer_name,
        sub.job.role_id,
        scoped.profile
    );

    let base = SubmissionReply {
        machine: Some(receiver.clone()),
        session_id: Some(session_id.clone()),
        profile: Some(scoped.profile.clone()),
        ..SubmissionReply::of(ReplyStatus::Accepted)
    };
    // A waited-on queued job streams its `queued` lines as they happen.
    let (progress, progress_rx) = if queued_what.is_some() && sub.wait {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let worker = Worker {
        state: state.clone(),
        admitted,
        token,
        peer: peer_addr.ip(),
        job: sub.job,
        scoped,
        base: base.clone(),
        progress,
        result_tx,
    };
    // A dedicated OS thread, not the async runtime: a dispatch blocks for
    // minutes, and a queued job blocks until its seat frees. The seat guard
    // lives in the thread, so the seat frees when the work ends, even if
    // the sender stopped waiting.
    let spawned = std::thread::Builder::new().name("darkmux-fleet-job".into()).spawn(move || worker.run(start));
    if let Err(e) = spawned {
        return refuse(&state, Some(peer_addr.ip()), &Refusal::BadRequest(format!("could not start the job: {e}")));
    }

    if let Some(what) = queued_what {
        if let Some(rx) = progress_rx {
            use futures::StreamExt;
            let lines = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
            return (StatusCode::OK, axum::body::Body::from_stream(lines)).into_response();
        }
        return (StatusCode::ACCEPTED, Json(queued_reply(&base, &receiver, &what))).into_response();
    }
    if !sub.wait {
        return (StatusCode::ACCEPTED, Json(base)).into_response();
    }
    let (code, reply) = finished_reply(&base, result_rx.await.ok());
    (code, Json(reply)).into_response()
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
    if ip.is_unspecified() || (ip.to_canonical().is_loopback() && !LOOPBACK_FOR_E2E) || ip.is_multicast() {
        return Err(format!("refusing to bind the fleet listener to {ip}: not a specific overlay address"));
    }
    Ok(SocketAddr::new(ip, port))
}

/// (#2916 stage 2) True only in a binary built with the
/// `e2e-fleet-loopback` cargo feature, which exists for ONE test target
/// (`tests/fleet_profile_address_two_daemons.rs`: two daemons on one machine, each
/// with a fake identity tool that places the other at 127.0.0.1). It lets
/// the listener bind the loopback address its fake provider reports. It is
/// a compile-time switch, not a runtime knob: no config value or env var
/// reaches it, and a release build (`cargo build --release`, brew) never
/// enables the feature, so production still refuses loopback.
pub(crate) const LOOPBACK_FOR_E2E: bool = cfg!(feature = "e2e-fleet-loopback");

// (#2916 stage 2 review C7) The loopback seam must never reach an optimized
// build: a release binary (brew, `cargo build --release`) with it would let
// a real listener answer on loopback, where every caller looks like this
// machine. The two-daemon test builds a debug binary.
#[cfg(all(feature = "e2e-fleet-loopback", not(debug_assertions)))]
compile_error!("the `e2e-fleet-loopback` feature is for the debug-build two-daemon test only; never build it with optimizations");

/// What the listener is doing, for `/health` and so for `darkmux doctor`
/// (#2916 review C8): a daemon started by launchd can fail where a shell
/// succeeds, and the reason used to live only in the daemon's log. Coarse
/// phrases only: no provider output, no ids.
pub(crate) static LISTENER_STATE: std::sync::Mutex<Option<(&'static str, String)>> = std::sync::Mutex::new(None);

/// (#2916 stage 2 review C5) The busy policy and hosted-job bound the
/// running listener was started with (it reads config once), so `darkmux
/// doctor` can report what is in force rather than what the file says now.
pub(crate) static LISTENER_BUSY: std::sync::Mutex<Option<(BusyPolicy, u32)>> = std::sync::Mutex::new(None);

/// The running listener's seat book, for the machine card. `None` until the
/// listener has started (this machine takes no fleet work).
pub(crate) static LISTENER_SEATS: std::sync::Mutex<Option<Arc<SeatBook>>> = std::sync::Mutex::new(None);

/// The running listener's busy policy and seats, for the machine card;
/// `None` when the listener has not started.
pub(crate) fn listener_seats() -> Option<(BusyPolicy, darkmux_fleet::SeatSnapshot)> {
    let (policy, _) = (*LISTENER_BUSY.lock().ok()?)?;
    let book = LISTENER_SEATS.lock().ok()?.clone()?;
    Some((policy, book.snapshot()))
}

/// [`LISTENER_BUSY`] for `/health`, for this machine only (`local` is
/// `is_local_request`'s answer); `None` when the listener has not started.
pub(crate) fn listener_busy(local: bool) -> Option<crate::wire::FleetBusy> {
    if !local {
        return None;
    }
    let (policy, hosted_cap) = (*LISTENER_BUSY.lock().ok()?)?;
    Some(crate::wire::FleetBusy { policy, hosted_cap })
}

/// `coarse` is one of `off` / `starting` / `waiting` / `listening` / `not started`;
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
        per_ip: PER_IP_CONNECTIONS,
        header_read_timeout: std::time::Duration::from_secs(3),
        conn_deadline: std::time::Duration::from_secs(60 * 60 + 300),
    };
}

/// Requests one admitted node may have in flight at once (#2916 round 3 C5),
/// and (#2916 stage 2) jobs it may have queued at once.
pub(crate) const NODE_CAP: usize = 4;

/// Connections one peer address may hold at once: every job it may have in
/// flight AND every card read it may make, so a peer whose jobs hold
/// [`NODE_CAP`] connections for the whole job (a waited job holds its
/// connection until it finishes) can still read this machine's card. The cap
/// is enforced at accept, before the router knows the path, so it is the sum
/// of the two per-node caps, not either one.
pub(crate) const PER_IP_CONNECTIONS: usize = NODE_CAP + CARD_NODE_CAP;

/// Card reads one admitted node may have in flight at once. A card is one
/// gather (a few subprocess reads), so a couple at a time is generous for
/// one poller.
pub(crate) const CARD_NODE_CAP: usize = 2;

/// (#2916 stage 2) How often a sender waiting on a queued job hears that it
/// is still queued: well inside the shortest read deadline a sender uses
/// (60 s), so the connection never looks dead while the job waits.
pub(crate) const QUEUE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(20);

/// (#2916 stage 2 review C1) Time a waited-on queued job leaves between its
/// own timeout and the end of its connection: its answer must still reach a
/// sender that is listening.
pub(crate) const QUEUE_SLACK: std::time::Duration = std::time::Duration::from_secs(60);

/// (#2916 stage 2 review C1) How long queued jobs may wait for a seat.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueLimits {
    /// How long one connection may live ([`ConnLimits::conn_deadline`]): a
    /// waited-on job waits and runs inside it.
    pub connection_lifetime: std::time::Duration,
    /// How long a job queued without `--wait` may wait before it is
    /// dropped as stale: 30 minutes. Nobody is listening for it, and work
    /// sent half an hour ago is rarely still wanted.
    pub no_wait_max_age: std::time::Duration,
}

impl QueueLimits {
    pub(crate) const PRODUCTION: QueueLimits = QueueLimits {
        connection_lifetime: ConnLimits::PRODUCTION.conn_deadline,
        no_wait_max_age: std::time::Duration::from_secs(30 * 60),
    };
}

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

    /// Whether `key` could take a slot now (without taking one).
    pub(crate) fn has_room(&self, key: &K) -> bool {
        let c = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.get(key).copied().unwrap_or(0) < self.max
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
                    eprintln!("darkmux serve: fleet listener: accept failed ({e}); pausing 100ms");
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
        set_state("off", "off (fleet.listener.enabled is false)");
        return;
    }
    set_state("starting", "starting");
    tokio::spawn(async move {
        if let Err(e) = run(shutdown).await {
            set_state("not started", format!("not started: {e}"));
            eprintln!("{}", darkmux_types::style::warn(&format!("darkmux serve: fleet listener: listener not started: {e}")));
        }
    });
}

async fn run(mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<(), String> {
    if !darkmux_flow::serve_token_present() {
        return Err("no fleet token (the serve token: Keychain item `darkmux-serve-token`, read only \
                    when `serve.token_keychain` is on, or DARKMUX_SERVE_TOKEN); a listener that cannot \
                    check a token takes no work"
            .into());
    }
    let provider: Arc<dyn IdentityProvider> =
        Arc::from(darkmux_fleet::configured_provider().map_err(|e| format!("{e:#}"))?);
    let receiver = darkmux_flow::resolve_machine_id()
        .ok_or_else(|| "this machine has no machine_id, so it cannot tell a misaddressed job".to_string())?;
    // (#2916 stage 2) Read once at start, like the rest of the listener's
    // config. `configured_provider` above already ran the fleet-submission
    // preflight, which refuses a bad value; this is the typed read.
    let busy_policy = darkmux_types::config_access::fleet_busy_policy().map_err(|e| e.to_string())?;
    let port = darkmux_types::config_access::fleet_listener_port();
    let (addr, local_id) = loop {
        let p = provider.clone();
        match tokio::task::spawn_blocking(move || p.local_node()).await {
            Ok(Ok(local)) => break (listen_addr(&local, port)?, local.node_id.clone()),
            Ok(Err(e)) => {
                set_state("waiting", format!("waiting for the {} network to answer (retrying every 30s)", provider.provider_name()));
                eprintln!(
                    "darkmux serve: fleet listener: the {} network is not answering ({e:#}); retrying in 30s",
                    provider.provider_name()
                )
            }
            Err(e) => eprintln!("darkmux serve: fleet listener: identity check failed ({e}); retrying in 30s"),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            _ = async { let _ = shutdown.wait_for(|v| *v).await; } => return Ok(()),
        }
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("binding {addr}: {e} (another process on `fleet.listener.port`?)"))?;
    if LOOPBACK_FOR_E2E {
        eprintln!(
            "{}",
            darkmux_types::style::warn(
                "darkmux serve: fleet listener: this binary was built with the test-only \
                 `e2e-fleet-loopback` feature and may bind loopback; never run it as a real fleet member"
            )
        );
    }
    set_state("listening", format!("listening on {addr}"));
    println!("  fleet listener: {addr} (work submission; identity: {})", provider.provider_name());
    let state = FleetListenerState::production(receiver, provider, Some(local_id), busy_policy);
    if let Ok(mut g) = LISTENER_BUSY.lock() {
        *g = Some((busy_policy, darkmux_types::config_access::remote_concurrent_cap()));
    }
    if let Ok(mut g) = LISTENER_SEATS.lock() {
        *g = Some(state.seats.clone());
    }
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

    /// The harness's card cache TTL: long, so two reads in one test are one gather.
    const CARD_TTL: Duration = Duration::from_secs(600);

    fn allow() -> AllowList {
        let mut m = AllowList::new();
        m.insert(
            "macbook-pro".into(),
            AcceptWorkEntry {
                node_id: Some("nLAPTOP".into()),
                profiles: Some(vec!["host".into(), "small".into(), "cloud".into()]),
                roles: Some(vec!["radio-host".into()]),
                images: None,
                workspace: Some(false),
                extras: Default::default(),
            },
        );
        m
    }

    /// The fixed allow-list above already lets the laptop run these.
    fn allow_profiles(_h: &Harness, profiles: &[&str]) {
        let listed = allow()["macbook-pro"].profiles.clone().unwrap();
        assert!(profiles.iter().all(|p| listed.iter().any(|l| l == p)), "{profiles:?} not in {listed:?}");
    }

    /// A real listener on 127.0.0.1 whose fake provider says the loopback
    /// peer is `peer` (or nobody, or is down). Returns the URL and a counter
    /// of executed jobs.
    struct Harness {
        refusal_log: Arc<RefusalLog>,
        url: String,
        ran: Arc<Mutex<Vec<(String, String)>>>,
        seats: Arc<SeatBook>,
        /// The allow-list the listener reads per request; a test may revoke.
        allow: Arc<Mutex<AllowList>>,
        queue_slots: Arc<KeySlots<String>>,
        /// When set, the default profile resolves to another local model.
        model_moved: Arc<std::sync::atomic::AtomicBool>,
        /// When set, the default profile resolves to a hosted endpoint.
        went_hosted: Arc<std::sync::atomic::AtomicBool>,
        /// The fleet token the listener expects; a test may rotate it.
        token: Arc<Mutex<Option<String>>>,
        /// What the identity provider answers; a test may change it.
        network: Arc<Switchable>,
        /// The allow-list name each job ran under, in order.
        origins: Arc<Mutex<Vec<String>>>,
    }

    impl Harness {
        fn origins(&self) -> Vec<String> {
            self.origins.lock().unwrap().clone()
        }
    }

    /// An identity provider a test can change mid-run: a node leaving the
    /// network, the provider going down, another node taking the address.
    struct Switchable(Mutex<StaticIdentityProvider>, Mutex<Duration>, std::sync::atomic::AtomicUsize);

    impl Switchable {
        /// Make every later `identify` take `d` (a slow provider tool).
        fn slow(&self, d: Duration) {
            *self.1.lock().unwrap() = d;
        }

        fn set(&self, peers: Vec<darkmux_fleet::NodeIdentity>, down: Option<&str>) {
            let mut g = self.0.lock().unwrap();
            g.peers = peers;
            g.down = down.map(str::to_string);
        }
    }

    impl IdentityProvider for Switchable {
        fn provider_name(&self) -> &str {
            "static"
        }
        fn identify(&self, peer: std::net::IpAddr) -> anyhow::Result<Option<darkmux_fleet::NodeIdentity>> {
            self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let delay = *self.1.lock().unwrap();
            std::thread::sleep(delay);
            self.0.lock().unwrap().identify(peer)
        }
        fn local_node(&self) -> anyhow::Result<darkmux_fleet::NodeIdentity> {
            self.0.lock().unwrap().local_node()
        }
        fn nodes(&self) -> anyhow::Result<Vec<darkmux_fleet::NodeIdentity>> {
            self.0.lock().unwrap().nodes()
        }
    }

    /// Wide queue limits: nothing in a test waits long enough to hit them.
    const WIDE: QueueLimits = QueueLimits {
        connection_lifetime: Duration::from_secs(3600),
        no_wait_max_age: Duration::from_secs(3600),
    };

    fn start(peer: Option<darkmux_fleet::NodeIdentity>, down: bool, job_ms: u64) -> Harness {
        start_with_preflight(peer, down, job_ms, Arc::new(|| Ok(())))
    }

    fn start_with_preflight(
        peer: Option<darkmux_fleet::NodeIdentity>,
        down: bool,
        job_ms: u64,
        config_preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Harness {
        start_full(peer, down, job_ms, config_preflight, BusyPolicy::Refuse, 1, WIDE)
    }

    /// The test resolver: `utility` is utility-only, `cloud` is a hosted
    /// endpoint, `small` a second local model, anything else the local `big`.
    fn test_resolution(requested: Option<&str>) -> ProfileResolution {
        use darkmux_fleet::WorkSeat;
        match requested {
            Some("utility") => ProfileResolution::UtilityOnly("utility".into()),
            Some("cloud") => ProfileResolution::Work { profile: "cloud".into(), seat: WorkSeat::Hosted { model: "gpt-x".into() } },
            Some("small") => ProfileResolution::Work { profile: "small".into(), seat: WorkSeat::Local { model: "small".into() } },
            Some(p) => ProfileResolution::Work { profile: p.to_string(), seat: WorkSeat::Local { model: "big".into() } },
            None => ProfileResolution::Work { profile: "host".into(), seat: WorkSeat::Local { model: "big".into() } },
        }
    }

    fn start_full(
        peer: Option<darkmux_fleet::NodeIdentity>,
        down: bool,
        job_ms: u64,
        config_preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        busy_policy: BusyPolicy,
        remote_cap: u32,
        queue_limits: QueueLimits,
    ) -> Harness {
        start_full_as(peer, down, job_ms, config_preflight, busy_policy, remote_cap, queue_limits, None)
    }

    /// [`start_full`] for a receiver that knows its own hardware uid.
    #[allow(clippy::too_many_arguments)]
    fn start_full_as(
        peer: Option<darkmux_fleet::NodeIdentity>,
        down: bool,
        job_ms: u64,
        config_preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        busy_policy: BusyPolicy,
        remote_cap: u32,
        queue_limits: QueueLimits,
        receiver_uid: Option<&str>,
    ) -> Harness {
        let allow_now = Arc::new(Mutex::new(allow()));
        let allow_read = allow_now.clone();
        let queue_slots = Arc::new(KeySlots::new(NODE_CAP));
        let model_moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let moved = model_moved.clone();
        let went_hosted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hosted_now = went_hosted.clone();
        let local = test_node("nSTUDIO", "studio", "100.64.0.2");
        let network = Arc::new(Switchable(
            Mutex::new(StaticIdentityProvider {
                local,
                peers: peer.into_iter().collect(),
                down: down.then(|| "daemon not running".to_string()),
            }),
            Mutex::new(Duration::ZERO),
            Default::default(),
        ));
        let token_now = Arc::new(Mutex::new(Some(TOKEN.to_string())));
        let token_read = token_now.clone();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let ran_c = ran.clone();
        let origins = Arc::new(Mutex::new(Vec::new()));
        let origins_c = origins.clone();
        let seats = Arc::new(SeatBook::new(remote_cap));
        let refusal_log = Arc::new(RefusalLog::new());
        let state = FleetListenerState {
            receiver: "studio".into(),
            receiver_uid: receiver_uid.map(str::to_string),
            provider: network.clone(),
            local_node_id: Some("nSTUDIO".into()),
            token: Arc::new(move || token_read.lock().unwrap().clone()),
            allow_list: Arc::new(move || Ok(allow_read.lock().unwrap().clone())),
            resolve_profile: Arc::new(move |_role, requested| {
                if requested.is_none() && hosted_now.load(std::sync::atomic::Ordering::SeqCst) {
                    return ProfileResolution::Work {
                        profile: "host".into(),
                        seat: darkmux_fleet::WorkSeat::Hosted { model: "gpt-x".into() },
                    };
                }
                if requested.is_none() && moved.load(std::sync::atomic::Ordering::SeqCst) {
                    return ProfileResolution::Work {
                        profile: "host".into(),
                        seat: darkmux_fleet::WorkSeat::Local { model: "moved".into() },
                    };
                }
                test_resolution(requested)
            }),
            execute: Arc::new(move |job: WorkJob, profile: String, origin: String| {
                origins_c.lock().unwrap().push(origin);
                std::thread::sleep(Duration::from_millis(job_ms));
                ran_c.lock().unwrap().push((job.session_id.wire(), profile.clone()));
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: format!("ran {} on {profile}", job.role_id),
                    stderr: String::new(),
                    session_id: job.session_id,
                    execution: None,
                    out_dir: None,
                    trajectory: None,
                })
            }),
            seats: seats.clone(),
            busy_policy,
            queue_slots: queue_slots.clone(),
            queue_heartbeat: Duration::from_millis(100),
            queue_limits,
            refusal_log: refusal_log.clone(),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            card_cache: Arc::new(crate::machine_card::CardCache::new(CARD_TTL)),
            identity_cache: Arc::new(IdentityCache::new()),
            config_preflight,
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
        Harness { refusal_log, url: format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH), ran, seats, allow: allow_now, queue_slots, model_moved, went_hosted, token: token_now, network, origins }
    }

    fn laptop() -> darkmux_fleet::NodeIdentity {
        test_node("nLAPTOP", "macbook-pro", "127.0.0.1")
    }

    /// The sender's session `nonce`: an ad-hoc `radio-host` dispatch in its
    /// mission `m-1`.
    fn sender(nonce: &str) -> darkmux_types::session_id::SessionId {
        darkmux_types::session_id::SessionId::adhoc(
            darkmux_types::session_id::RunId::mission("m-1").unwrap(),
            "radio-host",
            nonce,
        )
    }

    /// The receiver's relay session for the sender's `nonce` from the
    /// laptop, as a wire string.
    fn relay(nonce: &str) -> String {
        darkmux_types::session_id::SessionId::relay(sender(nonce), "macbook-pro").wire()
    }

    fn job(session: &str, profile: Option<&str>) -> WorkJob {
        darkmux_fleet::build_work_job(
            "studio".into(),
            "radio-host".into(),
            "hello".into(),
            sender(session),
            profile.map(str::to_string),
            None,
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
        assert_eq!(reply.status, ReplyStatus::Completed);
        assert_eq!(reply.exit_code, Some(0));
        assert_eq!(reply.stdout.as_deref(), Some("ran radio-host on host"));
        assert_eq!(reply.profile.as_deref(), Some("host"));
        assert_eq!(reply.session_id.map(|s| s.wire()), Some(relay("s-ok")), "the receiver's own session id");
        assert_eq!(h.ran.lock().unwrap().as_slice(), &[(relay("s-ok"), "host".to_string())]);
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
            assert_eq!(reply.status, ReplyStatus::Refused);
            assert!(reply.reason.as_deref().unwrap_or("").contains(want_reason), "{reply:?}");
        }
        assert!(h.ran.lock().unwrap().is_empty(), "a refused request must never run");
    }

    #[test]
    fn a_node_not_on_the_allow_list_is_refused_by_name() {
        let h = start(Some(test_node("nPHONE", "phone", "127.0.0.1")), false, 0);
        let (code, reply) = post(&h, TOKEN, job("s", None), true);
        assert_eq!(code, 403);
        let said = reply.reason.unwrap();
        assert!(said.starts_with("studio does not accept work from phone"), "{said}");
        // The remedy carries the role the job asked for, read from the body
        // the extractor could not see.
        assert!(said.contains("--roles radio-host"), "{said}");
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

    /// GET the card at `url` with `token`; the status and the body (a
    /// refusal's reply, or the card).
    fn get_card(url: &str, token: &str) -> (u16, serde_json::Value) {
        let resp = ureq::get(url).set("Authorization", &format!("Bearer {token}")).call();
        let (code, resp) = match resp {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => (code, r),
            Err(e) => panic!("no answer: {e}"),
        };
        (code, serde_json::from_reader(resp.into_reader()).unwrap_or(serde_json::Value::Null))
    }

    fn card_url(h: &Harness) -> String {
        h.url.replace(darkmux_fleet::SUBMISSION_PATH, darkmux_fleet::CARD_PATH)
    }

    /// The promise: the listener's card read states the verified caller's own
    /// grant and no other sender's.
    #[test]
    fn a_card_read_states_the_verified_callers_entry_and_no_ones_elses() {
        let h = start(Some(laptop()), false, 0);
        h.allow.lock().unwrap().insert(
            "mini-1".into(),
            AcceptWorkEntry {
                node_id: Some("nMINI".into()),
                profiles: Some(vec!["secret-profile".into()]),
                roles: Some(vec!["radio-host".into()]),
                images: None,
                workspace: Some(true),
                extras: Default::default(),
            },
        );
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["grant"]["state"], "listed");
        assert_eq!(body["grant"]["accepts"]["peer_name"], "macbook-pro");
        assert_eq!(body["grant"]["accepts"]["profiles"], serde_json::json!(["host", "small", "cloud"]));
        assert_eq!(body["grant"]["accepts"]["roles"], serde_json::json!(["radio-host"]));
        assert_eq!(body["grant"]["accepts"]["workspace"], false);
        assert_eq!(body["card"]["card_schema_version"], crate::machine_card::CARD_SCHEMA_VERSION);
        assert!(body["card"].get("grant").is_none() && body["card"].get("accepts").is_none(), "the card itself states no grant: {body}");
        let text = body.to_string();
        assert!(!text.contains("mini-1") && !text.contains("secret-profile"), "another sender's grant leaked: {text}");
    }

    /// The promise of symmetric visibility: a card read is AUTHENTICATED, not
    /// authorized. A node the network names, holding the token, gets this
    /// machine's card whether or not the allow-list has an entry for it; the
    /// grant says which. A job from the same node is still refused by name.
    #[test]
    fn a_verified_node_with_no_allow_list_entry_still_gets_the_card_and_the_grant_says_not_listed() {
        let h = start(Some(test_node("nPHONE", "phone", "127.0.0.1")), false, 0);
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 200, "a machine that grants nothing is still visible: {body}");
        assert_eq!(body["grant"], serde_json::json!({"state": "not_listed"}));
        assert_eq!(body["card"]["card_schema_version"], crate::machine_card::CARD_SCHEMA_VERSION);
        assert_eq!(h.refusal_log.written.load(std::sync::atomic::Ordering::SeqCst), 0, "a read is not a refusal");
        let (code, reply) = post(&h, TOKEN, job("s-phone", None), true);
        assert_eq!(code, 403, "{reply:?}");
        assert!(reply.reason.unwrap().starts_with("studio does not accept work from phone"));
    }

    /// An allow-list this machine cannot read, or one that names the caller
    /// twice, is "unknown", never "not listed": the card is still served.
    #[test]
    fn an_allow_list_that_cannot_say_states_an_unknown_grant_and_still_serves_the_card() {
        let h = start(Some(laptop()), false, 0);
        h.allow.lock().unwrap().insert(
            "macbook-pro-again".into(),
            AcceptWorkEntry {
                node_id: Some("nLAPTOP".into()),
                profiles: Some(vec!["host".into()]),
                roles: None,
                images: None,
                workspace: None,
                extras: Default::default(),
            },
        );
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["grant"], serde_json::json!({"state": "unknown"}));
        assert!(body["card"]["profiles"].is_array(), "the card is intact: {body}");
    }

    /// The gate on a card read is authentication: a wrong token, a node the
    /// network does not place, and a provider that cannot answer are refused
    /// with the listener's own sentence, so the asker can tell a refused
    /// machine from one with the listener off.
    #[test]
    fn a_card_read_is_refused_at_the_gate_when_the_caller_cannot_be_authenticated() {
        let h = start(Some(laptop()), false, 0);
        let (code, body) = get_card(&card_url(&h), "wrong-token");
        assert_eq!(code, 401, "{body}");
        assert!(body["reason"].as_str().unwrap().contains("fleet token is missing or does not match"), "{body}");
        let h = start(None, false, 0);
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 403, "{body}");
        assert!(body["reason"].as_str().unwrap().contains("did not come from a node"), "{body}");
        assert!(body.get("card").is_none() && body.get("grant").is_none());
        let h = start(Some(laptop()), true, 0);
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 503, "{body}");
        assert!(body["reason"].as_str().unwrap().contains("cannot tell which machine sent this request"), "{body}");
    }

    /// A poller that cannot be placed asks every few seconds; its refusals
    /// share the per-address log budget, so the log stays bounded.
    #[test]
    fn refused_card_reads_share_the_per_address_log_budget() {
        let h = start(None, false, 0);
        for _ in 0..30 {
            assert_eq!(get_card(&card_url(&h), TOKEN).0, 403);
        }
        assert_eq!(h.refusal_log.written.load(std::sync::atomic::Ordering::SeqCst), u64::from(RefusalLog::PER_WINDOW));
    }

    /// The promise: a burst of card reads costs one gather. The second read
    /// is the first's card (same `generated_at_ms`), and the card records the
    /// TTL it is served under.
    #[test]
    fn card_reads_are_served_from_the_card_cache_and_the_card_says_so() {
        let h = start(Some(laptop()), false, 0);
        let (_, first) = get_card(&card_url(&h), TOKEN);
        std::thread::sleep(Duration::from_millis(30));
        let (_, second) = get_card(&card_url(&h), TOKEN);
        assert_eq!(first["card"]["generated_at_ms"], second["card"]["generated_at_ms"], "the second read gathered again");
        assert_eq!(second["card"]["cache_ttl_ms"], CARD_TTL.as_millis() as u64);
    }

    /// The promise: a card read may reuse a recent identity lookup for its
    /// address, and a job never does. Two card reads run the provider once;
    /// two jobs run it twice.
    #[test]
    fn card_reads_reuse_an_identity_lookup_and_jobs_never_do() {
        let lookups = |h: &Harness| h.network.2.load(std::sync::atomic::Ordering::SeqCst);
        let h = start(Some(laptop()), false, 0);
        assert_eq!(get_card(&card_url(&h), TOKEN).0, 200);
        assert_eq!(get_card(&card_url(&h), TOKEN).0, 200);
        assert_eq!(lookups(&h), 1, "the second card read reused the first's lookup");
        let h = start(Some(laptop()), false, 0);
        assert_eq!(post(&h, TOKEN, job("s-a", None), true).0, 200);
        assert_eq!(post(&h, TOKEN, job("s-b", None), true).0, 200);
        assert_eq!(lookups(&h), 2, "every job is placed by the provider, so untrust and a node leaving take effect at once");
    }

    /// (#3004 A2) Connections are capped per address at accept, before the
    /// router knows the path. A peer whose waited jobs hold every connection
    /// it may use for jobs still gets its card: the cap leaves room for reads.
    #[test]
    fn a_peer_whose_jobs_hold_every_job_connection_can_still_read_the_card() {
        use std::io::Write;
        const { assert!(PER_IP_CONNECTIONS >= NODE_CAP + CARD_NODE_CAP) };
        let h = start(Some(laptop()), false, 0);
        let addr = h.url.trim_start_matches("http://").split('/').next().unwrap().to_string();
        let mut held = Vec::new();
        for _ in 0..NODE_CAP {
            let mut s = std::net::TcpStream::connect(&addr).unwrap();
            s.write_all(b"POST /fleet/work HTTP/1.1\r\nHost: x\r\n").unwrap();
            held.push(s);
        }
        std::thread::sleep(Duration::from_millis(150));
        let (code, body) = get_card(&card_url(&h), TOKEN);
        assert_eq!(code, 200, "{NODE_CAP} connections held for jobs starved the card read: {body}");
    }

    /// (#2947 review M1) A receiver whose own config would refuse the
    /// dispatch refuses the SUBMISSION synchronously: 503 with the preflight
    /// text, the busy slot never taken, the job never executed.
    #[test]
    fn a_bad_config_receiver_refuses_the_submission_synchronously() {
        let h = start_with_preflight(
            Some(laptop()),
            false,
            0,
            Arc::new(|| Err("dispatch: refusing to start: bad config (#2947) `seroius`".to_string())),
        );
        let (code, reply) = post(&h, TOKEN, job("s-bad", None), false);
        assert_eq!(code, 503, "{reply:?}");
        assert_eq!(reply.status, ReplyStatus::Refused);
        let reason = reply.reason.unwrap_or_default();
        assert!(reason.contains("`seroius`") && reason.contains("darkmux doctor"), "{reason}");
        // (#2947 review C-e) The daemon reads config once at start.
        assert!(reason.contains("restart `darkmux serve`"), "{reason}");
        assert!(h.seats.running().is_empty(), "a seat was taken");
        assert!(h.ran.lock().unwrap().is_empty(), "the job ran");
    }

    /// (#2947) The production state's preflight is the real dispatch-scope
    /// preflight, not a stub: a bad env value makes it refuse.
    #[serial_test::serial]
    #[test]
    fn the_production_submission_preflight_is_the_dispatch_scope_preflight() {
        let prev = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "seroius") };
        let state = FleetListenerState::production(
            "studio".into(),
            Arc::new(StaticIdentityProvider { local: test_node("nS", "studio", "100.64.0.2"), peers: vec![], down: None }),
            None,
            BusyPolicy::Refuse,
        );
        let r = (state.config_preflight)();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
        }
        let err = r.unwrap_err();
        assert!(err.contains("dispatch: refusing to start") && err.contains("`seroius`"), "{err}");
    }

    #[test]
    fn a_busy_local_model_says_so_at_once_and_frees_the_seat_when_done() {
        let h = start(Some(laptop()), false, 600);
        let (code, reply) = post(&h, TOKEN, job("s-long", None), false);
        assert_eq!(code, 202, "{reply:?}");
        assert_eq!(reply.status, ReplyStatus::Accepted);
        let started = std::time::Instant::now();
        let (code, reply) = post(&h, TOKEN, job("s-second", None), true);
        assert_eq!(code, 503);
        let reason = reply.reason.unwrap();
        assert!(reason.starts_with("busy: studio"), "{reason}");
        assert!(reason.contains(&format!("{} is running on big", relay("s-long"))), "names what is running: {reason}");
        assert!(started.elapsed() < Duration::from_millis(500), "busy is answered at once, not queued");
        // The seat frees when the first job ends.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !h.seats.running().is_empty() {
            assert!(std::time::Instant::now() < deadline, "the seat never freed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (code, _) = post(&h, TOKEN, job("s-third", None), true);
        assert_eq!(code, 200);
    }

    /// (#2916 stage 2) Busy is per local MODEL: a job on another local
    /// model runs while the first is still running.
    #[test]
    fn a_job_on_another_local_model_is_not_busy() {
        let h = start(Some(laptop()), false, 600);
        allow_profiles(&h, &["host", "small", "cloud"]);
        let (code, _) = post(&h, TOKEN, job("s-big", None), false);
        assert_eq!(code, 202);
        let (code, reply) = post(&h, TOKEN, job("s-small", Some("small")), true);
        assert_eq!(code, 200, "a different local model is a different seat: {reply:?}");
        assert_eq!(reply.status, ReplyStatus::Completed);
    }

    /// (#2916 stage 2) Hosted jobs run beside each other up to this
    /// machine's `remote.concurrent_cap`, and busy past it.
    #[test]
    fn hosted_jobs_run_together_up_to_the_receivers_cap() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Refuse, 2, WIDE);
        allow_profiles(&h, &["host", "small", "cloud"]);
        assert_eq!(post(&h, TOKEN, job("c1", Some("cloud")), false).0, 202);
        assert_eq!(post(&h, TOKEN, job("c2", Some("cloud")), false).0, 202, "the second hosted job fits a cap of 2");
        let (code, reply) = post(&h, TOKEN, job("c3", Some("cloud")), true);
        assert_eq!(code, 503, "{reply:?}");
        let reason = reply.reason.unwrap();
        assert!(reason.contains("remote.concurrent_cap"), "{reason}");
        // A local job is not held up by hosted ones.
        assert_eq!(post(&h, TOKEN, job("l1", None), false).0, 202);
    }

    /// (#2916 stage 2) `queue`: a waited-on job hears it is queued at once,
    /// then runs when the seat frees and gets its result.
    #[test]
    fn a_queued_job_is_told_it_waits_then_runs() {
        let h = start_full(Some(laptop()), false, 500, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let mut progress = Vec::new();
        let (code, reply) = darkmux_fleet::post_submission_with_progress(
            &h.url,
            TOKEN,
            &WorkSubmission::new(job("s-second", None), true),
            Duration::from_secs(10),
            &mut |r| progress.push(r.clone()),
        )
        .unwrap();
        assert_eq!(code, 200, "{reply:?}");
        assert_eq!(reply.status, ReplyStatus::Completed, "{reply:?}");
        assert_eq!(reply.stdout.as_deref(), Some("ran radio-host on host"));
        assert!(!progress.is_empty(), "the sender was never told it waits");
        let first = progress[0].reason.clone().unwrap();
        assert!(first.contains("queued") && first.contains(&relay("s-first")), "{first}");
        let ran = h.ran.lock().unwrap().clone();
        assert_eq!(ran.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(), vec![relay("s-first"), relay("s-second")]);
    }

    /// (#2916 stage 2) `queue` without `--wait`: the answer is `queued`,
    /// at once, and the job still runs.
    #[test]
    fn a_queued_job_without_wait_is_answered_queued_and_still_runs() {
        let h = start_full(Some(laptop()), false, 300, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let started = std::time::Instant::now();
        let (code, reply) = post(&h, TOKEN, job("s-second", None), false);
        assert_eq!(code, 202, "{reply:?}");
        assert_eq!(reply.status, ReplyStatus::Queued);
        assert!(started.elapsed() < Duration::from_millis(250), "answered at once");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.ran.lock().unwrap().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "the queued job never ran");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// (#2916 stage 2 review C5) `/health` reports the listener's busy
    /// settings to this machine only.
    #[test]
    fn the_busy_settings_are_reported_to_this_machine_only() {
        *LISTENER_BUSY.lock().unwrap() = Some((BusyPolicy::Queue, 2));
        let local = listener_busy(true).unwrap();
        assert_eq!(serde_json::to_value(local).unwrap(), serde_json::json!({ "policy": "queue", "hosted_cap": 2 }));
        assert!(listener_busy(false).is_none(), "a peer sees nothing");
        // Process-global: leave it as the process started.
        *LISTENER_BUSY.lock().unwrap() = None;
    }

    /// Wait until `ran` holds `n` jobs (or fail after 10 s).
    fn wait_ran(h: &Harness, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while h.ran.lock().unwrap().len() < n {
            assert!(std::time::Instant::now() < deadline, "only {:?} ran", h.ran.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until nothing holds a seat and nothing is queued for the laptop.
    fn wait_idle(h: &Harness) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let slots_free = (0..NODE_CAP).map(|_| h.queue_slots.try_take("macbook-pro".into())).collect::<Vec<_>>();
            if h.seats.running().is_empty() && slots_free.iter().all(Option::is_some) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "the listener never went idle: {:?}", h.seats.running());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Post `j` with `wait` from a background thread; the reply and every
    /// `queued` line it heard come back on the handle.
    fn post_in_background(h: &Harness, j: WorkJob) -> std::thread::JoinHandle<(u16, SubmissionReply, Vec<SubmissionReply>)> {
        let url = h.url.clone();
        std::thread::spawn(move || {
            let mut heard = Vec::new();
            let (code, r) = darkmux_fleet::post_submission_with_progress(
                &url,
                TOKEN,
                &WorkSubmission::new(j, true),
                Duration::from_secs(20),
                &mut |p| heard.push(p.clone()),
            )
            .unwrap();
            (code, r, heard)
        })
    }

    /// (#2916 stage 2 review M1) A job queued while its sender was trusted is
    /// checked again when its seat frees: after `untrust`, it is refused, and
    /// it never runs.
    #[test]
    fn a_queued_job_is_refused_when_its_sender_was_untrusted_while_it_waited() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        h.allow.lock().unwrap().clear();
        let (_, reply, heard) = waiting.join().unwrap();
        assert!(!heard.is_empty(), "it was queued first");
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        assert!(reply.reason.unwrap().contains("does not accept work from macbook-pro"));
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "only the first job ran");
    }

    /// (#2916 stage 2 review M1) The same for a job queued without `--wait`:
    /// nobody is listening, and it still never runs.
    #[test]
    fn a_no_wait_queued_job_never_runs_after_untrust() {
        let h = start_full(Some(laptop()), false, 400, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        assert_eq!(post(&h, TOKEN, job("s-queued", None), false).1.status, ReplyStatus::Queued);
        h.allow.lock().unwrap().clear();
        wait_ran(&h, 1);
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "the queued job ran after untrust");
    }

    /// Start a queuing listener with a job running and a second, waited-on
    /// job queued behind it; `revoke` runs while it waits. The queued job's
    /// reply comes back, and nothing but the first job may have run.
    fn queue_then(revoke: impl FnOnce(&Harness)) -> (Harness, SubmissionReply) {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        revoke(&h);
        let (_, reply, heard) = waiting.join().unwrap();
        assert!(!heard.is_empty(), "it was queued first");
        wait_ran(&h, 1);
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "the queued job ran after it was revoked: {reply:?}");
        (h, reply)
    }

    fn refused_because(reply: &SubmissionReply, needle: &str) {
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        let reason = reply.reason.clone().unwrap_or_default();
        assert!(reason.contains(needle), "wanted {needle:?} in: {reason}");
    }

    /// (#2916 stage 2 review F1) Rotating the fleet token revokes a job
    /// already queued under the old one.
    #[test]
    fn a_queued_job_is_refused_after_the_fleet_token_rotates() {
        let (_, reply) = queue_then(|h| *h.token.lock().unwrap() = Some("rotated-token".into()));
        refused_because(&reply, "fleet token is missing or does not match");
    }

    /// (#2916 stage 2 review F1) So does removing the token altogether.
    #[test]
    fn a_queued_job_is_refused_after_the_fleet_token_is_removed() {
        let (_, reply) = queue_then(|h| *h.token.lock().unwrap() = None);
        refused_because(&reply, "no fleet token configured");
    }

    /// (#2916 stage 2 review F1) A sender removed from the network while its
    /// job waited is refused: the address is no node any more.
    #[test]
    fn a_queued_job_is_refused_after_its_sender_leaves_the_network() {
        let (_, reply) = queue_then(|h| h.network.set(vec![], None));
        refused_because(&reply, "did not come from a node");
    }

    /// (#2916 stage 2 review F1) A provider that cannot answer when the seat
    /// frees fails closed.
    #[test]
    fn a_queued_job_is_refused_when_the_identity_provider_is_down() {
        let (_, reply) = queue_then(|h| h.network.set(vec![laptop()], Some("daemon not running")));
        refused_because(&reply, "cannot tell which machine sent this request");
    }

    /// (#2916 stage 2 review F1) Another node now holding the sender's
    /// address (even one this machine also trusts) is not the sender.
    #[test]
    fn a_queued_job_is_refused_when_another_node_holds_its_senders_address() {
        let (_, reply) = queue_then(|h| {
            h.allow.lock().unwrap().insert(
                "phone".into(),
                AcceptWorkEntry {
                    node_id: Some("nPHONE".into()),
                    profiles: Some(vec!["host".into()]),
                    roles: Some(vec!["radio-host".into()]),
                    images: None,
                    workspace: Some(false),
                    extras: Default::default(),
                },
            );
            h.network.set(vec![test_node("nPHONE", "phone", "127.0.0.1")], None);
        });
        refused_because(&reply, "did not come from a node");
    }

    /// (#2916 stage 2 final review 2) The allow-list is read AFTER the
    /// identity lookup when a queued job is checked again: an `untrust` that
    /// lands while the provider is still answering is seen.
    #[test]
    fn an_untrust_during_the_rechecks_identity_lookup_is_seen() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        // From now on the provider takes 800 ms to answer: the recheck's
        // lookup starts when the first job ends, and the untrust lands
        // inside it.
        h.network.slow(Duration::from_millis(800));
        wait_ran(&h, 1);
        std::thread::sleep(Duration::from_millis(300));
        h.allow.lock().unwrap().clear();
        let (_, reply, _) = waiting.join().unwrap();
        refused_because(&reply, "does not accept work from macbook-pro");
        assert_eq!(h.ran.lock().unwrap().len(), 1, "the queued job ran");
    }

    /// (#2916 stage 2 final review 3) A sender that hangs up WHILE its job is
    /// being checked again (the seat already taken, the provider still
    /// answering) is caught by the check just before the job runs.
    #[test]
    fn a_sender_that_hangs_up_during_the_recheck_never_runs() {
        use std::io::{Read, Write};
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let body = serde_json::to_vec(&WorkSubmission::new(job("s-gone", None), true)).unwrap();
        let addr = h.url.trim_start_matches("http://").split('/').next().unwrap().to_string();
        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        write!(
            sock,
            "POST {} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            darkmux_fleet::SUBMISSION_PATH,
            body.len()
        )
        .unwrap();
        sock.write_all(&body).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 4096];
        while !String::from_utf8_lossy(&got).contains("\"queued\"") {
            let n = sock.read(&mut b).unwrap();
            assert!(n > 0, "the listener closed before saying queued");
            got.extend_from_slice(&b[..n]);
        }
        // The recheck's lookup will take 1.2 s; hang up inside it, after the
        // waiter has already taken the freed seat.
        h.network.slow(Duration::from_millis(1_200));
        wait_ran(&h, 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!h.seats.running().is_empty(), "the waiter should hold the seat, mid-recheck");
        drop(sock);
        wait_idle(&h);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.ran.lock().unwrap().len(), 1, "a job whose sender hung up during its recheck ran");
    }

    /// (#2916 stage 2 review C3) The scope is checked again: an entry
    /// narrowed while the job waited refuses a profile it no longer lists.
    #[test]
    fn a_queued_job_is_refused_when_its_entry_is_narrowed_while_it_waits() {
        let (_, reply) = queue_then(|h| {
            h.allow.lock().unwrap().get_mut("macbook-pro").unwrap().profiles = Some(vec!["small".into()]);
        });
        refused_because(&reply, "not in the allow-list scope: profile host");
    }

    /// (#2916 stage 2 review C4) A job runs under the allow-list entry as it
    /// stands when its seat frees: an entry renamed while it waited is the
    /// name the job is attributed to.
    #[test]
    fn a_queued_job_runs_under_its_entrys_current_name() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        {
            let mut a = h.allow.lock().unwrap();
            let entry = a.remove("macbook-pro").unwrap();
            a.insert("laptop".into(), entry);
        }
        let (_, reply, _) = waiting.join().unwrap();
        assert_eq!(reply.status, ReplyStatus::Completed, "{reply:?}");
        assert_eq!(h.origins(), vec!["macbook-pro".to_string(), "laptop".to_string()]);
    }

    /// (#2916 stage 2 review M1) The config preflight runs again when a
    /// queued job's seat frees: a receiver whose config went bad while the
    /// job waited refuses it.
    #[test]
    fn a_queued_job_is_refused_when_the_receivers_config_went_bad_while_it_waited() {
        let bad = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let b = bad.clone();
        let preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
            if b.load(std::sync::atomic::Ordering::SeqCst) {
                Err("dispatch: refusing to start: bad config `seroius`".into())
            } else {
                Ok(())
            }
        });
        let h = start_full(Some(laptop()), false, 600, preflight, BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        bad.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, reply, _) = waiting.join().unwrap();
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        assert!(reply.reason.unwrap().contains("`seroius`"));
        assert_eq!(h.ran.lock().unwrap().len(), 1, "only the first job ran");
    }

    /// (#2916 stage 2 review M1) The profile is resolved again too: if it now
    /// runs on another model than the seat the job waited for, the job is
    /// refused rather than run on a seat it does not hold.
    #[test]
    fn a_queued_job_is_refused_when_its_profile_moved_to_another_model() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, job("s-queued", None));
        std::thread::sleep(Duration::from_millis(200));
        h.model_moved.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, reply, _) = waiting.join().unwrap();
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        assert!(reply.reason.unwrap().contains("different model"));
        assert_eq!(h.ran.lock().unwrap().len(), 1, "only the first job ran");
    }

    fn managed_only(mut j: WorkJob) -> WorkJob {
        j.boundary = Some(darkmux_fleet::Boundary::ManagedOnly);
        j
    }

    fn check(mut j: WorkJob) -> WorkJob {
        j.mode = darkmux_fleet::SubmissionMode::Check;
        j
    }

    /// The boundary is checked when the job ARRIVES, against the profile the
    /// receiver resolves it to: a managed profile runs under `managed_only`,
    /// a hosted one is refused with the `boundary` code and nothing runs,
    /// and a job with no boundary runs on either.
    #[test]
    fn a_managed_only_job_is_refused_on_a_hosted_profile_and_runs_on_a_managed_one() {
        let h = start(Some(laptop()), false, 0);
        let (code, reply) = post(&h, TOKEN, managed_only(job("s-managed", Some("host"))), true);
        assert_eq!((code, reply.status), (200, ReplyStatus::Completed), "{reply:?}");
        let (code, reply) = post(&h, TOKEN, managed_only(job("s-hosted", Some("cloud"))), true);
        assert_eq!((code, reply.status, reply.refusal), (403, ReplyStatus::Refused, Some(darkmux_fleet::RefusalCode::Boundary)), "{reply:?}");
        assert!(reply.reason.unwrap().contains("hosted endpoint"), "the sentence says why");
        let (code, reply) = post(&h, TOKEN, job("s-free", Some("cloud")), true);
        assert_eq!((code, reply.status), (200, ReplyStatus::Completed), "no boundary, no restriction: {reply:?}");
        assert_eq!(h.ran.lock().unwrap().len(), 2, "the refused job never ran");
    }

    /// A boundary this receiver cannot enforce is refused, never run.
    #[test]
    fn a_boundary_the_receiver_does_not_know_is_refused() {
        let h = start(Some(laptop()), false, 0);
        let mut j = job("s-future", Some("host"));
        j.boundary = Some(darkmux_fleet::Boundary::Unknown);
        let (code, reply) = post(&h, TOKEN, j, true);
        assert_eq!((code, reply.refusal), (403, Some(darkmux_fleet::RefusalCode::Boundary)), "{reply:?}");
        assert!(h.ran.lock().unwrap().is_empty());
    }

    /// (#3028) The receiver's own hardware uid reaches both places the
    /// listener checks a job's address (a run, and a `check`): a job
    /// addressed to the machine's former name, carrying its uid, is taken; the
    /// same name with another machine's uid is `misaddressed`.
    #[test]
    fn a_job_for_the_former_name_with_the_receivers_uid_is_taken_over_http() {
        let h = start_full_as(Some(laptop()), false, 0, Arc::new(|| Ok(())), BusyPolicy::Refuse, 1, WIDE, Some("UID-STUDIO"));
        let mut former = job("s-old", None);
        former.target_machine = "m1-max-32gb-studio".into();
        former.target_machine_uid = Some("uid-studio".into());
        let (code, reply) = post(&h, TOKEN, former.clone(), true);
        assert_eq!((code, reply.status), (200, ReplyStatus::Completed), "{reply:?}");
        let mut check = former;
        check.mode = darkmux_fleet::SubmissionMode::Check;
        check.session_id = sender("s-old-check");
        let (_, reply) = post(&h, TOKEN, check, false);
        assert_eq!(reply.status, ReplyStatus::Checked, "{reply:?}");
        let mut other = job("s-other", None);
        other.target_machine_uid = Some("UID-ELSEWHERE".into());
        let (code, reply) = post(&h, TOKEN, other, true);
        assert_eq!((code, reply.refusal), (421, Some(darkmux_fleet::RefusalCode::Misaddressed)), "{reply:?}");
    }

    /// The typed code reaches the sender for each refusal, beside the
    /// sentence, over real HTTP.
    #[test]
    fn each_refusal_reaches_the_sender_with_its_code() {
        use darkmux_fleet::RefusalCode as C;
        let h = start(Some(laptop()), false, 0);
        let cases: Vec<(&str, WorkJob, C)> = vec![
            ("wrong-token", job("c1", None), C::Token),
            (TOKEN, job("c2", Some("coder-big")), C::ProfileNotAllowed),
            (TOKEN, job("c3", Some("utility")), C::ProfileNotAllowed),
            (TOKEN, { let mut j = job("c4", None); j.workdir = Some("/x".into()); j }, C::WorkspaceNotAllowed),
            (TOKEN, { let mut j = job("c5", None); j.target_machine = "mini".into(); j }, C::Misaddressed),
            (TOKEN, { let mut j = job("c6", None); j.role_id = "coder".into(); j }, C::RoleNotAllowed),
            (TOKEN, { let mut j = job("c7", None); j.image = Some("evil.example/x".into()); j }, C::ImageNotAllowed),
        ];
        for (token, j, want) in cases {
            let (_, reply) = post(&h, token, j, true);
            assert_eq!(reply.refusal, Some(want), "{reply:?}");
        }
    }

    /// (M1 of the boundary) A queued job is checked against its boundary
    /// again when its seat frees: a profile that moved to a hosted endpoint
    /// while the job waited refuses it with the `boundary` code, and it never
    /// runs. Deterministic: the first job holds the seat until the flip is
    /// made, and the waiter is decided only after it frees.
    #[test]
    fn a_queued_managed_only_job_is_refused_when_its_profile_went_hosted_while_it_waited() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let waiting = post_in_background(&h, managed_only(job("s-queued", None)));
        std::thread::sleep(Duration::from_millis(200));
        h.went_hosted.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, reply, heard) = waiting.join().unwrap();
        assert!(!heard.is_empty(), "it was queued first, on its managed seat");
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        assert_eq!(reply.refusal, Some(darkmux_fleet::RefusalCode::Boundary), "{reply:?}");
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "only the first job ran");
    }

    /// A check answers `checked` with what a run would meet, and runs
    /// nothing, holds no seat, and starts no worker.
    #[test]
    fn a_check_answers_what_a_run_would_meet_and_runs_nothing() {
        let h = start(Some(laptop()), false, 0);
        let (code, reply) = post(&h, TOKEN, check(managed_only(job("k1", Some("host")))), true);
        assert_eq!((code, reply.status), (200, ReplyStatus::Checked), "{reply:?}");
        assert_eq!(reply.profile.as_deref(), Some("host"));
        assert_eq!(
            reply.check,
            Some(darkmux_fleet::CheckReport { endpoint: darkmux_fleet::EndpointClass::Managed, seat: darkmux_fleet::SeatOutlook::Free })
        );
        assert_eq!(reply.session_id, None, "no session was minted for a check");
        let (_, hosted) = post(&h, TOKEN, check(job("k2", Some("cloud"))), true);
        assert_eq!(hosted.check.unwrap().endpoint, darkmux_fleet::EndpointClass::Unmanaged);
        assert!(h.ran.lock().unwrap().is_empty(), "a check ran a job");
        assert!(h.seats.running().is_empty(), "a check held a seat");
    }

    /// A check meets every gate a run meets and refuses with the same code:
    /// the token, the allow-list, role and profile scope, the boundary, the
    /// version, and the busy seat under `refuse`.
    #[test]
    fn a_check_is_refused_exactly_as_a_run_would_be() {
        use darkmux_fleet::RefusalCode as C;
        let h = start_full(Some(laptop()), false, 800, Arc::new(|| Ok(())), BusyPolicy::Refuse, 1, WIDE);
        let cases: Vec<(&str, WorkJob, C)> = vec![
            ("wrong-token", check(job("k1", None)), C::Token),
            (TOKEN, check(job("k2", Some("coder-big"))), C::ProfileNotAllowed),
            (TOKEN, check({ let mut j = job("k4", None); j.role_id = "coder".into(); j }), C::RoleNotAllowed),
            (TOKEN, check(managed_only(job("k5", Some("cloud")))), C::Boundary),
        ];
        for (token, j, want) in cases {
            let mut run = j.clone();
            run.mode = darkmux_fleet::SubmissionMode::Run;
            let (_, checked) = post(&h, token, j, true);
            let (_, ran) = post(&h, token, run, false);
            assert_eq!(checked.status, ReplyStatus::Refused, "{checked:?}");
            assert_eq!(checked.refusal, Some(want), "{checked:?}");
            assert_eq!(ran.refusal, Some(want), "the run is refused the same way: {ran:?}");
        }
        // Busy under `refuse`: the same code a run gets, and the check
        // itself never took the seat.
        assert_eq!(post(&h, TOKEN, job("k-first", None), false).0, 202);
        let (_, busy) = post(&h, TOKEN, check(job("k6", None)), true);
        assert_eq!((busy.status, busy.refusal), (ReplyStatus::Refused, Some(C::Busy)), "{busy:?}");
        wait_ran(&h, 1);
        wait_idle(&h);
        let (_, free) = post(&h, TOKEN, check(job("k7", None)), true);
        assert_eq!(free.status, ReplyStatus::Checked, "the seat is free again: {free:?}");
        assert_eq!(h.ran.lock().unwrap().len(), 1, "no check ran anything");
    }

    /// Under `queue` a check on a busy seat says the run would wait, takes no
    /// queue slot, and leaves the queue as it was.
    #[test]
    fn a_check_on_a_busy_seat_under_queue_says_would_queue_and_takes_no_slot() {
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("q-first", None), false).0, 202);
        for i in 0..NODE_CAP + 2 {
            let (_, reply) = post(&h, TOKEN, check(job(&format!("q-check-{i}"), None)), true);
            assert_eq!(reply.check.map(|c| c.seat), Some(darkmux_fleet::SeatOutlook::WouldQueue), "check {i}: {reply:?}");
        }
        wait_ran(&h, 1);
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "no check ran, and none was queued to run");
    }

    /// A check answers a full queue as a run does: `busy`.
    #[test]
    fn a_check_when_the_peers_queue_is_full_is_refused_busy() {
        let h = start_full(Some(laptop()), false, 1_500, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("f-first", None), false).0, 202);
        let _slots: Vec<_> = (0..NODE_CAP).map(|_| h.queue_slots.try_take("macbook-pro".into()).unwrap()).collect();
        let (_, reply) = post(&h, TOKEN, check(job("f-check", None)), true);
        assert_eq!((reply.status, reply.refusal), (ReplyStatus::Refused, Some(darkmux_fleet::RefusalCode::Busy)), "{reply:?}");
    }

    /// A newer minor is refused naming both versions, over the wire, with the
    /// `version` code; the current version is taken.
    #[test]
    fn a_newer_minor_is_refused_with_the_version_code_over_the_wire() {
        let h = start(Some(laptop()), false, 0);
        let mut newer = WorkSubmission::new(job("v1", None), true);
        newer.schema = "8.9".into();
        let (code, reply) =
            darkmux_fleet::post_submission(&h.url, TOKEN, &newer, Duration::from_secs(10)).unwrap();
        assert_eq!((code, reply.refusal), (400, Some(darkmux_fleet::RefusalCode::Version)), "{reply:?}");
        let reason = reply.reason.unwrap();
        assert!(reason.contains("v8.9") && reason.contains(&format!("v{}", darkmux_fleet::WORK_JOB_SCHEMA_VERSION)), "{reason}");
        assert!(h.ran.lock().unwrap().is_empty());
    }

    /// (#2916 stage 2 review M2) A sender that waits on a queued job and then
    /// hangs up gives its place back: its queue slot frees, and its job never
    /// runs.
    #[test]
    fn a_waiting_sender_that_hangs_up_frees_its_slot_and_its_job_never_runs() {
        use std::io::{Read, Write};
        let h = start_full(Some(laptop()), false, 1_500, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        let body = serde_json::to_vec(&WorkSubmission::new(job("s-gone", None), true)).unwrap();
        let addr = h.url.trim_start_matches("http://").split('/').next().unwrap().to_string();
        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        write!(
            sock,
            "POST {} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            darkmux_fleet::SUBMISSION_PATH,
            body.len()
        )
        .unwrap();
        sock.write_all(&body).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 4096];
        while !String::from_utf8_lossy(&got).contains("\"queued\"") {
            let n = sock.read(&mut b).unwrap();
            assert!(n > 0, "the listener closed before saying queued");
            got.extend_from_slice(&b[..n]);
        }
        drop(sock);
        // (#2916 stage 2 review C2) Its place comes back WHILE the first job
        // still runs: the wait itself notices the hang-up. Without that, the
        // slot would free only once the first job released the seat.
        let deadline = std::time::Instant::now() + Duration::from_millis(1_000);
        loop {
            let slots: Vec<_> = (0..NODE_CAP).map(|_| h.queue_slots.try_take("macbook-pro".into())).collect();
            if slots.iter().all(Option::is_some) {
                break;
            }
            drop(slots);
            assert!(std::time::Instant::now() < deadline, "the hung-up waiter still holds its queue slot");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.ran.lock().unwrap().is_empty(), "the first job was still running when the slot freed");
        wait_ran(&h, 1);
        wait_idle(&h);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.ran.lock().unwrap().len(), 1, "the abandoned job ran");
    }

    /// (#2916 stage 2 review C1) A waited-on job whose timeout leaves no time
    /// to wait inside one connection is answered busy at once, not queued.
    #[test]
    fn a_waited_job_with_no_room_in_its_connection_is_busy_not_queued() {
        let tight = QueueLimits { connection_lifetime: Duration::from_secs(120), no_wait_max_age: Duration::from_secs(60) };
        let h = start_full(Some(laptop()), false, 600, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, tight);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        // A 60 s job in a 120 s connection, less 60 s of slack: no time left.
        let (code, reply) = post(&h, TOKEN, job("s-second", None), true);
        assert_eq!((code, reply.status), (503, ReplyStatus::Refused), "{reply:?}");
        assert!(reply.reason.unwrap().contains("no time left to wait"));
    }

    /// (#2916 stage 2 review C1) A queued job that waits past its limit is
    /// dropped: busy to a waiting sender, silently for a job sent without
    /// `--wait`. Neither runs.
    #[test]
    fn a_queued_job_past_its_limit_is_dropped_and_never_runs() {
        let short = QueueLimits {
            connection_lifetime: Duration::from_secs(60) + QUEUE_SLACK + Duration::from_millis(300),
            no_wait_max_age: Duration::from_millis(300),
        };
        let h = start_full(Some(laptop()), false, 1_500, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, short);
        assert_eq!(post(&h, TOKEN, job("s-first", None), false).0, 202);
        assert_eq!(post(&h, TOKEN, job("s-nowait", None), false).1.status, ReplyStatus::Queued);
        let (_, reply, heard) = post_in_background(&h, job("s-wait", None)).join().unwrap();
        assert!(!heard.is_empty());
        assert_eq!(reply.status, ReplyStatus::Refused, "{reply:?}");
        assert!(reply.reason.unwrap().contains("waited as long as it may"));
        wait_ran(&h, 1);
        wait_idle(&h);
        assert_eq!(h.ran.lock().unwrap().len(), 1, "a timed-out queued job ran");
    }

    /// (#2916 stage 2) A peer may queue at most `NODE_CAP` jobs.
    #[test]
    fn a_peer_cannot_queue_without_bound() {
        let h = start_full(Some(laptop()), false, 2_000, Arc::new(|| Ok(())), BusyPolicy::Queue, 1, WIDE);
        assert_eq!(post(&h, TOKEN, job("s-run", None), false).0, 202);
        for i in 0..NODE_CAP {
            let (code, r) = post(&h, TOKEN, job(&format!("s-q{i}"), None), false);
            assert_eq!((code, r.status), (202, ReplyStatus::Queued), "{r:?}");
        }
        let (code, r) = post(&h, TOKEN, job("s-over", None), false);
        assert_eq!(code, 503, "{r:?}");
        assert!(r.reason.unwrap().contains("as many jobs queued"));
    }

    /// A listener state whose allow-list read is counted in `reads` and
    /// whose executor must never run: for tests about what the gate refuses.
    fn gate_only_state(reads: Arc<std::sync::atomic::AtomicUsize>) -> FleetListenerState {
        FleetListenerState {
            receiver: "studio".into(),
            receiver_uid: None,
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            local_node_id: Some("nSTUDIO".into()),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(move || {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allow())
            }),
            resolve_profile: Arc::new(|_, _| test_resolution(None)),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            card_cache: Arc::new(crate::machine_card::CardCache::new(Duration::ZERO)),
            identity_cache: Arc::new(IdentityCache::new()),
            config_preflight: Arc::new(|| Ok(())),
        }
    }

    /// Submit a job from a loopback peer carrying `headers`; returns the
    /// status and how many times the allow-list was read.
    fn submit_from_loopback(headers: &[(&str, &str)]) -> (StatusCode, usize) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = gate_only_state(reads.clone());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let resp = rt.block_on(async {
            use tower::ServiceExt;
            let body = serde_json::to_vec(&WorkSubmission::new(job("s", None), true)).unwrap();
            let mut b = axum::http::Request::post(darkmux_fleet::SUBMISSION_PATH);
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            let mut req = b.body(axum::body::Body::from(body)).unwrap();
            req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
            router(state).oneshot(req).await.unwrap()
        });
        (resp.status(), reads.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// A caller without the token is answered before the allow-list is
    /// read or the provider runs.
    #[test]
    fn a_caller_without_the_token_reads_nothing() {
        let (status, reads) = submit_from_loopback(&[("Authorization", "Bearer wrong")]);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(reads, 0, "the allow-list was read for a caller without the token");
    }

    /// (#2988 follow-up) Execution never inherits the read posture: a work
    /// submission that arrives through a reverse proxy (`tailscale serve`'s
    /// headers) with no token is refused before anything is read, whatever
    /// `serve.read_auth` says. The fleet gate reads no read-auth switch.
    #[test]
    fn a_proxied_submission_without_the_token_is_refused() {
        for proxied in [
            ("X-Forwarded-For", "100.64.0.7"),
            ("Tailscale-User-Login", "someone@example.com"),
            ("Forwarded", "for=100.64.0.7"),
        ] {
            let (status, reads) = submit_from_loopback(&[proxied]);
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{proxied:?}");
            assert_eq!(reads, 0, "{proxied:?}");
        }
    }

    /// A request with no peer address is refused (fail closed), even with
    /// the right token.
    #[test]
    fn a_request_without_a_peer_address_is_refused() {
        let h_state = FleetListenerState {
            receiver: "studio".into(),
            receiver_uid: None,
            local_node_id: Some("nSTUDIO".into()),
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| test_resolution(None)),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            card_cache: Arc::new(crate::machine_card::CardCache::new(Duration::ZERO)),
            identity_cache: Arc::new(IdentityCache::new()),
            config_preflight: Arc::new(|| Ok(())),
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
            receiver_uid: None,
            local_node_id: Some("nSTUDIO".into()),
            provider: slow.clone(),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| test_resolution(None)),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            card_cache: Arc::new(crate::machine_card::CardCache::new(Duration::ZERO)),
            identity_cache: Arc::new(IdentityCache::new()),
            config_preflight: Arc::new(|| Ok(())),
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

    /// A listener whose node cap is ONE request and whose profile resolution
    /// takes `resolve_ms`, so a job holds the node's only slot for that long.
    /// Returns its base URL.
    fn spawn_one_slot_listener(resolve_ms: u64) -> String {
        let state = FleetListenerState {
            receiver: "studio".into(),
            receiver_uid: None,
            local_node_id: Some("nSTUDIO".into()),
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(move |_, _| {
                std::thread::sleep(Duration::from_millis(resolve_ms));
                test_resolution(None)
            }),
            execute: Arc::new(|j: WorkJob, _, _| {
                Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: j.session_id, execution: None, out_dir: None, trajectory: None })
            }),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(1)),
            card_slots: Arc::new(KeySlots::new(CARD_NODE_CAP)),
            card_cache: Arc::new(crate::machine_card::CardCache::new(Duration::ZERO)),
            identity_cache: Arc::new(IdentityCache::new()),
            config_preflight: Arc::new(|| Ok(())),
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
        format!("http://127.0.0.1:{port}")
    }

    /// (#2916 round 3 C5) Once identified, one node has at most its cap of
    /// requests in flight, whatever addresses they came from.
    #[test]
    fn one_node_has_at_most_its_cap_of_requests_in_flight() {
        let url = format!("{}{}", spawn_one_slot_listener(600), darkmux_fleet::SUBMISSION_PATH);
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

    /// A card read takes a slot of its own: a sender whose job holds every
    /// request slot it has still gets its card, and a sender's card reads are
    /// still capped (a poller cannot ask for more than a few at a time).
    #[test]
    fn a_sender_whose_job_slots_are_full_still_gets_its_card() {
        let base = spawn_one_slot_listener(1_500);
        let work = format!("{base}{}", darkmux_fleet::SUBMISSION_PATH);
        let running = std::thread::spawn(move || {
            darkmux_fleet::post_submission(&work, TOKEN, &WorkSubmission::new(job("s1", None), true), Duration::from_secs(10)).unwrap()
        });
        std::thread::sleep(Duration::from_millis(300));
        let (code, body) = get_card(&format!("{base}{}", darkmux_fleet::CARD_PATH), TOKEN);
        assert_eq!(code, 200, "the job holds the node's only request slot: {body}");
        assert_eq!(body["grant"]["accepts"]["peer_name"], "macbook-pro");
        assert_eq!(running.join().unwrap().0, 200);
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

    /// A listener that is switched off says so on `/health`, to a peer and to
    /// this machine, instead of reporting `null`.
    #[serial_test::serial]
    #[test]
    fn an_off_listener_reports_off_not_null() {
        *LISTENER_STATE.lock().unwrap() = None;
        unsafe { std::env::set_var("DARKMUX_FLEET_LISTENER_ENABLED", "false") };
        let (_tx, rx) = tokio::sync::watch::channel(false);
        spawn_if_enabled(rx);
        unsafe { std::env::remove_var("DARKMUX_FLEET_LISTENER_ENABLED") };
        assert_eq!(listener_state(false).as_deref(), Some("off"));
        assert_eq!(listener_state(true).as_deref(), Some("off (fleet.listener.enabled is false)"));
        *LISTENER_STATE.lock().unwrap() = None;
    }

    /// Only the `Bearer` scheme carries the token: the same value under any
    /// other scheme, or with no scheme, is a mismatch. The scheme name is
    /// case-insensitive.
    #[test]
    fn only_the_bearer_scheme_carries_the_token() {
        let check = |value: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(axum::http::header::AUTHORIZATION, value.parse().unwrap());
            check_token(&h, Some("s3cret".to_string()))
        };
        assert!(matches!(check("Bearer s3cret"), TokenCheck::Match));
        assert!(matches!(check("bEaReR s3cret"), TokenCheck::Match), "the scheme is case-insensitive");
        assert!(matches!(check("Basic s3cret"), TokenCheck::Mismatch), "the token under another scheme");
        assert!(matches!(check("Token s3cret"), TokenCheck::Mismatch));
        assert!(matches!(check("s3cret"), TokenCheck::Mismatch), "no scheme at all");
        assert!(matches!(check("Bearer wrong"), TokenCheck::Mismatch));
    }

    #[test]
    fn a_peer_sees_only_the_coarse_listener_state() {
        set_state("listening", "listening on 100.64.0.2:8766");
        assert_eq!(listener_state(false).as_deref(), Some("listening"));
        assert_eq!(listener_state(true).as_deref(), Some("listening on 100.64.0.2:8766"));
    }

    /// (#2988 review) The listener's no-token reason names the switch that
    /// reads the Keychain, so an operator with the item stored is not told it
    /// is missing.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_listener_without_a_token_names_the_keychain_switch() {
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN") };
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let said = run(rx).await.expect_err("no token, no listener");
        assert!(said.contains("serve.token_keychain"), "{said}");
    }

    #[test]
    fn the_open_file_limit_is_raised_never_lowered() {
        let (old, new) = crate::raise_open_file_limit(1).expect("getrlimit works");
        assert_eq!(new, old, "a target below the current soft limit changes nothing");
    }

    #[test]
    // Without the test-only `e2e-fleet-loopback` feature, which is how every
    // release binary is built: loopback is refused.
    #[cfg(not(feature = "e2e-fleet-loopback"))]
    fn the_listener_binds_only_a_specific_overlay_address() {
        let local = test_node("n", "studio", "100.64.0.2");
        assert_eq!(listen_addr(&local, 8766).unwrap().to_string(), "100.64.0.2:8766");
        for bad in ["0.0.0.0", "127.0.0.1", "::", "::ffff:127.0.0.1"] {
            let n = test_node("n", "studio", bad);
            assert!(listen_addr(&n, 8766).is_err(), "{bad}");
        }
        let mut none = local.clone();
        none.addresses.clear();
        assert!(listen_addr(&none, 8766).is_err());
    }
}
