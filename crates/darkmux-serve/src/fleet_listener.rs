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

/// Everything the listener needs, injectable so tests drive the real router
/// over a real socket with a fake provider and a fake executor.
#[derive(Clone)]
pub(crate) struct FleetListenerState {
    /// This machine's name (its `machine_id`).
    pub receiver: String,
    pub provider: Arc<dyn IdentityProvider>,
    /// The expected fleet token, read per request (`None` = not configured).
    pub token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// The allow-list, read per request. An error refuses everything.
    pub allow_list: Arc<dyn Fn() -> Result<AllowList, String> + Send + Sync>,
    /// (role, requested profile) → what this machine would run.
    pub resolve_profile: Arc<dyn Fn(&str, Option<&str>) -> ProfileResolution + Send + Sync>,
    /// Runs an admitted, in-scope job on the resolved profile. Blocking.
    pub execute: Arc<dyn Fn(WorkJob, String) -> anyhow::Result<DispatchResult> + Send + Sync>,
    /// The one submitted job running now, by session id.
    pub busy: Arc<Mutex<Option<String>>>,
}

impl FleetListenerState {
    /// Production wiring: the configured provider, the serve token, the
    /// allow-list from `config.json`, this machine's registry, and
    /// `darkmux_fleet::execute_job`.
    pub(crate) fn production(receiver: String, provider: Arc<dyn IdentityProvider>) -> Self {
        let resolve_receiver = receiver.clone();
        Self {
            receiver,
            provider,
            token: Arc::new(|| darkmux_flow::serve_token().map(|t| t.expose_for_compare().to_string())),
            allow_list: Arc::new(darkmux_fleet::read_user_allow_list),
            resolve_profile: Arc::new(move |role, requested| {
                darkmux_fleet::resolve_work_profile(role, requested, &resolve_receiver)
            }),
            execute: Arc::new(darkmux_fleet::execute_job),
            busy: Arc::new(Mutex::new(None)),
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

fn refuse(receiver: &str, r: &Refusal) -> Response {
    let code = StatusCode::from_u16(r.http_status()).unwrap_or(StatusCode::FORBIDDEN);
    eprintln!("darkmux fleet: refused — {}", r.reason(receiver));
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
    let receiver = state.receiver.clone();
    let Some(peer) = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()) else {
        return refuse(
            &receiver,
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
        TokenCheck::Mismatch => return refuse(&receiver, &Refusal::Token),
        TokenCheck::NotConfigured => return refuse(&receiver, &Refusal::NoTokenConfigured),
    }
    let allow = match (state.allow_list)() {
        Ok(a) => a,
        Err(e) => {
            return refuse(
                &receiver,
                &Refusal::BadRequest(format!("this machine's allow-list cannot be read ({e}); refusing everything")),
            )
        }
    };
    // The identity lookup is blocking (a subprocess) and runs only after the
    // token matched: `admit` calls this closure after its token check.
    let provider = state.provider.clone();
    let provider_name = provider.provider_name().to_string();
    let decision = tokio::task::spawn_blocking(move || {
        darkmux_fleet::admit(
            token,
            || provider.identify(peer).map_err(|e| format!("{e:#}")),
            &provider_name,
            peer,
            &allow,
        )
    })
    .await;
    match decision {
        Ok(Ok(admitted)) => {
            req.extensions_mut().insert(admitted);
            next.run(req).await
        }
        Ok(Err(refusal)) => refuse(&receiver, &refusal),
        Err(e) => refuse(
            &receiver,
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
    Extension(admitted): Extension<Admitted>,
    body: Bytes,
) -> Response {
    let receiver = state.receiver.clone();
    let sub = match WorkSubmission::parse(&body) {
        Ok(s) => s,
        Err(r) => return refuse(&receiver, &r),
    };
    let job = sub.job;
    let resolve = state.resolve_profile.clone();
    let (role, requested) = (job.role_id.clone(), job.profile.clone());
    let resolution = match tokio::task::spawn_blocking(move || resolve(&role, requested.as_deref())).await {
        Ok(r) => r,
        Err(e) => ProfileResolution::Unresolved(format!("profile resolution did not finish: {e}")),
    };
    let profile = match darkmux_fleet::check_scope(&receiver, &admitted, &job, resolution) {
        Ok(p) => p,
        Err(r) => return refuse(&receiver, &r),
    };

    // One submitted job at a time; a second is answered "busy" at once.
    {
        let mut slot = state.busy.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(running) = slot.as_ref() {
            return refuse(&receiver, &Refusal::Busy { session_id: running.clone() });
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
    // A dedicated OS thread, not the async runtime: a dispatch blocks for
    // minutes. The guard moves in, so the slot frees when the work ends,
    // even if the sender stopped waiting.
    let spawned = std::thread::Builder::new().name("darkmux-fleet-job".into()).spawn(move || {
        let _guard = guard;
        let result = execute(job, run_profile);
        let _ = tx.send(result);
    });
    if let Err(e) = spawned {
        return refuse(&receiver, &Refusal::BadRequest(format!("could not start the job: {e}")));
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

/// Start the listener if `fleet.listener.enabled`. Never fails the daemon:
/// each reason it cannot start is logged, and `darkmux doctor` reports it.
/// Waits for the provider to come up (it may start after the daemon at
/// boot), retrying every 30 s.
pub(crate) fn spawn_if_enabled(shutdown: tokio::sync::watch::Receiver<bool>) {
    if !darkmux_types::config_access::fleet_listener_enabled() {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = run(shutdown).await {
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
    let addr = loop {
        let p = provider.clone();
        match tokio::task::spawn_blocking(move || p.local_node()).await {
            Ok(Ok(local)) => break listen_addr(&local, port)?,
            Ok(Err(e)) => eprintln!(
                "darkmux fleet: the {} network is not answering ({e:#}); retrying in 30s",
                provider.provider_name()
            ),
            Err(e) => eprintln!("darkmux fleet: identity check failed ({e}); retrying in 30s"),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            _ = shutdown.wait_for(|v| *v) => return Ok(()),
        }
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("binding {addr}: {e} (another process on `fleet.listener.port`?)"))?;
    println!("  fleet listener: {addr} (work submission; identity: {})", provider.provider_name());
    let app = router(FleetListenerState::production(receiver, provider));
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|v| *v).await;
        })
        .await
        .map_err(|e| format!("{e}"))
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
        let state = FleetListenerState {
            receiver: "studio".into(),
            provider: Arc::new(provider),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_role, requested| match requested {
                Some("utility") => ProfileResolution::UtilityOnly("utility".into()),
                Some(p) => ProfileResolution::Work(p.to_string()),
                None => ProfileResolution::Work("host".into()),
            }),
            execute: Arc::new(move |job: WorkJob, profile: String| {
                std::thread::sleep(Duration::from_millis(job_ms));
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
        };
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
                axum::serve(l, router(state).into_make_service_with_connect_info::<SocketAddr>())
                    .await
                    .unwrap();
            });
        });
        Harness { url: format!("http://127.0.0.1:{port}{}", darkmux_fleet::SUBMISSION_PATH), ran, busy }
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
        assert_eq!(reply.status, "completed");
        assert_eq!(reply.exit_code, Some(0));
        assert_eq!(reply.stdout.as_deref(), Some("ran radio-host on host"));
        assert_eq!(reply.profile.as_deref(), Some("host"));
        assert_eq!(h.ran.lock().unwrap().as_slice(), &[("s-ok".to_string(), "host".to_string())]);
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
        assert!(reply.reason.unwrap().contains("busy running s-long"));
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
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(move || {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allow())
            }),
            resolve_profile: Arc::new(|_, _| ProfileResolution::Work("host".into())),
            execute: Arc::new(|_, _| panic!("never runs")),
            busy: Arc::new(Mutex::new(None)),
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
            provider: Arc::new(StaticIdentityProvider {
                local: test_node("nSTUDIO", "studio", "100.64.0.2"),
                peers: vec![laptop()],
                down: None,
            }),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(|| Ok(allow())),
            resolve_profile: Arc::new(|_, _| ProfileResolution::Work("host".into())),
            execute: Arc::new(|_, _| panic!("never runs")),
            busy: Arc::new(Mutex::new(None)),
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
