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
    Admitted, IdentityProvider, ProfileResolution, Refusal, ReplyStatus, ScopedJob, SeatBook, SeatGuard,
    SubmissionReply, TokenCheck, Waited, WorkJob, WorkSubmission,
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
    /// (#2947) This machine's dispatch-scope config preflight, run per
    /// submission before the job is accepted. `Err` carries the refusal
    /// text. Production: `darkmux_crew::user_files::preflight(Scope::Dispatch)`.
    pub config_preflight: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

/// (#2947) The production config preflight a submission runs.
pub(crate) fn dispatch_config_preflight() -> Result<(), String> {
    darkmux_crew::user_files::preflight(darkmux_types::config_enum::Scope::Dispatch).map_err(|e| e.to_string())
}

impl FleetListenerState {
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
            config_preflight: Arc::new(dispatch_config_preflight),
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
    let expected = (state.token)();
    let token = check_token(req.headers(), expected.clone());
    // A caller without the token is answered before this machine reads its
    // allow-list or runs the provider's tool (`admit` checks it again).
    match token {
        TokenCheck::Match => {
            // (#2916 stage 2 review F1) Which token was in force, so a job
            // queued now can be checked against the token in force when its
            // seat frees. A fingerprint, never the token.
            if let Some(t) = expected.as_deref() {
                req.extensions_mut().insert(TokenFingerprint::of(t));
            }
        }
        TokenCheck::Mismatch => return refuse(&state, Some(peer), &Refusal::Token),
        TokenCheck::NotConfigured => return refuse(&state, Some(peer), &Refusal::NoTokenConfigured),
    }
    // The identity lookup is blocking (a subprocess) and runs only after the
    // token matched: `admit` calls this closure after its token check, and
    // reads the allow-list after the lookup.
    let provider = state.provider.clone();
    let provider_name = provider.provider_name().to_string();
    let local_id = state.local_node_id.clone();
    let allow_list = state.allow_list.clone();
    let decision = tokio::task::spawn_blocking(move || {
        darkmux_fleet::admit(
            token,
            || provider.identify(peer).map_err(|e| format!("{e:#}")),
            &provider_name,
            peer,
            local_id.as_deref(),
            || allow_list(),
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

/// Decide how a scoped job starts: its seat now, a place in the queue, or a
/// busy refusal.
fn start_or_refuse(
    state: &FleetListenerState,
    admitted: &Admitted,
    scoped: &ScopedJob,
    session_id: &str,
    sub: &WorkSubmission,
) -> Result<Start, Refusal> {
    let occupied = match state.seats.try_claim(&scoped.seat, session_id) {
        Ok(guard) => return Ok(Start::Now(guard)),
        Err(occupied) => occupied,
    };
    match state.busy_policy {
        BusyPolicy::Refuse => Err(Refusal::Busy { what: occupied.what }),
        BusyPolicy::Queue => {
            let Some(deadline) = queue_deadline(&state.queue_limits, sub.wait, sub.job.timeout_seconds) else {
                return Err(Refusal::Busy {
                    what: format!(
                        "{}; a job with a {}s timeout has no time left to wait inside one connection",
                        occupied.what, sub.job.timeout_seconds
                    ),
                });
            };
            match state.queue_slots.try_take(admitted.peer_name.clone()) {
                Some(queue_slot) => Ok(Start::Queued { what: occupied.what, queue_slot, deadline }),
                None => Err(Refusal::QueueFull { peer: admitted.peer_name.clone(), what: occupied.what }),
            }
        }
    }
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
    /// (`serve_token`'s Keychain tier and `daemon_auth_enabled` are cached
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
        let now = darkmux_fleet::check_scope(&state.receiver, &admitted, &self.job, resolution)?;
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
        let sid = &self.job.session_id;
        let waited = state.seats.claim_waiting(&self.scoped.seat, sid, state.queue_heartbeat, deadline, &|| self.sender_gone(), |o| {
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

async fn submit_handler(
    State(state): State<FleetListenerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Extension(admitted): Extension<Admitted>,
    Extension(token): Extension<TokenFingerprint>,
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
    let mut sub = match WorkSubmission::parse(&body) {
        Ok(s) => s,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };
    let resolve = state.resolve_profile.clone();
    let (role, requested) = (sub.job.role_id.clone(), sub.job.profile.clone());
    let resolution = match tokio::task::spawn_blocking(move || resolve(&role, requested.as_deref())).await {
        Ok(r) => r,
        Err(e) => ProfileResolution::Unresolved(format!("profile resolution did not finish: {e}")),
    };
    let scoped = match darkmux_fleet::check_scope(&receiver, &admitted, &sub.job, resolution) {
        Ok(s) => s,
        Err(r) => return refuse(&state, Some(peer_addr.ip()), &r),
    };

    // (#2916 review C2) The receiver's own id for this run, never the
    // sender's verbatim.
    sub.job.session_id = darkmux_fleet::receiver_session_id(&sub.job.session_id, &admitted.peer_name);
    // (#2916 re-review C4) A submitted job is never attributed to one of
    // THIS machine's own missions: no allow-list scope grants that, so any
    // `phase_id` the sender set is dropped here.
    sub.job.phase_id = None;
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
    if ip.is_unspecified() || (ip.is_loopback() && !LOOPBACK_FOR_E2E) || ip.is_multicast() {
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
static LISTENER_STATE: std::sync::Mutex<Option<(&'static str, String)>> = std::sync::Mutex::new(None);

/// (#2916 stage 2 review C5) The busy policy and hosted-job bound the
/// running listener was started with (it reads config once), so `darkmux
/// doctor` can report what is in force rather than what the file says now.
pub(crate) static LISTENER_BUSY: std::sync::Mutex<Option<(BusyPolicy, u32)>> = std::sync::Mutex::new(None);

/// [`LISTENER_BUSY`] for `/health`, for this machine only (`local` is
/// `is_local_request`'s answer); `None` when the listener has not started.
pub(crate) fn listener_busy(local: bool) -> Option<serde_json::Value> {
    if !local {
        return None;
    }
    let (policy, cap) = (*LISTENER_BUSY.lock().ok()?)?;
    Some(serde_json::json!({ "policy": policy.as_str(), "hosted_cap": cap }))
}

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

/// Requests one admitted node may have in flight at once (#2916 round 3 C5),
/// and (#2916 stage 2) jobs it may have queued at once.
pub(crate) const NODE_CAP: usize = 4;

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
        return Err("no fleet token (the serve token: Keychain item `darkmux-serve-token` or \
                    DARKMUX_SERVE_TOKEN); a listener that cannot check a token takes no work"
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
    struct Switchable(Mutex<StaticIdentityProvider>, Mutex<Duration>);

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
        let allow_now = Arc::new(Mutex::new(allow()));
        let allow_read = allow_now.clone();
        let queue_slots = Arc::new(KeySlots::new(NODE_CAP));
        let model_moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let moved = model_moved.clone();
        let local = test_node("nSTUDIO", "studio", "100.64.0.2");
        let network = Arc::new(Switchable(
            Mutex::new(StaticIdentityProvider {
                local,
                peers: peer.into_iter().collect(),
                down: down.then(|| "daemon not running".to_string()),
            }),
            Mutex::new(Duration::ZERO),
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
            provider: network.clone(),
            local_node_id: Some("nSTUDIO".into()),
            token: Arc::new(move || token_read.lock().unwrap().clone()),
            allow_list: Arc::new(move || Ok(allow_read.lock().unwrap().clone())),
            resolve_profile: Arc::new(move |_role, requested| {
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
                assert!(job.phase_id.is_none(), "a submitted job's phase_id must be dropped (#2916 re-review C4)");
                ran_c.lock().unwrap().push((job.session_id.clone(), profile.clone()));
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: format!("ran {} on {profile}", job.role_id),
                    stderr: String::new(),
                    session_id: job.session_id,
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
        Harness { refusal_log, url: format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH), ran, seats, allow: allow_now, queue_slots, model_moved, token: token_now, network, origins }
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
        assert_eq!(reply.status, ReplyStatus::Completed);
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
        assert!(reason.contains("s-long-from-macbook-pro is running on big"), "names what is running: {reason}");
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
        assert!(first.contains("queued") && first.contains("s-first-from-macbook-pro"), "{first}");
        let ran = h.ran.lock().unwrap().clone();
        assert_eq!(ran.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), vec!["s-first-from-macbook-pro", "s-second-from-macbook-pro"]);
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
        assert_eq!((local["policy"].as_str(), local["hosted_cap"].as_u64()), (Some("queue"), Some(2)));
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
            resolve_profile: Arc::new(|_, _| test_resolution(None)),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
            config_preflight: Arc::new(|| Ok(())),
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
            resolve_profile: Arc::new(|_, _| test_resolution(None)),
            execute: Arc::new(|_, _, _| panic!("never runs")),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(NODE_CAP)),
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
                test_resolution(None)
            }),
            execute: Arc::new(|j: WorkJob, _, _| {
                Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: j.session_id, out_dir: None, trajectory: None })
            }),
            seats: Arc::new(SeatBook::new(1)),
            busy_policy: BusyPolicy::Refuse,
            queue_slots: Arc::new(KeySlots::new(NODE_CAP)),
            queue_heartbeat: QUEUE_HEARTBEAT,
            queue_limits: QueueLimits::PRODUCTION,
            refusal_log: Arc::new(RefusalLog::new()),
            node_slots: Arc::new(KeySlots::new(1)),
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
    // Without the test-only `e2e-fleet-loopback` feature, which is how every
    // release binary is built: loopback is refused.
    #[cfg(not(feature = "e2e-fleet-loopback"))]
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
