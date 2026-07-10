//! `darkmux serve` — minimal HTTP daemon for flow record retrieval.

use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Path, Query, Request, State},
    http::StatusCode,
    middleware::{from_fn, Next},
    response::{IntoResponse, Response, sse::{Event, KeepAlive, Sse}},
    routing::get,
    Router,
};
use futures::stream::{self, Stream};
use std::collections::VecDeque;
use std::io::{BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// (#925) Per-route request timeout for the NON-streaming routes. Bounds a
/// slow/hung request; the long-lived `/flow/:date/stream` SSE route is
/// deliberately excluded.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// (#925) Max simultaneously-open SSE streams across the daemon, so a client
/// can't exhaust connections. Generous for a personal fleet's viewers.
const MAX_CONCURRENT_SSE: usize = 64;

/// (#925) Per-line byte cap for the day-file reader. A single flow record is
/// small; a line beyond this (a corrupt/adversarial flows-dir file with no
/// newline) is skipped rather than ballooned into one allocation.
const MAX_FLOW_LINE_BYTES: usize = 1 << 20; // 1 MiB

/// Application state shared across handlers.
#[derive(Clone)]
pub(crate) struct AppState {
    flows_dir: PathBuf,
    worktrees_base: PathBuf,
    /// (#925) Count of currently-open SSE streams; `flow_stream_handler`
    /// gates new streams on `MAX_CONCURRENT_SSE` and an `SseSlot` guard
    /// decrements this on disconnect.
    sse_open: Arc<AtomicUsize>,
    /// (#1247 Part 3) The lab observer lens's scan root — `--lab-dir` /
    /// `DARKMUX_LAB_DIR`. `None` when the daemon started without it: the
    /// `/lab/*` routes then answer `configured: false` / 404 rather than
    /// scanning anything. Deliberately NO default scan path — machine-local
    /// lab data is only read when the operator names the directory (operator
    /// sovereignty; see the lab-vs-fleet scope boundary project memory).
    lab_dir: Option<PathBuf>,
}

/// (#925) RAII slot for one open SSE stream: decrements `AppState::sse_open`
/// when dropped (the stream ends or the client disconnects). Carried inside
/// the SSE stream so its lifetime exactly tracks the connection's.
struct SseSlot(Arc<AtomicUsize>);

impl Drop for SseSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Validate a path segment as a bare date string (`YYYY-MM-DD`).
fn is_valid_date(date: &str) -> Option<&str> {
    if date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date[..4].chars().all(|c| c.is_ascii_digit())
        && date[5..7].chars().all(|c| c.is_ascii_digit())
        && date[8..].chars().all(|c| c.is_ascii_digit())
    {
        Some(date)
    } else {
        None
    }
}

/// Embedded HTML for the observability viewer. Shared between the live
/// route (`GET /`) and the playback route (`GET /play/:date`); each handler
/// injects a `<meta name="darkmux-mode">` tag the viewer's boot() reads to
/// decide whether to start the SSE tail (live) or skip it (playback).
///
/// Lives at `crates/darkmux-serve/assets/viewer.html` (not under `docs/`)
/// specifically so GitHub Pages doesn't also serve it at `darkmux.com/viewer/`.
/// `darkmux.com` is reserved for the demo (`/demo`) + marketing surface; the
/// live and playback viewers only exist where there's a daemon to talk to
/// (operator's localhost or tailnet). See #624.
const VIEWER_HTML: &str = include_str!("../assets/viewer.html");

/// Inject a `<meta name="darkmux-mode">` tag right after `<head>` so the
/// viewer's `boot()` can read it before any data-fetching logic runs.
/// Optionally injects a `<meta name="darkmux-date">` to pin playback to a
/// specific UTC date independent of the browser's URL.
///
/// (#1129) Always injects the build identifier (`darkmux-version` =
/// `build_version()`, with the git SHA) and the flow-schema version
/// (`darkmux-flow-schema`) so the viewer header can show WHICH build is
/// running + the data contract it renders. Values are darkmux-controlled
/// (`x.y.z (sha✱)` / `x.y.z`) — no quote/angle chars — so no escaping needed.
fn inject_mode_meta(html: &str, mode: &str, date: Option<&str>) -> String {
    let mut meta = format!(r#"<meta name="darkmux-mode" content="{}">"#, mode);
    if let Some(d) = date {
        meta.push_str(&format!(r#"
<meta name="darkmux-date" content="{}">"#, d));
    }
    meta.push_str(&format!(
        r#"
<meta name="darkmux-version" content="{}">
<meta name="darkmux-flow-schema" content="{}">"#,
        darkmux_types::build_version(),
        darkmux_flow::FLOW_SCHEMA_VERSION,
    ));
    // <head> appears exactly once; the replacement is unambiguous.
    html.replacen("<head>", &format!("<head>\n{}", meta), 1)
}

/// Serve the LIVE observability viewer at `GET /` (#554, #624 follow-up).
///
/// Because the daemon and the viewer share an origin, the viewer's
/// same-origin `fetch('/flow/:date')` calls are CORS- and
/// mixed-content-free **by construction** — that's the structural fix
/// behind #554, not a CORS allowlist. The HTML is embedded via
/// `include_str!` so the daemon binary is self-contained (no asset dir to
/// ship).
///
/// Live mode tails today's records via SSE on `/flow/:date/stream`. For
/// scrubbing through a past day without live records bleeding in, use the
/// playback route `GET /play/:date` instead.
///
/// darkmux.com/demo is THIS viewer in playback mode, generated from
/// `assets/viewer.html` by `scripts/build-demo.sh` and fed a committed flow
/// file (`docs/demo/demo-flow.jsonl`) — identical to a local `/play`, not a
/// separate fork.
async fn root_html() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        inject_mode_meta(VIEWER_HTML, "live", None),
    )
}

/// Serve the PLAYBACK observability viewer at `GET /play/:date`. Same HTML
/// as the live route, but the injected mode meta tells the viewer's boot()
/// to skip `startLiveTail()` and stay in scrubber-replay mode for the
/// captured day. Operators inspecting a past day get a deterministic
/// playback view that isn't disrupted by today's incoming records.
///
/// Returns 404 for malformed dates. The viewer expects UTC `YYYY-MM-DD`.
async fn play_html(Path(date): Path<String>) -> impl IntoResponse {
    match is_valid_date(&date) {
        Some(valid) => (
            StatusCode::OK,
            [("content-type", "text/html; charset=utf-8")],
            inject_mode_meta(VIEWER_HTML, "play", Some(valid)),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [("content-type", "text/plain; charset=utf-8")],
            "darkmux serve: expected /play/YYYY-MM-DD\n",
        )
            .into_response(),
    }
}

/// Build the HTTP router with a configurable flows directory. Test-only
/// convenience wrapper (production's `run()` calls `build_router_full`
/// directly so it can thread `lab_dir`) — no lab observer root
/// (`lab_dir: None`); tests that need `/lab/*` go through `build_router_full`.
#[cfg(test)]
pub(crate) fn build_router(flows_dir: PathBuf) -> Router {
    build_router_with_worktrees_base(flows_dir, worktrees_base_dir())
}

/// Like `build_router` but accepts a custom worktrees base directory.
/// Delegates to `build_router_full` with `lab_dir: None`. Test-only, same
/// reason as `build_router` above.
#[cfg(test)]
pub(crate) fn build_router_with_worktrees_base(
    flows_dir: PathBuf,
    worktrees_base: PathBuf,
) -> Router {
    build_router_full(flows_dir, worktrees_base, None)
}

/// Full router builder — flows dir + worktrees base + the lab observer's
/// scan root. This is the SINGLE place routes are registered (#881 collapsed
/// the prior duplicate builder; #1247 Part 3 added the `/lab/*` group here
/// rather than a parallel builder), so the auth layers below land in exactly
/// one place.
///
/// **Auth wiring (#881):** when a bearer token is configured
/// (`darkmux_flow::serve_token_present()`), two gates are added — otherwise the
/// router is byte-for-byte today's behavior (zero friction for the loopback-only
/// default install):
///   - `/diff/:session_id` gets an **always-on** token gate (`route_layer`), so
///     the most sensitive endpoint (live in-flight source) requires the token
///     even on loopback.
///   - a **remote-only** gate (`auth_mw`) wraps the whole router: loopback peers
///     pass (the operator's own machine + the bundled viewer keep working),
///     non-loopback peers must present the token. `/health` is always exempt
///     (doctor's reachability probe). Layered INNER of CORS so preflight is
///     handled by the CORS layer first.
///
/// **`/lab/*` auth (#1247 Part 3):** the lab routes ride the SAME remote-only
/// gate as everything but `/diff` — read-only local-run artifacts are no more
/// sensitive than flow records, and the lab lens is machine-local by design
/// (never federated), so there's no reason to single them out for the
/// always-on `/diff`-style gate.
pub(crate) fn build_router_full(
    flows_dir: PathBuf,
    worktrees_base: PathBuf,
    lab_dir: Option<PathBuf>,
) -> Router {
    let state = AppState {
        flows_dir,
        worktrees_base,
        sse_open: Arc::new(AtomicUsize::new(0)),
        lab_dir,
    };
    let auth_on = darkmux_flow::serve_token_present();

    // /diff is gated unconditionally when a token exists (even on loopback).
    let diff_route = if auth_on {
        get(diff_handler).route_layer(from_fn(diff_auth_mw))
    } else {
        get(diff_handler)
    };

    // (#925) Keep the long-lived SSE stream route SEPARATE so the per-route
    // request timeout below never applies to it (it's meant to stay open).
    let streaming = Router::new().route("/flow/:date/stream", get(flow_stream_handler));

    // Every other (non-streaming) route gets a request timeout, bounding a
    // slow/hung request.
    let timed = Router::new()
        .route("/", get(root_html))
        .route("/play/:date", get(play_html))
        .route("/health", get(health))
        .route("/flow/:date", get(flow_handler))
        .route("/flow-days", get(flow_days_handler))
        .route("/flow-missions", get(flow_missions_handler))
        .route("/flow-mission/:id", get(flow_mission_handler))
        .route("/flow-session/:id", get(flow_session_handler))
        .route("/flow-status", get(flow_status_handler))
        .route("/model/status", get(model_status_handler))
        .route("/machine/specs", get(machine_specs_handler))
        .route("/machine/memory", get(machine_memory_handler))
        .route("/missions", get(missions_handler))
        .route("/sprints", get(sprints_handler))
        .route("/fleet/sessions/live", get(fleet_sessions_live_handler))
        .route("/fleet/machines/live", get(fleet_machines_live_handler))
        .route("/lab/runs", get(lab_runs_handler))
        .route("/lab/run/detail", get(lab_run_detail_handler))
        .route("/lab/run/events", get(lab_run_events_handler))
        .route("/diff/:session_id", diff_route)
        .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(
            REQUEST_TIMEOUT_SECS,
        )));

    let mut router = timed.merge(streaming);

    // Remote-only gate, added BEFORE the CORS layer so CORS ends up outermost
    // (handles preflight + sets headers first); the gate runs just inside it.
    if auth_on {
        router = router.layer(from_fn(auth_mw));
    }

    router.layer(local_only_cors()).with_state(state)
}

/// (#881) Build a `401 Unauthorized` with a `WWW-Authenticate: Bearer` hint.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized: darkmux serve requires a bearer token (Authorization: Bearer <token>)\n",
    )
        .into_response()
}

/// (#881) Constant-time-ish equality (length-independent body) for comparing a
/// presented token against the configured one — avoids a trivial early-return
/// timing oracle. Hand-rolled to keep the dep set tiny (no `subtle`/`constant_time_eq`).
fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in presented.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// (#881) Whether the request carries a valid `Authorization: Bearer <token>`
/// matching the configured serve token. `false` when no token is configured
/// (the caller only invokes this on the auth-on path, but failing closed is the
/// safe default).
fn request_token_ok(headers: &axum::http::HeaderMap) -> bool {
    let Some(token) = darkmux_flow::serve_token() else {
        return false;
    };
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    // RFC 6750: the auth scheme is case-insensitive (`Bearer`/`bearer`); split
    // on the first whitespace so extra spacing before the token is tolerated.
    let Some((scheme, presented)) = value.split_once(char::is_whitespace) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    tokens_match(presented.trim().as_bytes(), token.expose_for_compare().as_bytes())
}

/// (#881) Always-on token gate for `/diff/:session_id` — the most sensitive
/// endpoint (live in-flight agent-produced source), gated even on loopback.
async fn diff_auth_mw(req: Request, next: Next) -> Response {
    if request_token_ok(req.headers()) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// (#881) Remote-only token gate for the rest of the surface. Loopback peers
/// pass (the operator's own machine + the bundled same-origin viewer keep
/// working with zero friction); non-loopback peers must present the token.
/// `/health` is always exempt so doctor's reachability probe (and external
/// liveness checks) keep working. A missing `ConnectInfo` (the `oneshot` test
/// path, which has no connection) is treated as loopback.
///
/// **Trust assumption:** "loopback is trusted" holds for darkmux's deployment —
/// bound directly (no reverse proxy) over a Tailscale tailnet that preserves the
/// real peer IP. Behind a connection-terminating reverse proxy every peer would
/// appear loopback and this gate would be bypassed (`/diff` would still be
/// gated — it ignores `ConnectInfo`). If darkmux ever grows a
/// multi-tenant/shared-host or behind-proxy mode, revisit this exemption.
async fn auth_mw(req: Request, next: Next) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let is_remote = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| !ci.0.ip().is_loopback())
        .unwrap_or(false);
    if !is_remote || request_token_ok(req.headers()) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// (#881) Refuse a non-loopback bind unless a token is configured. Pure +
/// testable: parse `bind` to an `IpAddr`; a loopback address (127.0.0.0/8, ::1)
/// is always allowed; any other parsed address (a LAN/Tailnet IP, or `0.0.0.0`)
/// requires `token_present`. A bind string that doesn't parse as an IP is
/// treated conservatively as non-loopback (so a typo can't sneak past the gate).
fn bind_requires_token(bind: &str, token_present: bool) -> Result<(), String> {
    let is_loopback = bind
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    if is_loopback || token_present {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind the serve daemon to a non-loopback address ({bind}) without a token \
configured — the daemon would expose flow records, machine specs, mission state, and the live \
git diff of in-flight worktrees to any reachable peer, unauthenticated.\n\
  Fix: set a token — `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` (macOS) \
plus `daemon_auth_enabled: true` in ~/.darkmux/config.json, OR export DARKMUX_SERVE_TOKEN=… — then \
re-run. Or bind to 127.0.0.1 (the default) for a loopback-only daemon."
        ))
    }
}

/// GET /fleet/machines/live — the machines present in the fleet RIGHT NOW
/// (#638): every unexpired `darkmux:presence:<uid>` beat, fleet-wide across
/// the shared Redis. **Presence is the source of truth for "who's in the
/// fleet" — independent of whether a machine has ever emitted a flow
/// record.** A freshly-synced, idle machine broadcasting its heartbeat shows
/// here with zero dispatch activity; the live viewer unions this with the
/// record-derived machines so a present machine appears (and a machine that
/// stops beating drops off) regardless of work.
///
/// Returns `[]` when `DARKMUX_REDIS_URL` is unset or unreachable — a
/// single-machine, file-only fleet has no shared presence substrate, so the
/// viewer falls back to record-derived machines. Never 500s on a Redis blip.
async fn fleet_machines_live_handler() -> impl IntoResponse {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let redis_url = darkmux_flow::redis_url();
    let beats: Vec<darkmux_flow::presence::PresenceBeat> = match redis_url {
        Some(url) => tokio::task::spawn_blocking(move || {
            redis::Client::open(url.expose_for_probe())
                .ok()
                .and_then(|c| darkmux_flow::presence::read_live(&c).ok())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default(),
        None => Vec::new(),
    };
    axum::Json(beats)
}

/// GET /fleet/sessions/live — the sessions with a live heartbeat right now
/// (#638). Each running dispatch refreshes a short-TTL
/// `darkmux:session-presence:<sid>` Redis key; this returns every unexpired
/// one (fleet-wide, across machines on the shared Redis). The live viewer
/// keys "running" on membership here, so a crashed / killed / timed-out
/// dispatch ages out of the live set instead of showing "running" forever
/// for want of a `dispatch.complete` record.
///
/// Returns `[]` when `DARKMUX_REDIS_URL` is unset or unreachable —
/// single-machine, file-only fleets have no live-session substrate, so the
/// viewer shows terminal status only. Never 500s on a Redis blip (the live
/// signal is best-effort; degraded == "nothing live", not an error).
async fn fleet_sessions_live_handler() -> impl IntoResponse {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let redis_url = darkmux_flow::redis_url();
    let beats: Vec<darkmux_flow::session_presence::SessionBeat> = match redis_url {
        Some(url) => tokio::task::spawn_blocking(move || {
            redis::Client::open(url.expose_for_probe())
                .ok()
                .and_then(|c| darkmux_flow::session_presence::read_live_sessions(&c).ok())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default(),
        None => Vec::new(),
    };
    axum::Json(beats)
}

/// CORS layer for the daemon's browser-facing endpoints. **Default**:
/// `null` only (i.e. `file://` pages — the bundled topology + flow
/// viewers run from disk). **Operator override**: set
/// `DARKMUX_DAEMON_CORS_ORIGINS=<comma-list>` to extend the allowlist
/// with specific origins (e.g. `http://localhost:5173` for a Vite dev
/// server). Each entry is matched **exactly** — no prefix wildcard.
///
/// The tightening from "any localhost origin" (#225) to "null only by
/// default" (#273) follows from #270 elevating the daemon's data
/// surface to fleet-wide. A compromised tab at any localhost origin
/// would otherwise be able to exfiltrate fleet activity via CORS;
/// requiring an explicit opt-in matches operator-sovereignty (default
/// is the safer choice; operator names the dev-server origins they
/// actually run).
///
/// Non-browser clients (curl, darkmux CLI, probe) are unaffected —
/// CORS is browser-enforced; the header simply isn't set for unmatched
/// origins and the response is returned normally.
fn local_only_cors() -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderValue, Method};
    use tower_http::cors::AllowOrigin;

    // env(DARKMUX_DAEMON_CORS_ORIGINS) > config.runtime.daemon_cors_origins >
    // "" (#661 Slice 4).
    let extra_origins = parse_cors_origins(
        &darkmux_types::config_access::daemon_cors_origins().unwrap_or_default(),
    );

    tower_http::cors::CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, _parts: &axum::http::request::Parts| {
                let s = origin.to_str().unwrap_or("");
                if s == "null" {
                    return true;
                }
                // Normalize the incoming Origin so trailing-slash /
                // uppercase mismatches between what the browser sends
                // and what the operator typed in their env var don't
                // silently deny. Browsers DO emit canonical Origin
                // (lowercase scheme+host, no trailing slash) but
                // belt-and-suspenders: normalize both sides.
                let normalized_incoming = normalize_cors_origin(s);
                extra_origins
                    .iter()
                    .any(|allowed| allowed == &normalized_incoming)
            },
        ))
        .allow_methods([Method::GET])
}

/// Parse `DARKMUX_DAEMON_CORS_ORIGINS` env value into a normalized
/// allowlist (#288). Each entry is trimmed, normalized via
/// `normalize_cors_origin` (lowercase scheme + host, strip trailing
/// slash), and filtered for emptiness.
///
/// **What's rejected**: only the literal `*` (with a stderr warning
/// pointing operators at a real fix). Other wildcard-shaped patterns
/// (`**`, `https://*.example.com`) and oddities (`null`) are kept
/// as-is + normalized; they pass through as exact-match strings.
/// Subdomain-wildcard patterns silently never match anything (no
/// browser sends `*.example.com` as Origin). `null` is the one
/// special case the predicate ALREADY allows unconditionally (it's
/// the file:// origin); adding it to the env list is harmless-but-
/// redundant.
///
/// Shared by `local_only_cors` (predicate construction) and
/// `build_startup_banner` (operator-visible allowlist report).
pub(crate) fn parse_cors_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            if entry == "*" {
                eprintln!(
                    "darkmux serve: DARKMUX_DAEMON_CORS_ORIGINS contains `*` — \
                     CORS does not wildcard via a literal `*` Origin header value; \
                     this entry is being ignored. Use specific origins like \
                     `http://localhost:5173` instead."
                );
                return None;
            }
            Some(normalize_cors_origin(entry))
        })
        .collect()
}

/// Normalize a CORS origin string: lowercase the scheme + host (RFC
/// 6454 origins are case-insensitive for scheme + host), strip any
/// trailing slash (browsers don't send one; operator's env value
/// might). Path / query / fragment have no meaning in an Origin
/// header — caller has already filtered the input to the env-var
/// shape, not a full URL. (#288)
///
/// **Constraints (operator-side input contract):**
/// - RFC 6454 origins are `scheme + host + optional port` only. No
///   userinfo / path / query / fragment. An entry containing those
///   IS operator error — the function does NOT special-case them;
///   userinfo will be destructively lowercased (e.g.
///   `http://User:Pass@host` → `http://user:pass@host`) which is
///   harmless for case-insensitive fields and meaningless for an
///   Origin header that should not have userinfo.
/// - Default-port equivalence is NOT collapsed. `http://localhost`
///   and `http://localhost:80` are spec-equivalent but kept distinct
///   here — operators using non-dev ports must type the port
///   consistently on both sides of the env / browser. Dev servers
///   (Vite 5173, Next 3000, etc.) never hit this edge.
pub(crate) fn normalize_cors_origin(s: &str) -> String {
    let trimmed = s.trim().trim_end_matches('/');
    // Lowercase the scheme + authority. Origins don't have paths in
    // the proper sense; if the operator typed `http://Foo:5173`, this
    // produces `http://foo:5173`. Conservative split-on-`://` so a
    // malformed entry without scheme isn't mangled.
    match trimmed.split_once("://") {
        Some((scheme, rest)) => format!("{}://{}", scheme.to_lowercase(), rest.to_lowercase()),
        None => trimmed.to_lowercase(),
    }
}

// The daemon reachability probe + the every-dispatch nudge moved to
// `darkmux_flow::daemon_probe` in #463's cycle-break (so `crew` can fire the
// nudge without depending on `serve`). Re-exported here so existing
// `crate::serve::{DEFAULT_DAEMON_ADDR, nudge_if_daemon_unreachable, ...}`
// paths keep resolving unchanged.
pub use darkmux_flow::daemon_probe::{
    nudge_if_daemon_unreachable, DEFAULT_DAEMON_ADDR, DEFAULT_DAEMON_PORT,
};

/// Grace period (seconds) between receiving a shutdown signal and
/// force-exiting the process. SSE streams hold connections open
/// indefinitely; axum's graceful shutdown would otherwise block forever
/// waiting for them to drain.
pub(crate) const SHUTDOWN_GRACE_SECS: u64 = 3;

// Compile-time bounds: drift outside this range is the painful state
// #121 fixed (operator hammering Ctrl-C / killing PID by hand at the
// long end; killing clean disconnects mid-flight at the short end).
// Build fails here if a future change pushes the const out of range.
const _: () = assert!(
    SHUTDOWN_GRACE_SECS <= 5,
    "SHUTDOWN_GRACE_SECS too long — operators will fall back to kill <pid>"
);
const _: () = assert!(
    SHUTDOWN_GRACE_SECS >= 1,
    "SHUTDOWN_GRACE_SECS too short — clean disconnects deserve a beat"
);

/// Build the lines of the startup banner. Factored out so tests can
/// assert content without spawning the daemon.
///
/// `mission_count` and `sprint_count` are loaded via
/// `darkmux_crew::loader::load_missions`/`load_sprints` in production;
/// tests pass synthetic counts.
#[allow(clippy::too_many_arguments)]
fn build_startup_banner(
    addr: &std::net::SocketAddr,
    flows_dir: &std::path::Path,
    flows_dir_exists: bool,
    missions_dir: &std::path::Path,
    missions_dir_exists: bool,
    sprints_dir: &std::path::Path,
    sprints_dir_exists: bool,
    mission_count: usize,
    sprint_count: usize,
    lab_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let version = env!("CARGO_PKG_VERSION");
    let flow_schema = darkmux_flow::FLOW_SCHEMA_VERSION;

    lines.push(format!("darkmux serve · v{version}"));
    lines.push(format!(
        "  viewer:   {}",
        darkmux_types::style::accent(&format!("http://{addr}/"))
    ));
    lines.push(format!("  flow schema:    {}", darkmux_types::style::dim(flow_schema)));
    lines.push(format!(
        "  flows dir:      {}",
        darkmux_types::style::dim(&flows_dir.display().to_string())
    ));
    lines.push(format!(
        "  missions:       {} loaded",
        darkmux_types::style::dim(&mission_count.to_string())
    ));
    lines.push(format!(
        "  sprints:        {} loaded",
        darkmux_types::style::dim(&sprint_count.to_string())
    ));

    // CORS allowlist surface (#273) — operators with a localhost dev
    // server origin need to opt in via DARKMUX_DAEMON_CORS_ORIGINS;
    // surface the resolved state so failure-to-connect is debuggable.
    // Routed through `parse_cors_origins` (#288) so the banner shows
    // the same normalized form the predicate uses — operators who type
    // `http://Foo:5173/` see `http://foo:5173` in the banner and can
    // verify the normalization landed.
    // (#875) Banner must reflect the SAME allowlist source the router uses —
    // env > config.runtime.daemon_cors_origins via config_access, not raw env
    // (else a config-only allowlist shows as "null" in the banner).
    let extra_list = parse_cors_origins(
        &darkmux_types::config_access::daemon_cors_origins().unwrap_or_default(),
    );
    if extra_list.is_empty() {
        lines.push(
            "  cors allowlist: null (file://) only — set DARKMUX_DAEMON_CORS_ORIGINS to opt in"
                .to_string(),
        );
    } else {
        // Put the normalization hint on the populated branch where it
        // matters — operators who SEE entries are the ones who might
        // wonder why their typed env value looks different here.
        lines.push(format!(
            "  cors allowlist: null (file://) + {} (exact-match, normalized lowercase + no trailing slash)",
            darkmux_types::style::dim(&extra_list.join(", "))
        ));
    }

    // (#881) Auth-state line — so the operator can see at a glance whether the
    // bearer gate is active and (when bound non-loopback) that a token is set.
    let token_present = darkmux_flow::serve_token_present();
    let non_loopback = !addr.ip().is_loopback();
    if token_present {
        lines.push(format!(
            "  auth:           {} (remote reads + /diff require a bearer token; loopback open)",
            darkmux_types::style::success("token set")
        ));
    } else if non_loopback {
        // Should be unreachable — `bind_requires_token` refuses this combination
        // before bind — but surface it loudly if the bind path ever changes.
        lines.push(darkmux_types::style::warn(
            "  ! auth:         NO token set but bound non-loopback — the read surface is UNAUTHENTICATED",
        ));
    } else {
        lines.push(
            "  auth:           none (loopback-only; set DARKMUX_SERVE_TOKEN or daemon_auth_enabled + Keychain to bind non-loopback)"
                .to_string(),
        );
    }

    if !flows_dir_exists {
        lines.push(darkmux_types::style::warn(
            "  ! flows dir doesn't exist yet — will be created on first record write",
        ));
    }
    if !missions_dir_exists {
        lines.push(darkmux_types::style::warn(&format!(
            "  ! missions dir not found at {} (/missions endpoint returns empty)",
            missions_dir.display()
        )));
    }
    if !sprints_dir_exists {
        lines.push(darkmux_types::style::warn(&format!(
            "  ! sprints dir not found at {} (/sprints endpoint returns empty)",
            sprints_dir.display()
        )));
    }

    // (#1247 Part 3) Lab observer scan root — absent by design (no default
    // scanning of arbitrary paths; the operator names it via `--lab-dir` /
    // `DARKMUX_LAB_DIR`), so surface whichever state is true rather than
    // silently leaving the lab lens unexplained.
    match lab_dir {
        Some(dir) => lines.push(format!(
            "  lab dir:        {}",
            darkmux_types::style::dim(&dir.display().to_string())
        )),
        None => lines.push(
            "  lab dir:        none (pass --lab-dir <path> to enable the lab observer lens)"
                .to_string(),
        ),
    }

    lines.push(darkmux_types::style::success("  ready — Ctrl-C to stop"));
    lines
}

/// Start the HTTP daemon, binding on `bind:port`. Blocks until a
/// shutdown signal (SIGINT or SIGTERM) is received. After the signal,
/// axum gets `SHUTDOWN_GRACE_SECS` to drain in-flight connections
/// before the process force-exits — SSE streams to the viewer would
/// otherwise keep the daemon alive forever.
pub fn run(port: u16, bind: String, flows_dir: PathBuf, lab_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let app = build_router_full(flows_dir.clone(), worktrees_base_dir(), lab_dir.clone());
        let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
        // (#881) Refuse a non-loopback bind without a configured token BEFORE we
        // bind the socket — exposing the read surface unauthenticated is the
        // vulnerability this gate closes.
        bind_requires_token(&bind, darkmux_flow::serve_token_present())
            .map_err(anyhow::Error::msg)?;
        let listener = tokio::net::TcpListener::bind(addr).await?;

        // Banner: print after bind succeeds so we don't claim "listening"
        // before we actually are.
        let flows_dir_exists = flows_dir.exists();
        let missions_dir = darkmux_crew::loader::missions_dir();
        let missions_dir_exists = missions_dir.exists();
        // Print the ASCII-art wordmark + tagline only when stdout is a real
        // terminal (so piped / redirected logs stay clean).
        if std::io::stdout().is_terminal() {
            let art = r#"██████╗  █████╗ ██████╗ ██╗  ██╗███╗   ███╗██╗   ██╗██╗  ██╗
██╔══██╗██╔══██╗██╔══██╗██║ ██╔╝████╗ ████║██║   ██║╚██╗██╔╝
██║  ██║███████║██████╔╝█████╔╝ ██╔████╔██║██║   ██║ ╚███╔╝ 
██║  ██║██╔══██║██╔══██╗██╔═██╗ ██║╚██╔╝██║██║   ██║ ██╔██╗ 
██████╔╝██║  ██║██║  ██║██║  ██╗██║ ╚═╝ ██║╚██████╔╝██╔╝ ██╗
╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝"#;
            println!("{}", darkmux_types::style::accent(art));
            println!("  🤖 Mission control for your AI fleet.");
            println!();
        }

        // Print the startup panel (viewer URL, counts, warnings).
        let sprints_dir = darkmux_crew::loader::sprints_dir();
        let sprints_dir_exists = sprints_dir.exists();
        let mission_count = darkmux_crew::loader::load_missions()
            .map(|v| v.len())
            .unwrap_or(0);
        let sprint_count = darkmux_crew::loader::load_sprints()
            .map(|v| v.len())
            .unwrap_or(0);
        for line in build_startup_banner(
            &addr,
            &flows_dir,
            flows_dir_exists,
            &missions_dir,
            missions_dir_exists,
            &sprints_dir,
            sprints_dir_exists,
            mission_count,
            sprint_count,
            lab_dir.as_deref(),
        ) {
            println!("{line}");
        }

        // Spawn the fleet work-queue runner thread (#246 PR-C.2, renamed #595).
        // Runs on a dedicated std::thread (not a tokio task) so the sync
        // redis client + sync crew::dispatch::dispatch don't saturate
        // the tokio executor. The runner self-disables when its prerequisite
        // (DARKMUX_REDIS_URL) isn't declared — single-machine fleets
        // continue to work unchanged (#590: Redis presence is the
        // participation gate; tier declaration is no longer required).
        // The thread runs for the daemon's lifetime; the process
        // force-exit in the SHUTDOWN_GRACE_SECS path kills it cleanly.
        let _runner_handle = darkmux_fleet::spawn_runner_thread();

        // (#638) Spawn the fleet-presence heartbeat emitter. Same dedicated-
        // std::thread shape + DARKMUX_REDIS_URL self-disable as the runner
        // above (plus a stable-machine-uid gate, #640): while this daemon
        // runs, it refreshes a short-TTL `darkmux:presence:<uid>` key so the
        // live-only fleet view, the live-version skew check, and the roster
        // can key on "who's actually here now" instead of "who ever wrote a
        // record".
        let _presence_handle = darkmux_flow::presence::spawn_emitter_thread();

        // (#647) Presence edge-recording for playback. Self-emit this machine's
        // `machine.online` open-edge now (it's online), and spawn the reconciler
        // that records `machine.offline` close-edges when a peer's presence key
        // disappears (deduped fleet-wide via an atomic claim). Heartbeats stay
        // ephemeral; only these ~2-per-episode edges hit the durable stream, so
        // playback can replay machine present/absent intervals without the
        // heartbeat noise. Same DARKMUX_REDIS_URL self-disable as presence.
        darkmux_flow::presence_reconciler::emit_machine_online_edge();
        let _reconciler_handle = darkmux_flow::presence_reconciler::spawn_reconciler_thread();

        // Shutdown plumbing: multiplex one signal to two consumers
        // (axum's graceful shutdown future and the force-exit timer).
        // `watch::channel` is the right shape — both consumers wait_for
        // the same latch flip.
        let (shutdown_tx, mut shutdown_rx_axum) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
            eprintln!(
                "\ndarkmux serve: shutdown signal received, {SHUTDOWN_GRACE_SECS}s grace for in-flight connections"
            );
            // (#918) Force-exit watchdog on a real OS thread, NOT a tokio task.
            // A tokio task is cancelled the instant the runtime is dropped — and
            // the runtime is dropped as soon as graceful drain completes — so a
            // task-based watchdog can't guarantee exit if runtime teardown then
            // blocks on a wedged background thread (e.g. a Redis presence /
            // stream worker pointed at an unreachable endpoint). A std::thread
            // outlives the runtime and guarantees the process exits within the
            // grace window regardless of which thread is stuck. On a genuinely
            // clean shutdown the process exits first and this thread is reaped
            // with it, so the grace window is only ever spent when something is
            // actually wedged.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(SHUTDOWN_GRACE_SECS));
                eprintln!(
                    "{}",
                    darkmux_types::style::warn(
                        "darkmux serve: force exit (a background thread blocked clean teardown past the grace window)"
                    )
                );
                std::process::exit(0);
            });
        });

        let axum_shutdown = async move {
            let _ = shutdown_rx_axum.wait_for(|&v| v).await;
        };

        // (#881) SECURITY-LOAD-BEARING: `into_make_service_with_connect_info`
        // populates each request's `ConnectInfo<SocketAddr>` from the real TCP
        // peer — that's how `auth_mw` tells a remote peer (token required) from a
        // loopback one (exempt). If this is ever downgraded to a plain
        // `into_make_service()`, `auth_mw` sees no `ConnectInfo`, treats EVERY
        // peer as loopback, and the remote gate becomes a silent no-op. Do not
        // change without re-checking `auth_mw`. (`/diff` stays gated regardless,
        // as its `route_layer` doesn't consult `ConnectInfo`.)
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(axum_shutdown)
        .await?;

        println!("darkmux serve: clean shutdown");
        Ok::<_, anyhow::Error>(())
    })
}

/// GET /health — returns darkmux version + flow schema version.
async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "darkmux_version": env!("CARGO_PKG_VERSION"),
        "flow_schema_version": darkmux_flow::FLOW_SCHEMA_VERSION,
    }))
}

/// Resolve the base directory holding per-sprint worktrees:
/// `~/.darkmux/worktrees` (HOME-less fallback `/tmp/darkmux/worktrees`).
fn worktrees_base_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".darkmux").join("worktrees"))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/worktrees"))
}

/// Validate a base ref string: must match `^[A-Za-z0-9][A-Za-z0-9_/.-]*$`.
/// No leading dash (prevents git argument injection). Plain char iteration,
/// no regex crate.
pub fn validate_base_ref(base: &str) -> bool {
    if base.is_empty() {
        return false;
    }
    let mut chars = base.chars();
    // First char: must be alphanumeric (no leading dash).
    if !chars.clone().next().unwrap_or_default().is_ascii_alphanumeric() {
        return false;
    }
    // Rest: alphanumeric, underscore, slash, dot, hyphen.
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-'))
}

/// Validate that `worktree_path` is contained within `base_dir`.
/// Both paths are canonicalized first; missing paths return false.
pub fn worktree_contained(
    worktree_path: &StdPath,
    base_dir: &StdPath,
) -> bool {
    let canon_base = match std::fs::canonicalize(base_dir) {
        Ok(p) => p,
        Err(_) => return false, // base doesn't exist
    };
    let canon_wt = match std::fs::canonicalize(worktree_path) {
        Ok(p) => p,
        Err(_) => return false, // worktree doesn't exist
    };
    canon_wt.starts_with(&canon_base)
}

/// Resolve a session_id from flow records: find the LAST `mission.run.start`
/// record matching that session across the two lexicographically last
/// `.jsonl` day files, and extract payload.worktree/base/branch.
pub fn resolve_session(
    session_id: &str,
    flows_dir: &StdPath,
) -> Option<(String, String, String)> {
    // Read all .jsonl day files from flows_dir.
    let entries: Vec<PathBuf> = std::fs::read_dir(flows_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
        })
        .collect();

    // Take the two lexicographically last day files (covers midnight rollover).
    let mut last_two: Vec<PathBuf> = entries
        .iter()
        .filter_map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .filter(|name| is_valid_date(name).is_some())
                .map(|_| p.clone())
        })
        .collect();
    last_two.sort_by(|a, b| {
        let a_name = a.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let b_name = b.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        a_name.cmp(b_name)
    });

    let candidates: Vec<&StdPath> = last_two.iter().rev().take(2).map(|p| p.as_path()).collect();

    // Scan all lines from those files, find the LAST matching record.
    let mut best: Option<(String, String, String)> = None;

    for day_file in &candidates {
        let Ok(file) = std::fs::File::open(day_file) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let action = record.get("action").and_then(|a| a.as_str()).unwrap_or("");
            if action != "mission.run.start" {
                continue;
            }
            let sid = record
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if sid != session_id {
                continue;
            }
            let payload = record.get("payload");
            if let Some(p) = payload {
                let wt = p.get("worktree").and_then(|w| w.as_str()).unwrap_or("");
                let base = p.get("base").and_then(|b| b.as_str()).unwrap_or("");
                let branch = p.get("branch").and_then(|b| b.as_str()).unwrap_or("");
                if !wt.is_empty() && !base.is_empty() {
                    best = Some((wt.to_string(), base.to_string(), branch.to_string()));
                }
            }
        }
    }

    best
}

/// Compute the git diff for a worktree against its base ref.
pub fn compute_git_diff(
    worktree: &StdPath,
    base: &str,
) -> Result<(String, Vec<DiffFile>, Vec<String>, bool), String> {
    // Hard cap on the returned diff text. Applied by STREAMING git's stdout
    // (below) so a pathological diff never fully materializes in RAM, then
    // again as a char-boundary truncation on the returned string.
    const DIFF_TEXT_CAP: usize = 262_144;

    // 1) unified diff text — stream stdout with the cap instead of buffering
    //    all of it (a stray node_modules / regenerated lockfile / huge binary
    //    change would otherwise buffer git's entire output before the cap
    //    discards it). Read at most DIFF_TEXT_CAP+1 bytes; the +1 detects
    //    that there was more, so the result is marked truncated.
    let mut child = Command::new("git")
        .current_dir(worktree)
        .args(["diff", "--no-color", base])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| "git error".to_string())?;
    let mut diff_bytes: Vec<u8> = Vec::new();
    let read_result = if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        out.by_ref()
            .take(DIFF_TEXT_CAP as u64 + 1)
            .read_to_end(&mut diff_bytes)
            .map(|_| ())
            .map_err(|_| "git error".to_string())
    } else {
        Ok(())
    };
    // Stop + reap git BEFORE propagating a read error, so an I/O failure never
    // leaks the child. If the diff exceeded the cap we stopped reading, so git
    // may still be writing into a now-full pipe; `git diff` is read-only (no
    // index lock), so killing it is safe.
    let _ = child.kill();
    let _ = child.wait();
    read_result?;
    let diff_text = String::from_utf8_lossy(&diff_bytes).to_string();

    // 2) numstat (additions/deletions per file)
    let numstat_out = Command::new("git")
        .current_dir(worktree)
        .args(["diff", "--numstat", base])
        .output()
        .map_err(|_| "git error".to_string())?;
    let numstat_text = String::from_utf8_lossy(&numstat_out.stdout);

    // 3) untracked files
    let ls_out = Command::new("git")
        .current_dir(worktree)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .map_err(|_| "git error".to_string())?;
    let untracked: Vec<String> = String::from_utf8_lossy(&ls_out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .take(200)
        .map(|s| s.to_string())
        .collect();

    // Parse numstat lines.
    let mut files: Vec<DiffFile> = Vec::new();
    for line in numstat_text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let additions = parse_numstat_count(parts[0]);
        let deletions = parse_numstat_count(parts[1]);
        let path = parts[2].to_string();
        files.push(DiffFile {
            path,
            additions,
            deletions,
        });
    }

    // Cap diff text at DIFF_TEXT_CAP bytes (char boundary).
    let mut truncated = false;
    let diff_text = if diff_text.len() > DIFF_TEXT_CAP {
        // Truncate at a char boundary.
        let mut end = DIFF_TEXT_CAP;
        while end > 0 && !diff_text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            truncated = true;
            diff_text.clone()
        } else {
            truncated = true;
            diff_text[..end].to_string()
        }
    } else {
        diff_text
    };

    Ok((diff_text, files, untracked, truncated))
}

/// Parse a numstat count field: "-" means binary (null), otherwise parse as u64.
fn parse_numstat_count(s: &str) -> Option<u64> {
    if s == "-" {
        None
    } else {
        s.parse::<u64>().ok()
    }
}

/// Diff response struct.
#[derive(serde::Serialize)]
pub struct DiffResponse {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<DiffFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub untracked: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Per-file diff stats.
#[derive(serde::Serialize)]
pub struct DiffFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
}

/// GET /diff/:session_id — returns the running git diff of a mission-run worktree.
pub(crate) async fn diff_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    // 1. Resolve session from flow records.
    let flows_dir = state.flows_dir.clone();
    let worktrees_base = state.worktrees_base.clone();
    // Offload the blocking session resolution (fs read_dir + line scans) to
    // the blocking pool so it never occupies an async worker thread.
    let resolved = tokio::task::spawn_blocking({
        let session_id = session_id.clone();
        let flows_dir = flows_dir.clone();
        move || resolve_session(&session_id, &flows_dir)
    })
    .await
    .ok()
    .flatten();
    let (worktree, base, branch) = match resolved {
        Some((wt, b, br)) => (wt, b, br),
        None => {
            return axum::Json(DiffResponse {
                available: false,
                session_id: None,
                base: None,
                branch: None,
                files: None,
                untracked: None,
                diff: None,
                truncated: None,
                reason: Some("no mission-run record for session".to_string()),
            })
            .into_response();
        }
    };

    // 2. Security: containment check.
    if !worktree_contained(StdPath::new(&worktree), &worktrees_base) {
        return axum::Json(DiffResponse {
            available: false,
            session_id: None,
            base: None,
            branch: None,
            files: None,
            untracked: None,
            diff: None,
            truncated: None,
            reason: Some("worktree outside managed base".to_string()),
        })
        .into_response();
    }

    // 3. Security: base ref validation.
    if !validate_base_ref(&base) {
        return axum::Json(DiffResponse {
            available: false,
            session_id: None,
            base: None,
            branch: None,
            files: None,
            untracked: None,
            diff: None,
            truncated: None,
            reason: Some("invalid base ref".to_string()),
        })
        .into_response();
    }

    // 4. Compute diff — three git subprocesses + file scans, offloaded to the
    // blocking pool so a slow/hung git can't starve the async runtime.
    let diff_result = tokio::task::spawn_blocking({
        let worktree = worktree.clone();
        let base = base.clone();
        move || compute_git_diff(StdPath::new(&worktree), &base)
    })
    .await
    .unwrap_or_else(|_| Err("diff task panicked".to_string()));
    let (diff_text, files, untracked, truncated) = match diff_result {
        Ok(r) => r,
        Err(reason) => {
            return axum::Json(DiffResponse {
                available: false,
                session_id: None,
                base: None,
                branch: None,
                files: None,
                untracked: None,
                diff: None,
                truncated: None,
                reason: Some(reason),
            })
            .into_response();
        }
    };

    axum::Json(DiffResponse {
        available: true,
        session_id: Some(session_id),
        base: Some(base),
        branch: Some(branch),
        files: Some(files),
        untracked: Some(untracked),
        diff: Some(diff_text),
        truncated: Some(truncated),
        reason: None,
    })
    .into_response()
}

/// GET /model/status — returns currently-loaded models (per `lms ps --json`)
/// as JSON so the flow viewer's toolbar pill / modal can render them
/// without parsing `lms` output client-side. See issue #87 for the
/// operator-facing motivation.
///
/// Always returns 200 with a structured body. `lms_unreachable: true`
/// signals the binary couldn't be invoked (operator hasn't installed
/// LMStudio's CLI, or it's not on PATH) — UI surfaces this as a
/// degraded-state pill rather than treating it as a hard error.
///
/// `lms::list_loaded()` is sync (subprocess invocation), so it runs on
/// the blocking pool to keep the axum executor free.
/// GET /missions — list of all missions from the JSON source-of-truth
/// (`~/.darkmux/crew/missions/`). Includes status + transition timestamps
/// (started_ts/closed_ts/paused_ts) so the viewer can render wall-clock
/// durations and the sprint-progress widget. Empty array on no missions
/// or unreachable crew root; never errors.
async fn missions_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(darkmux_crew::loader::load_missions).await;
    let missions = match result {
        Ok(Ok(m)) => m,
        _ => Vec::new(),
    };
    axum::Json(serde_json::json!({
        "missions": missions,
        "generated_at_ms": current_millis(),
    }))
}

/// GET /sprints — list of all sprints from the JSON source-of-truth
/// (`~/.darkmux/crew/sprints/`). Includes status + transition timestamps
/// (started_ts/completed_ts/abandoned_ts) so the viewer's wall-clock
/// graphic can render Running sprints' live elapsed time + Complete
/// sprints' frozen durations. Empty array on no sprints; never errors.
async fn sprints_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(darkmux_crew::loader::load_sprints).await;
    let sprints = match result {
        Ok(Ok(s)) => s,
        _ => Vec::new(),
    };
    axum::Json(serde_json::json!({
        "sprints": sprints,
        "generated_at_ms": current_millis(),
    }))
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── Lab observer lens (#1247 Part 3) ──────────────────────────────────────
//
// "Two doors, one viewer, distinct questions" (operator direction, #1247):
// these routes read ONLY `AppState::lab_dir` — the operator-named scan root
// passed via `--lab-dir` / `DARKMUX_LAB_DIR` — and never touch the flow
// stream, Redis, or any other machine's data. Machine-local by construction;
// no federation, ever. A "run" is any directory directly containing
// `funnels.json`, `funnel-events.jsonl`, or `scores.json` (the artifacts
// `review-bench --funnel` writes per-run-local, #1247 Parts 1-2, plus
// `scores.json` from any other bench mode). The scan is depth-bounded, and a
// matched run's own `cases/`/`worktrees/` subtrees (full repo checkouts) are
// never walked into (`LAB_SCAN_SKIP_DIRS` below) — but a match does NOT stop
// the scan from continuing into a matched dir's OTHER subdirectories, so a
// stray marker file left at any level (e.g. an ad-hoc `--scores-out`) can
// never swallow the runs nested beneath it.

/// Bounded recursion depth for the `/lab/runs` scan, counted from `lab_dir`
/// itself (depth 0).
const LAB_SCAN_MAX_DEPTH: usize = 4;

/// Subdirectory names never recursed into, even within the depth budget — a
/// bench run's own artifacts (`worktrees/<case>` is a full git checkout;
/// `cases/` holds diff/label fixtures) rather than further run clusters,
/// plus common tooling noise that can sit beside a corpus directory. Belt
/// and suspenders alongside the depth cap: a checked-out repo tree can be
/// deep and wide even within 4 levels.
const LAB_SCAN_SKIP_DIRS: &[&str] =
    &["worktrees", "cases", ".git", "node_modules", "__pycache__", "target"];

/// Max bytes read from `funnel-events.jsonl` in one `/lab/run/events` poll —
/// bounds a first-poll backfill against a huge historical run. The endpoint's
/// contract is explicit backfill via `offset` (never a full-file read in one
/// response); the client re-polls for the remainder.
const MAX_LAB_EVENTS_READ_BYTES: usize = 2 << 20; // 2 MiB

/// Resolve + SECURITY-CHECK a `dir` query param against `lab_dir`. Returns
/// the joined, containment-verified path when `dir` resolves strictly inside
/// (or IS exactly) `lab_dir` — no absolute override, no `..` component, no
/// symlink escape; `None` otherwise. Every `/lab/*` endpoint that takes a
/// `dir` param MUST go through this before touching the filesystem — a
/// read-only route is still a path-traversal surface if unchecked.
///
/// `dir == ""` resolves to `lab_dir` ITSELF, not a rejection: a matched run
/// at `lab_dir`'s own root (an operator pointing `--lab-dir` directly at one
/// run's folder, or a stray marker file left there — see
/// `scan_lab_runs_a_stray_marker_at_lab_dir_root_does_not_hide_nested_runs`)
/// gets `""` as its `dir` from `build_lab_run_summary`'s `strip_prefix`, and
/// that identifier has to round-trip back through here or that run is
/// listed but permanently un-drillable (QA finding, #1247 PR review round).
///
/// The string-shape checks (absolute path, `..` component) are defense in
/// depth and give a cheap, explicit rejection; the REAL boundary is
/// `worktree_contained`'s canonicalize + prefix check below, which is what
/// catches a symlink inside `lab_dir` pointing outside it (a pure string
/// check can't — canonicalize resolves the link to its real target before
/// the prefix comparison). `lab_dir.join("")` is `lab_dir` unchanged, so an
/// empty `dir` still passes through the SAME containment check as any other
/// value (`worktree_contained` trivially holds for a path against itself) —
/// no separate code path, no separate risk surface.
fn resolve_lab_run_dir(lab_dir: &StdPath, dir: &str) -> Option<PathBuf> {
    if StdPath::new(dir).is_absolute() {
        return None;
    }
    if StdPath::new(dir)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let candidate = lab_dir.join(dir);
    worktree_contained(&candidate, lab_dir).then_some(candidate)
}

/// One run cluster's summary row for `GET /lab/runs`.
#[derive(Debug, serde::Serialize)]
struct LabRunSummary {
    /// POSIX-style path relative to `lab_dir` — the identifier every other
    /// `/lab/*` endpoint's `dir` query param takes.
    dir: String,
    /// Newest mtime among the run's marker artifacts, epoch milliseconds.
    mtime_ms: u64,
    case_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    crew: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    /// The resolved per-seat staffing (model/k/max_tokens/n_ctx) this run
    /// actually used — `None` only for a brand-new live run whose first case
    /// hasn't completed yet (no `funnels.json` snapshot exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    staffing: Option<darkmux_lab::lab::funnel::CrewStaffingSnapshot>,
    bundles: usize,
    raw_flags: usize,
    deduped_flags: usize,
    confirmed: usize,
    needs_check: usize,
    archived: usize,
    degenerate: bool,
    /// `scores.json` exists — the run reached its terminal artifact write.
    /// The viewer's live/finished badge keys on this.
    finished: bool,
    has_funnels: bool,
    has_events: bool,
}

/// GET /lab/runs — every run cluster under the lab observer's scan root,
/// newest-first. `configured: false` (empty `runs`) when the daemon started
/// without `--lab-dir` — never a 404/500, so the viewer renders a clean
/// "not configured" state instead of an error.
async fn lab_runs_handler(State(state): State<AppState>) -> impl IntoResponse {
    let Some(lab_dir) = state.lab_dir.clone() else {
        return axum::Json(serde_json::json!({ "configured": false, "runs": [] }))
            .into_response();
    };
    let runs = tokio::task::spawn_blocking(move || scan_lab_runs(&lab_dir))
        .await
        .unwrap_or_default();
    axum::Json(serde_json::json!({ "configured": true, "runs": runs })).into_response()
}

fn scan_lab_runs(lab_dir: &StdPath) -> Vec<LabRunSummary> {
    let mut out = Vec::new();
    scan_lab_dir_rec(lab_dir, lab_dir, 0, &mut out);
    // Newest-first — the series/history view wants the latest run of each
    // task up top; sorting here (not client-side) keeps the contract simple
    // for any other consumer of this endpoint.
    // sort_by_key + Reverse rather than a comparator closure — clippy 1.97
    // (CI's toolchain; local 1.94 doesn't flag it) lints the closure form,
    // same toolchain-skew class as the #1259 fix.
    out.sort_by_key(|r| std::cmp::Reverse(r.mtime_ms));
    out
}

fn scan_lab_dir_rec(dir: &StdPath, lab_dir: &StdPath, depth: usize, out: &mut Vec<LabRunSummary>) {
    if depth > LAB_SCAN_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let has_funnels = dir.join("funnels.json").is_file();
    let has_events = dir.join("funnel-events.jsonl").is_file();
    let has_scores = dir.join("scores.json").is_file();
    if has_funnels || has_events || has_scores {
        if let Some(summary) = build_lab_run_summary(dir, lab_dir, has_funnels, has_events, has_scores) {
            out.push(summary);
        }
        // Deliberately DOES NOT `return` here — a match does not stop the
        // scan from continuing into this dir's OTHER subdirectories (see
        // below). Verified live against a real corpus (#1247 manual
        // verification): a stray top-level `funnels.json` sitting directly
        // in `--lab-dir` (leftover from an earlier ad-hoc run) would
        // otherwise swallow the ENTIRE tree — every nested run under it
        // silently disappears from `/lab/runs`. The property this guards
        // against (never walking INTO a matched run's own `worktrees/`/
        // `cases/` checkout looking for decoy runs) is already enforced
        // unconditionally by `LAB_SCAN_SKIP_DIRS` below, independent of
        // whether the current dir itself matched — so skipping the `return`
        // loses nothing on that front while fixing the swallow bug.
    }
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        // `DirEntry::file_type()` reports the entry's OWN type (does not
        // follow a symlink) — a symlinked directory is skipped here rather
        // than walked, matching the containment guard's symlink-escape
        // stance for the scan side too.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if LAB_SCAN_SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        subdirs.push(entry.path());
    }
    for sub in subdirs {
        scan_lab_dir_rec(&sub, lab_dir, depth + 1, out);
    }
}

fn build_lab_run_summary(
    dir: &StdPath,
    lab_dir: &StdPath,
    has_funnels: bool,
    has_events: bool,
    has_scores: bool,
) -> Option<LabRunSummary> {
    let rel = dir
        .strip_prefix(lab_dir)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");

    let mut mtime_ms = 0u64;
    for name in ["scores.json", "funnels.json", "funnel-events.jsonl"] {
        if let Ok(ms) = mtime_ms_of(&dir.join(name)) {
            mtime_ms = mtime_ms.max(ms);
        }
    }

    let mut case_ids: Vec<String> = Vec::new();
    let mut crew: Option<String> = None;
    let mut exec_mode: Option<String> = None;
    let mut staffing = None;
    let mut bundles = 0usize;
    let mut raw_flags = 0usize;
    let mut deduped_flags = 0usize;
    let mut confirmed = 0usize;
    let mut needs_check = 0usize;
    let mut archived = 0usize;
    let mut degenerate = false;

    if has_funnels {
        if let Ok(text) = std::fs::read_to_string(dir.join("funnels.json")) {
            if let Ok(envs) = serde_json::from_str::<Vec<darkmux_lab::lab::funnel::FunnelEnvelope>>(&text) {
                for e in &envs {
                    if !case_ids.contains(&e.case_id) {
                        case_ids.push(e.case_id.clone());
                    }
                    if crew.is_none() {
                        crew = Some(e.crew.clone());
                    }
                    if exec_mode.is_none() {
                        exec_mode = Some(e.mode.clone());
                    }
                    if staffing.is_none() {
                        staffing = e.staffing.clone();
                    }
                    bundles += e.bundles;
                    raw_flags += e.raw_flags;
                    deduped_flags += e.deduped_flags;
                    confirmed += e.confirmed;
                    needs_check += e.needs_check;
                    archived += e.archived;
                    degenerate = degenerate || e.degenerate.is_some();
                }
            }
        }
    }

    let mut profile: Option<String> = None;
    if has_scores {
        if let Ok(doc) = darkmux_lab::lab::scores::read_scores(&dir.join("scores.json")) {
            profile = doc.provenance.profile.clone();
            // A NON-funnel bench (scores.json but no funnels.json — Strict/
            // FreeForm/Agentic/Dialectic mode) has its case ids + crew/mode
            // only in the score rows' `detail`/`extras`, never an envelope.
            if !has_funnels {
                for r in &doc.rows {
                    if r.axis == "case" {
                        if let Some(cid) = r.detail.get("case").and_then(|v| v.as_str()) {
                            if !case_ids.contains(&cid.to_string()) {
                                case_ids.push(cid.to_string());
                            }
                        }
                    }
                }
                if crew.is_none() {
                    crew = doc.extras.get("crew").and_then(|v| v.as_str()).map(str::to_string);
                }
                if exec_mode.is_none() {
                    exec_mode =
                        doc.extras.get("exec_mode").and_then(|v| v.as_str()).map(str::to_string);
                }
            }
        }
    }

    // A brand-new live funnel run (no case has completed yet, so
    // `funnels.json` doesn't exist) has ONLY the events file — best-effort
    // case_id/crew/exec_mode from its `funnel.task` "started" records so the
    // run card isn't blank before the first case lands.
    if case_ids.is_empty() && has_events {
        scan_events_prefix_for_task_meta(&dir.join("funnel-events.jsonl"), &mut case_ids, &mut crew, &mut exec_mode);
    }

    case_ids.sort();

    Some(LabRunSummary {
        dir: rel,
        mtime_ms,
        case_ids,
        crew,
        exec_mode,
        profile,
        staffing,
        bundles,
        raw_flags,
        deduped_flags,
        confirmed,
        needs_check,
        archived,
        degenerate,
        finished: has_scores,
        has_funnels,
        has_events,
    })
}

fn mtime_ms_of(path: &StdPath) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0))
}

/// Best-effort case_id/crew/exec_mode extraction from a LIVE run's
/// `funnel-events.jsonl` before any case has completed — reads only the
/// first `PREFIX_SCAN_LINES` lines (a `funnel.task` "started" record for
/// each case lands near where that case's block of events begins) rather
/// than the whole file, so a long-running live file doesn't make the run
/// LIST endpoint expensive.
fn scan_events_prefix_for_task_meta(
    path: &StdPath,
    case_ids: &mut Vec<String>,
    crew: &mut Option<String>,
    exec_mode: &mut Option<String>,
) {
    const PREFIX_SCAN_LINES: usize = 200;
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(PREFIX_SCAN_LINES) {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if rec.get("action").and_then(|a| a.as_str()) != Some("funnel.task") {
            continue;
        }
        let Some(payload) = rec.get("payload") else {
            continue;
        };
        if payload.get("status").and_then(|s| s.as_str()) != Some("started") {
            continue;
        }
        if let Some(cid) = payload.get("case_id").and_then(|v| v.as_str()) {
            if !case_ids.contains(&cid.to_string()) {
                case_ids.push(cid.to_string());
            }
        }
        if crew.is_none() {
            *crew = payload.get("crew").and_then(|v| v.as_str()).map(str::to_string);
        }
        if exec_mode.is_none() {
            *exec_mode = payload.get("exec_mode").and_then(|v| v.as_str()).map(str::to_string);
        }
    }
}

#[derive(serde::Deserialize)]
struct LabDirQuery {
    dir: String,
}

#[derive(serde::Serialize, Default)]
struct LabRunDetailResponse {
    dir: String,
    funnels: Vec<darkmux_lab::lab::funnel::FunnelEnvelope>,
    scores: Option<darkmux_lab::lab::scores::ScoresDoc>,
}

/// GET /lab/run/detail?dir=<rel> — the envelope(s) + scores content for one
/// run. `funnels` is `[]` (never an error) when `funnels.json` is absent or
/// unparseable — a live run before its first case completes, or a non-funnel
/// bench mode. `scores` is `null` the same way.
///
/// DELIBERATELY no byte cap on the two file reads (unlike the events
/// route's `MAX_LAB_EVENTS_READ_BYTES`): `funnels.json` / `scores.json` are
/// bounded-by-construction artifacts darkmux itself wrote (one envelope per
/// case; a heavy real-world run measures single-digit MB), read from a dir
/// the operator explicitly named on their own machine — not an unbounded
/// append stream and not attacker-supplied. The events route caps because a
/// LIVE `funnel-events.jsonl` grows without bound during a run; these don't.
/// If a cap ever becomes warranted (e.g. a future artifact kind), mirror the
/// events route's explicit-backfill spirit rather than truncating JSON.
async fn lab_run_detail_handler(
    State(state): State<AppState>,
    Query(q): Query<LabDirQuery>,
) -> impl IntoResponse {
    let Some(lab_dir) = state.lab_dir.clone() else {
        return lab_not_configured();
    };
    let Some(run_dir) = resolve_lab_run_dir(&lab_dir, &q.dir) else {
        return lab_bad_dir();
    };
    let dir_label = q.dir.clone();
    let (funnels, scores) = tokio::task::spawn_blocking(move || {
        let funnels: Vec<darkmux_lab::lab::funnel::FunnelEnvelope> =
            std::fs::read_to_string(run_dir.join("funnels.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
        let scores = darkmux_lab::lab::scores::read_scores(&run_dir.join("scores.json")).ok();
        (funnels, scores)
    })
    .await
    .unwrap_or_default();
    axum::Json(LabRunDetailResponse { dir: dir_label, funnels, scores }).into_response()
}

#[derive(serde::Deserialize)]
struct LabEventsQuery {
    dir: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, PartialEq, serde::Serialize)]
struct LabRunEventsResponse {
    lines: Vec<serde_json::Value>,
    next_offset: u64,
    /// `scores.json` exists — the run reached its terminal artifact write,
    /// so this file has stopped growing. The client stops polling once it
    /// sees this true (after draining any remaining backlog).
    finished: bool,
}

/// GET /lab/run/events?dir=<rel>&offset=<byte> — POLL-based tail of
/// `funnel-events.jsonl` from a byte offset, returning new complete lines +
/// the next offset. Deliberately NOT SSE: issue #1227 found a stuck
/// long-lived stream over a tailscale hop silently drops without a
/// client-visible signal; a poll loop self-heals on the next tick — the
/// client re-polls every few seconds with the returned `next_offset`
/// (explicit backfill, the same shape `/flow/:date?since=` uses).
async fn lab_run_events_handler(
    State(state): State<AppState>,
    Query(q): Query<LabEventsQuery>,
) -> impl IntoResponse {
    let Some(lab_dir) = state.lab_dir.clone() else {
        return lab_not_configured();
    };
    let Some(run_dir) = resolve_lab_run_dir(&lab_dir, &q.dir) else {
        return lab_bad_dir();
    };
    let offset = q.offset;
    match tokio::task::spawn_blocking(move || tail_lab_events(&run_dir, offset)).await {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "darkmux serve: lab events read failed\n",
        )
            .into_response(),
    }
}

fn lab_not_configured() -> Response {
    (
        StatusCode::NOT_FOUND,
        "darkmux serve: lab observer not configured — restart with --lab-dir <path>\n",
    )
        .into_response()
}

fn lab_bad_dir() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "darkmux serve: bad or out-of-bounds lab run dir\n",
    )
        .into_response()
}

/// Read `funnel-events.jsonl` from `offset`, capped at
/// `MAX_LAB_EVENTS_READ_BYTES` per call. Only consumes up to the LAST
/// complete line in the chunk it read — a torn trailing write (a concurrent
/// writer mid-flush) or hitting the read cap mid-line is left for the next
/// poll rather than dropped or mis-parsed, so `next_offset` always lands on
/// a line boundary.
fn tail_lab_events(dir: &StdPath, offset: u64) -> LabRunEventsResponse {
    let path = dir.join("funnel-events.jsonl");
    let finished = dir.join("scores.json").is_file();
    let empty = |next_offset: u64| LabRunEventsResponse { lines: Vec::new(), next_offset, finished };

    let Ok(mut file) = std::fs::File::open(&path) else {
        return empty(offset);
    };
    let Ok(meta) = file.metadata() else {
        return empty(offset);
    };
    let len = meta.len();
    if offset >= len {
        // Nothing new — or a stale offset from a rotated/truncated file;
        // clamp back to 0 rather than erroring so a client holding a
        // now-invalid offset recovers on its next poll instead of wedging.
        return empty(if offset > len { 0 } else { offset });
    }

    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return empty(offset);
    }
    let cap = (len - offset).min(MAX_LAB_EVENTS_READ_BYTES as u64) as usize;
    let mut buf = vec![0u8; cap];
    let Ok(n) = file.read(&mut buf) else {
        return empty(offset);
    };
    buf.truncate(n);

    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        // No complete line in this chunk yet (a very long line, or the
        // writer hasn't flushed a newline) — report no progress; the client
        // retries with the same offset.
        return empty(offset);
    };
    let consumed = &buf[..=last_nl];
    let mut lines = Vec::new();
    for line in consumed.split(|&b| b == b'\n') {
        if line.is_empty() || line.len() > MAX_FLOW_LINE_BYTES {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
            lines.push(v);
        }
    }
    LabRunEventsResponse { lines, next_offset: offset + last_nl as u64 + 1, finished }
}

/// GET /flow-status — diagnostic snapshot of the flow substrate. Same
/// data shape as `darkmux flow status --json`; the shared shell's
/// store-status pill polls this every 30s. (#170)
///
/// `flow::collect_status()` opens a Redis connection when Redis is
/// configured, so runs on the blocking pool to keep the axum executor
/// free.
async fn flow_status_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(darkmux_flow::collect_status).await;
    match result {
        Ok(status) => axum::Json(
            serde_json::to_value(status)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        ),
        Err(_) => axum::Json(serde_json::json!({
            "error": "flow status collector panicked",
            "generated_at_ms": current_millis(),
        })),
    }
}

async fn model_status_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(darkmux_profiles::lms::list_loaded).await;
    let (models, unreachable) = match result {
        Ok(Ok(m)) => (m, false),
        _ => (Vec::new(), true),
    };
    let generated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    axum::Json(serde_json::json!({
        "models": models,
        "lms_unreachable": unreachable,
        "generated_at_ms": generated_at_ms,
    }))
}

/// (#1286) GET /machine/memory cache TTL. Computed on request with a short
/// in-process cache so a phone-viewer poll burst costs ONE gather; the TTL
/// is recorded in the payload (`cache_ttl_ms`) per the observer-effect
/// constraint — cadence is a visible knob, never adaptive-silent.
const MACHINE_MEMORY_CACHE_TTL: Duration = Duration::from_secs(2);
static MACHINE_MEMORY_CACHE: std::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>> =
    std::sync::Mutex::new(None);

/// GET /machine/memory (#1286) — the live machine-scope memory ledger:
/// resident models with potential-vs-current + color state, pool/limit
/// lines, and the unified-memory pressure rows (swap / compressor /
/// memory-pressure free%). Exactly `darkmux model ledger --json`, served —
/// ONE implementation (`darkmux_profiles::model_ledger`) feeds both.
///
/// Data-source class note (#1286 boundary hygiene): this reads LIVE PROBES
/// (lms metadata + kernel counters) — a third source class, distinct from
/// run artifacts (lab lens) and the flow stream (fleet lenses). Read-only,
/// zero model dispatches; the gather stamps its own cost (`gather_ms`)
/// into the payload. Auth: rides the same remote-only bearer gate as every
/// other route (loopback open, remote requires the token) — nothing extra
/// here.
///
/// The gather shells out (bounded) — runs on the blocking pool. Best-effort
/// contract like /machine/specs: probe failures degrade to `warnings` in
/// the payload, never a 500.
async fn machine_memory_handler() -> axum::Json<serde_json::Value> {
    if let Ok(guard) = MACHINE_MEMORY_CACHE.lock() {
        if let Some((at, v)) = guard.as_ref() {
            if at.elapsed() < MACHINE_MEMORY_CACHE_TTL {
                return axum::Json(v.clone());
            }
        }
    }
    let result = tokio::task::spawn_blocking(darkmux_profiles::model_ledger::gather).await;
    let mut value = match result {
        Ok(ledger) => serde_json::to_value(&ledger)
            .unwrap_or_else(|_| serde_json::json!({"error": "ledger serialization failed"})),
        Err(_) => serde_json::json!({
            "error": "memory-ledger gather panicked",
            "generated_at_ms": current_millis(),
        }),
    };
    if let Some(obj) = value.as_object_mut() {
        // Recorded cadence knob (#1286 observer constraint 4).
        obj.insert(
            "cache_ttl_ms".to_string(),
            serde_json::json!(MACHINE_MEMORY_CACHE_TTL.as_millis() as u64),
        );
    }
    if let Ok(mut guard) = MACHINE_MEMORY_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), value.clone()));
    }
    axum::Json(value)
}

/// GET /machine/specs — local-machine spec sheet for `darkmux fleet
/// status --deep` aggregation. Composes:
/// - identity (machine_id from env)
/// - hardware (ram_total_bytes, ram_free_for_ai_bytes, cpu_brand, os)
/// - software (darkmux_version, flow_schema_version)
/// - state (loaded_models from lms ps; redacted Redis URL from env)
///
/// All fields are best-effort: a missing sysctl, an unreachable `lms`,
/// or an unset env var degrades to `null` (or `[]` / `0` for typed
/// fields) rather than 500-ing. This is the contract `fleet status
/// --deep` relies on — degraded state is a visible cell, not a failed
/// command. (#275 PR-A)
async fn machine_specs_handler() -> axum::Json<serde_json::Value> {
    // Shell-out probes run in spawn_blocking so the async runtime stays
    // responsive. Each result is independent — one failure doesn't
    // cascade.
    let lms_result = tokio::task::spawn_blocking(darkmux_profiles::lms::list_loaded).await;
    let (loaded_models, lms_unreachable) = match lms_result {
        Ok(Ok(m)) => (m, false),
        _ => (Vec::new(), true),
    };
    let ram_total = tokio::task::spawn_blocking(read_ram_total_bytes)
        .await
        .ok()
        .flatten();
    let ram_free = tokio::task::spawn_blocking(read_ram_free_for_ai_bytes)
        .await
        .ok()
        .flatten();
    let cpu_brand = tokio::task::spawn_blocking(read_cpu_brand)
        .await
        .ok()
        .flatten();

    let machine_id = darkmux_flow::resolve_machine_id();
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5); `Display` on
    // `RawRedisUrl` is the redacted form, so this stays password-safe.
    let redis_url_redacted = darkmux_flow::redis_url().map(|u| u.to_string());

    // (#1008) The configured machine utility model (`internal.utility`) + whether
    // it's resident, so the viewer's utility card shows OBSERVED state instead of
    // a hardcoded "resident". `null` when no utility model is registered. The id
    // matches a loaded model by its namespaced identifier OR its bare model key
    // (utility_model_id may be stored either way). Best-effort — a profiles read
    // failure just yields `None`.
    let utility_model = tokio::task::spawn_blocking(|| {
        darkmux_profiles::profiles::load_registry(None)
            .ok()
            .and_then(|lr| lr.registry.utility_model_id().map(str::to_string))
    })
    .await
    .ok()
    .flatten()
    .map(|id| {
        let loaded = loaded_models
            .iter()
            .any(|m| m.identifier == id || m.model == id);
        serde_json::json!({ "id": id, "loaded": loaded })
    });

    let generated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    axum::Json(serde_json::json!({
        "darkmux_version": env!("CARGO_PKG_VERSION"),
        "flow_schema_version": darkmux_flow::FLOW_SCHEMA_VERSION,
        "machine_id": machine_id,
        "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "ram_total_bytes": ram_total,
        "ram_free_for_ai_bytes": ram_free,
        "cpu_brand": cpu_brand,
        "loaded_models": loaded_models,
        "lms_unreachable": lms_unreachable,
        "utility_model": utility_model,
        "redis_url_redacted": redis_url_redacted,
        "generated_at_ms": generated_at_ms,
    }))
}

/// Read total system RAM in bytes. macOS uses `sysctl hw.memsize` which
/// returns the raw byte count. Linux reads `/proc/meminfo`'s
/// `MemTotal` line. Other platforms return `None`. Sync — wrap in
/// `spawn_blocking` for async contexts.
fn read_ram_total_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: u64 = rest
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse()
                    .ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Read the AI-available RAM ceiling in bytes — the same "real
/// headroom" doctor reports, expressed in bytes for JSON precision.
/// Wraps doctor's existing `reclaimable + resident − safety_margin`
/// computation. Returns `None` on non-macOS (today's doctor is
/// macOS-shaped).
fn read_ram_free_for_ai_bytes() -> Option<u64> {
    let gb = darkmux_doctor::reclaimable_gb_for_specs()?;
    let resident_gb = darkmux_profiles::lms::list_loaded()
        .ok()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| darkmux_eureka::parse_size_gb(&m.size))
                .sum::<f64>()
        })
        .unwrap_or(0.0);
    let safety_gb = darkmux_doctor::RAM_SAFETY_MARGIN_GB_FOR_SPECS as f64;
    let real_gb = (gb as f64) + resident_gb - safety_gb;
    if real_gb < 0.0 {
        return Some(0);
    }
    let bytes = (real_gb * 1024.0 * 1024.0 * 1024.0).round() as u64;
    Some(bytes)
}

/// Read the CPU brand string. macOS: `sysctl machdep.cpu.brand_string`
/// (e.g. `"Apple M5 Max"`). Other platforms return `None`. Sync — wrap
/// in `spawn_blocking` for async contexts.
fn read_cpu_brand() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// GET /flow/:date/stream — SSE-tails new flow records for a UTC day.
///
/// When `DARKMUX_REDIS_URL` is set + reachable at request time, the
/// stream comes from `XREAD BLOCK darkmux:flow $` — the **fleet-wide**
/// tail of new records from every machine writing to the shared
/// stream. When Redis is unset OR unreachable at probe time, falls
/// back to file-tailing `<flows_dir>/<date>.jsonl` (today's
/// per-machine behavior). #270 PR-B.
///
/// Tail-from-now semantics on both paths: pre-existing entries are
/// NOT replayed.
async fn flow_stream_handler(
    State(state): State<AppState>,
    Path(date_raw): Path<String>,
) -> Result<Sse<futures::stream::BoxStream<'static, Result<Event, std::convert::Infallible>>>, (StatusCode, &'static str)> {
    let Some(date) = is_valid_date(&date_raw) else {
        return Err((StatusCode::BAD_REQUEST, "bad date format"));
    };
    let date_owned = date.to_string();

    // (#925) Bound concurrent SSE streams so a client can't exhaust
    // connections. fetch_add-then-check; roll back + 503 if over the cap.
    // The `SseSlot` (carried in the stream below) decrements on disconnect.
    let prev = state.sse_open.fetch_add(1, Ordering::SeqCst);
    if prev >= MAX_CONCURRENT_SSE {
        state.sse_open.fetch_sub(1, Ordering::SeqCst);
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "too many concurrent streams",
        ));
    }
    let slot = SseSlot(Arc::clone(&state.sse_open));

    // Probe Redis once at request time. If the URL is set AND we can
    // open a connection, use the XREAD path. Otherwise fall back to
    // the file-tail. Probe is cheap (~ms) and bounds the failure mode
    // to "operator misconfigured DARKMUX_REDIS_URL → degraded but
    // serving" rather than 500.
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let redis_url = darkmux_flow::redis_url();
    let redis_reachable = if let Some(url) = &redis_url {
        let probe_url = url.clone();
        tokio::task::spawn_blocking(move || {
            // Bounded by REDIS_CONNECT_TIMEOUT (#278) — Studio-offline
            // scenarios must not wedge the SSE-handler probe.
            redis::Client::open(probe_url.expose_for_probe())
                .ok()
                .and_then(|c| {
                    c.get_connection_with_timeout(darkmux_flow::REDIS_CONNECT_TIMEOUT)
                        .ok()
                })
                .is_some()
        })
        .await
        .unwrap_or(false)
    } else {
        false
    };

    use futures::stream::StreamExt;
    let event_stream: futures::stream::BoxStream<'static, Result<Event, std::convert::Infallible>> =
        if redis_reachable {
            let url = redis_url.expect("redis_reachable implies url");
            let stream_name = darkmux_types::config_access::redis_stream(); // (#875)
            Box::pin(
                // expose_for_probe(): redis_tail_lines + resolve_current_last_id
                // use the URL only for Client::open and never log it (they
                // pre-date the RawRedisUrl wrapper and handle the raw string
                // safely), so the secret stays unlogged. (#661 Slice 5)
                redis_tail_lines(url.expose_for_probe().to_string(), stream_name, date_owned)
                    .map(|line| Ok(Event::default().data(line))),
            )
        } else {
            if redis_url.is_some() {
                eprintln!(
                    "darkmux serve: GET /flow/{date_owned}/stream Redis probe failed; \
                     falling back to file tail"
                );
            }
            let path = state.flows_dir.join(format!("{date_owned}.jsonl"));
            let start_offset = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            Box::pin(build_tail_stream(path, start_offset))
        };

    // (#925) Carry the SSE slot inside the stream so the open-count decrement
    // happens exactly when the connection ends or the client disconnects.
    let guarded = stream::unfold((event_stream, slot), |(mut s, slot)| async move {
        s.next().await.map(|item| (item, (s, slot)))
    });
    Ok(Sse::new(guarded.boxed()).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// GET /flow/:date — returns flow records for a UTC day.
///
/// `GET /flow/<date>` → JSON array (`application/json`) of the day's flow
/// records. When `DARKMUX_REDIS_URL` is set + reachable, the array is
/// aggregated from Redis (`darkmux:flow` stream, filtered by date) — the
/// **fleet-wide** view across every machine writing to the same stream. When
/// Redis is unconfigured OR unreachable, the array comes from the local
/// `<date>.jsonl` file (#270). The viewer's playback + backfill consume this.
///
/// `<date>` must be a bare `YYYY-MM-DD` (no extension). A missing day is an
/// empty array (200), not a 404 — "the flow records for date X" is an empty
/// collection, not an absent resource. (The pre-#270 `<date>.jsonl` ndjson
/// shape was retired in #701 — nothing fetched it once the `/flow` page
/// became a redirect stub.)
async fn flow_handler(
    State(state): State<AppState>,
    Path(segment): Path<String>,
    Query(params): Query<FlowQuery>,
) -> impl IntoResponse {
    let Some(date) = is_valid_date(&segment) else {
        return (StatusCode::BAD_REQUEST, "bad date format").into_response();
    };
    let mut records = aggregate_flow_records_for_date(date, &state.flows_dir).await;
    // (#1173) Optional `?since=<iso-ts>` incremental filter. The live viewer's
    // 20s reconcile backstop passes the newest ts it already holds, so the
    // response is just the recent tail rather than the whole (multi-MB) day —
    // the client stops re-parsing the full day file every tick. Flow ts are
    // uniform second-precision UTC (...Z), so a lexical `>=` sorts
    // chronologically; a record missing `ts` is kept (never silently dropped).
    if let Some(since) = params.since.as_deref().filter(|s| !s.is_empty()) {
        records.retain(|r| match r.get("ts").and_then(|v| v.as_str()) {
            Some(ts) => ts >= since,
            None => true,
        });
    }
    axum::Json(records).into_response()
}

/// Query params for `GET /flow/:date`. `?since=<iso-ts>` bounds the response to
/// records at or after that timestamp — the #1173 incremental-reconcile
/// backstop. Absent = the whole day, unchanged.
#[derive(serde::Deserialize)]
struct FlowQuery {
    #[serde(default)]
    since: Option<String>,
}

/// `GET /flow-days` → the playback **catalog**: every day with a flow file on
/// disk, newest-first, each with a one-line summary (record count, the distinct
/// missions seen, and how many dispatches ran). The viewer's day-picker consumes
/// this so historical playback is *discoverable* — you pick a day from a list
/// instead of having to know the date and type `/play/<date>` (#691).
///
/// Deliberately **disk-backed** (the durable per-day files), distinct from the
/// live view's Redis-fed rolling window: the catalog is "everything, by day",
/// the live view is "now-ish on present machines". Storage stays per-day append;
/// this is the navigation layer over it. A `mission_id`/`session_id` cross-day
/// index is the scale path (not built here) — scanning day files is fine at the
/// per-operator scale this serves.
async fn flow_days_handler(State(state): State<AppState>) -> impl IntoResponse {
    let dir = state.flows_dir.clone();
    let days = tokio::task::spawn_blocking(move || scan_flow_days(&dir))
        .await
        .unwrap_or_default();
    axum::Json(serde_json::json!({
        "days": days,
        "generated_at_ms": current_millis(),
    }))
}

/// Scan `flows_dir` for `YYYY-MM-DD.jsonl` files and summarize each. Newest day
/// first. Unreadable / malformed files are skipped (best-effort catalog, never
/// an error that hides the days that DO parse). Schema-header lines (`_type ==
/// "schema"`) don't count as records.
fn scan_flow_days(flows_dir: &std::path::Path) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    let mut days: Vec<serde_json::Value> = Vec::new();
    let Ok(entries) = std::fs::read_dir(flows_dir) else {
        return days;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(date) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if is_valid_date(date).is_none() {
            continue;
        }
        // Stream the file line-by-line (the catalog needs only counts + ids, not
        // the parsed content) so a large day file is never held whole in memory.
        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        let mut records: u64 = 0;
        let mut missions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut dispatches: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("_type").and_then(|t| t.as_str()) == Some("schema") {
                continue;
            }
            records += 1;
            if let Some(m) = v.get("mission_id").and_then(|m| m.as_str()) {
                missions.insert(m.to_string());
            }
            // A dispatch = a dispatch.start edge (tolerate the dotted + spaced
            // action forms the flow stream carries).
            let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
            if action == "dispatch.start" || action == "dispatch start" {
                if let Some(s) = v.get("session_id").and_then(|s| s.as_str()) {
                    dispatches.insert(s.to_string());
                }
            }
        }
        days.push(serde_json::json!({
            "date": date,
            "records": records,
            "missions": missions.into_iter().collect::<Vec<_>>(),
            "dispatches": dispatches.len(),
        }));
    }
    days.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });
    days
}

// ── #691: cross-day mission/dispatch catalog ──────────────────────────────────
// `/flow-days` lists days; the catalog lists the units users actually think in —
// missions and dispatch sessions — by scanning ACROSS day files. A mission that
// spans midnight is one filtered view, not two unrelated day rows. Deliberately
// DISK-backed (the durable per-day files), the same source seam as scan_flow_days
// and distinct from the live view's Redis-fed rolling window. A mission_id /
// session_id index is the scale path (not built here); streaming day files is
// fine at per-operator scale.

/// Max records returned by a single by-mission / by-session catalog fetch. A
/// mission spanning many days could otherwise return an unbounded array; the cap
/// is tied structurally to the per-day file cap (`MAX_FLOW_FILE_RECORDS`) so the
/// two can't drift, and `truncated` flags when it's hit.
const MAX_CATALOG_RECORDS: usize = MAX_FLOW_FILE_RECORDS;

/// Validate a catalog id path param (mission_id / session_id). These are FILTER
/// values compared to record fields, never filesystem paths — but bound the
/// length + charset so a hostile client can't smuggle control bytes into a
/// response or force a pathological scan.
fn is_valid_catalog_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// Visit every flow record across all `YYYY-MM-DD.jsonl` day files in
/// `flows_dir`, OLDEST day first (so visitors see chronological order). Streams
/// each file line-by-line (never whole-file), skipping blank + schema-header
/// lines. Best-effort: unreadable/malformed files + lines are skipped. The
/// visitor returns `ControlFlow::Break` to stop early (e.g. once a fetch hits its
/// record cap). Shared cross-day scan primitive the catalog endpoints filter over
/// (#691) — factored from the same read_dir + line-parse loop as scan_flow_days.
fn for_each_flow_record_across_days(
    flows_dir: &std::path::Path,
    mut visit: impl FnMut(&str, &serde_json::Value) -> std::ops::ControlFlow<()>,
) {
    use std::io::BufRead;
    let Ok(entries) = std::fs::read_dir(flows_dir) else {
        return;
    };
    let mut day_files: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(date) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if is_valid_date(date).is_none() {
            continue;
        }
        day_files.push((date.to_string(), entry.path()));
    }
    // Oldest-first → the visitor sees records in chronological order across days.
    day_files.sort_by(|a, b| a.0.cmp(&b.0));
    for (date, path) in day_files {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("_type").and_then(|t| t.as_str()) == Some("schema") {
                continue;
            }
            if visit(&date, &v).is_break() {
                return;
            }
        }
    }
}

/// `GET /flow-missions` → the cross-day mission catalog: every `mission_id` seen
/// across all day files, with a rollup (records, dispatch count, date span, ts
/// span, machines), newest-activity first. The viewer lists these so you can
/// "replay the mission" instead of hunting through days (#691). Disk-backed.
async fn flow_missions_handler(State(state): State<AppState>) -> impl IntoResponse {
    let dir = state.flows_dir.clone();
    let missions = tokio::task::spawn_blocking(move || scan_flow_missions(&dir))
        .await
        .unwrap_or_default();
    axum::Json(serde_json::json!({
        "missions": missions,
        "generated_at_ms": current_millis(),
    }))
}

fn scan_flow_missions(flows_dir: &std::path::Path) -> Vec<serde_json::Value> {
    use std::collections::{BTreeSet, HashMap};
    struct Agg {
        records: u64,
        dispatches: BTreeSet<String>,
        machines: BTreeSet<String>,
        first_ts: String,
        last_ts: String,
        first_date: String,
        last_date: String,
    }
    let mut by_mission: HashMap<String, Agg> = HashMap::new();
    for_each_flow_record_across_days(flows_dir, |date, v| {
        let Some(mid) = v.get("mission_id").and_then(|m| m.as_str()) else {
            return std::ops::ControlFlow::Continue(());
        };
        if mid.is_empty() {
            return std::ops::ControlFlow::Continue(());
        }
        let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
        let e = by_mission.entry(mid.to_string()).or_insert_with(|| Agg {
            records: 0,
            dispatches: BTreeSet::new(),
            machines: BTreeSet::new(),
            first_ts: ts.to_string(),
            last_ts: ts.to_string(),
            first_date: date.to_string(),
            last_date: date.to_string(),
        });
        e.records += 1;
        if !ts.is_empty() {
            if e.first_ts.is_empty() || ts < e.first_ts.as_str() {
                e.first_ts = ts.to_string();
            }
            if ts > e.last_ts.as_str() {
                e.last_ts = ts.to_string();
            }
        }
        if e.first_date.is_empty() || date < e.first_date.as_str() {
            e.first_date = date.to_string();
        }
        if date > e.last_date.as_str() {
            e.last_date = date.to_string();
        }
        let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
        if action == "dispatch.start" || action == "dispatch start" {
            if let Some(s) = v.get("session_id").and_then(|s| s.as_str()) {
                e.dispatches.insert(s.to_string());
            }
        }
        if let Some(mach) = v.get("machine_id").and_then(|m| m.as_str()) {
            if !mach.is_empty() {
                e.machines.insert(mach.to_string());
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    let mut out: Vec<serde_json::Value> = by_mission
        .into_iter()
        .map(|(mid, a)| {
            serde_json::json!({
                "mission_id": mid,
                "records": a.records,
                "dispatches": a.dispatches.len(),
                "machines": a.machines.into_iter().collect::<Vec<_>>(),
                "first_ts": a.first_ts,
                "last_ts": a.last_ts,
                "first_date": a.first_date,
                "last_date": a.last_date,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["last_ts"]
            .as_str()
            .unwrap_or("")
            .cmp(a["last_ts"].as_str().unwrap_or(""))
    });
    out
}

/// `GET /flow-mission/:id` → the flow records for one mission, across days, in
/// chronological order (the "replay this mission" payload the viewer loads).
async fn flow_mission_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    catalog_records_response(state, "mission_id", id).await
}

/// `GET /flow-session/:id` → the flow records for one dispatch session, across
/// days (the "replay this dispatch" payload).
async fn flow_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    catalog_records_response(state, "session_id", id).await
}

async fn catalog_records_response(
    state: AppState,
    field: &'static str,
    id: String,
) -> axum::response::Response {
    if !is_valid_catalog_id(&id) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "invalid id (alphanumeric + -_.: , <=128 chars)",
        )
            .into_response();
    }
    let dir = state.flows_dir.clone();
    let (records, truncated) =
        tokio::task::spawn_blocking(move || collect_records_by_field(&dir, field, &id))
            .await
            .unwrap_or_else(|_| (Vec::new(), false));
    axum::Json(serde_json::json!({
        "records": records,
        "count": records.len(),
        "truncated": truncated,
        "generated_at_ms": current_millis(),
    }))
    .into_response()
}

/// Collect every record whose top-level string `field` equals `id`, across all
/// day files in chronological order, bounded at MAX_CATALOG_RECORDS. Returns the
/// records + whether the cap truncated the result. Stops scanning once the cap is
/// hit (ControlFlow::Break) rather than reading the rest of history.
fn collect_records_by_field(
    flows_dir: &std::path::Path,
    field: &str,
    id: &str,
) -> (Vec<serde_json::Value>, bool) {
    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    for_each_flow_record_across_days(flows_dir, |_date, v| {
        if v.get(field).and_then(|f| f.as_str()) == Some(id) {
            if records.len() >= MAX_CATALOG_RECORDS {
                truncated = true;
                return std::ops::ControlFlow::Break(());
            }
            records.push(v.clone());
        }
        std::ops::ControlFlow::Continue(())
    });
    (records, truncated)
}

/// Aggregate the day's flow records — Redis-when-available, file-otherwise.
///
/// Resolution order:
/// 1. `DARKMUX_REDIS_URL` set + reachable → `XREVRANGE darkmux:flow + - COUNT N`
///    (newest-first — #809: cap-saturation must cut the oldest entries, never
///    the newest), parse each entry's `record` field, filter by
///    `record.ts.starts_with(date)`, reverse back to chronological.
///    `darkmux:flow` stream override honored via `DARKMUX_REDIS_STREAM`.
/// 2. Redis configured but unreachable → log the fallback once and read
///    the local file. Daemon stays serving rather than 500-ing.
/// 3. `DARKMUX_REDIS_URL` unset → read the local file directly.
///
/// Missing-file is not an error here: empty array is the correct response
/// for a date the local machine has no record of. The viewer can ask
/// `flow-status` to know whether Redis is participating.
async fn aggregate_flow_records_for_date(
    date: &str,
    flows_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let redis_url = darkmux_flow::redis_url();

    if let Some(url) = redis_url {
        let date_owned = date.to_string();
        let url_owned = url.clone();
        let redis_result = tokio::task::spawn_blocking(move || {
            read_flow_records_from_redis(url_owned.expose_for_probe(), &date_owned)
        })
        .await;
        match redis_result {
            Ok(Ok(records)) => return records,
            Ok(Err(e)) => {
                eprintln!(
                    "darkmux serve: GET /flow/{date} Redis aggregation failed ({e}); \
                     falling back to local file"
                );
            }
            Err(e) => {
                eprintln!(
                    "darkmux serve: GET /flow/{date} blocking task join error ({e}); \
                     falling back to local file"
                );
            }
        }
    }

    read_flow_records_from_file(date, flows_dir).await
}

/// XRANGE the flow stream + filter by `record.ts` matching `<date>`.
/// Synchronous (uses the sync `redis::Client`) — call site wraps in
/// `spawn_blocking` so the daemon's async runtime stays responsive.
fn read_flow_records_from_redis(
    url: &str,
    date: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    use anyhow::Context;
    // NOTE: never interpolate `url` into a log/error — it may carry an inline
    // password (the env-URL tier). Use the non-secret `{date}` for context.
    // (#661 Slice 5 — sealing a pre-existing raw-URL log the audit flagged.)
    let client = redis::Client::open(url)
        .with_context(|| format!("opening Redis client for /flow aggregation (date {date})"))?;
    // Bounded by REDIS_CONNECT_TIMEOUT (#278) — Studio-offline scenarios
    // must not wedge the HTTP handler thread for the OS default 75s.
    let mut conn = client
        .get_connection_with_timeout(darkmux_flow::REDIS_CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to Redis for /flow aggregation (date {date})"))?;
    let stream = darkmux_types::config_access::redis_stream(); // (#875)
    // (#809) NEWEST-first read. The stream rides at its `MAXLEN ~` cap once
    // the fleet has been busy long enough (XLEN floats a little above the
    // cap — trimming is lazy), and an oldest-first `XRANGE - + COUNT N`
    // then silently DROPS the newest entries — the live view loses exactly
    // the most recent records at exactly the busiest moment (found live:
    // the operator's orchestrator note "not showing"). XREVRANGE makes
    // cap-saturation cut the OLDEST entries instead, which the date filter
    // below mostly discards anyway. Entries are reversed after parsing so
    // callers still see chronological order.
    let raw: redis::Value = redis::cmd("XREVRANGE")
        .arg(&stream)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(10000)
        .query(&mut conn)
        .with_context(|| format!("XREVRANGE on {stream}"))?;
    let entries = match raw {
        redis::Value::Array(v) => v,
        other => {
            return Err(anyhow::anyhow!(
                "unexpected XRANGE response shape: {other:?}"
            ));
        }
    };
    let mut records = Vec::with_capacity(entries.len().min(10000));
    for entry in entries {
        // Each entry is [id, [k, v, k, v, ...]]. Find the `record` field.
        let pairs = match entry {
            redis::Value::Array(v) if v.len() >= 2 => v,
            _ => continue,
        };
        let fields = match &pairs[1] {
            redis::Value::Array(f) => f,
            _ => continue,
        };
        let Some(json) = extract_record_field(fields) else { continue };
        if !record_ts_matches_date(json, date) {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        records.push(parsed);
    }
    // XREVRANGE yields newest-first; restore chronological order so the
    // response matches the local-file path's ordering (#809).
    records.reverse();
    Ok(records)
}

/// The most records `GET /flow/:date` returns from the local day-file —
/// the newest `MAX_FLOW_FILE_RECORDS`, matching the Redis path's
/// `XREVRANGE … COUNT 10000` cap (#900). The file path previously read the
/// WHOLE file into memory + parsed every line into a `Vec`, so a large
/// day-file under concurrent requests could OOM the daemon (the Redis path
/// was already bounded; the snapshot path wasn't).
const MAX_FLOW_FILE_RECORDS: usize = 10_000;

/// Parse `<flows_dir>/<date>.jsonl` into a Vec of JSON values, keeping only
/// the newest `MAX_FLOW_FILE_RECORDS` (chronological order preserved).
/// Missing file = empty Vec (not an error).
///
/// (#900) Streams the file line-by-line and holds a bounded ring of parsed
/// records, so transient memory is ~one line + the cap regardless of the
/// day-file's size — instead of an unbounded `tokio::fs::read` + full-DOM
/// parse. Mirrors the Redis path's newest-first `COUNT 10000` semantics.
async fn read_flow_records_from_file(
    date: &str,
    flows_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    use tokio::io::AsyncReadExt;
    let path = flows_dir.join(format!("{date}.jsonl"));
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return Vec::new(), // missing file = empty (not an error)
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut ring: std::collections::VecDeque<serde_json::Value> =
        std::collections::VecDeque::with_capacity(MAX_FLOW_FILE_RECORDS.min(1024));

    // (#925) Bounded line read: accumulate bytes up to MAX_FLOW_LINE_BYTES; a
    // line that exceeds the cap (a corrupt/adversarial no-newline flows file)
    // is skipped — drained to its next newline — rather than read into one
    // ballooning allocation. Chunked (not byte-at-a-time) for speed.
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut over_cap = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(_) => break,
        };
        for &b in &chunk[..n] {
            if b == b'\n' {
                if !over_cap {
                    push_flow_line(&line, &mut ring);
                }
                line.clear();
                over_cap = false;
            } else if over_cap {
                // Draining an over-long line — discard until its newline.
            } else if line.len() >= MAX_FLOW_LINE_BYTES {
                eprintln!(
                    "darkmux serve: skipping a >{MAX_FLOW_LINE_BYTES}-byte line in {} \
                     (corrupt/adversarial flows file)",
                    path.display()
                );
                over_cap = true;
                line.clear();
            } else {
                line.push(b);
            }
        }
    }
    // A final line with no trailing newline.
    if !over_cap && !line.is_empty() {
        push_flow_line(&line, &mut ring);
    }
    ring.into()
}

/// (#925) Parse one flow-record line and push it into the bounded ring
/// (newest-`MAX_FLOW_FILE_RECORDS`). Non-UTF-8, empty, or non-JSON lines are
/// dropped silently — the same tolerance the old `.lines()` loop had.
fn push_flow_line(line: &[u8], ring: &mut std::collections::VecDeque<serde_json::Value>) {
    let s = match std::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    if s.is_empty() {
        return;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        if ring.len() == MAX_FLOW_FILE_RECORDS {
            ring.pop_front();
        }
        ring.push_back(v);
    }
}

/// Tail a `.jsonl` file from `start_offset`, yielding each new complete
/// line as a `String`. The testable core of the SSE machinery — kept
/// `Event`-free so tests assert against raw line content.
///
/// Polls at 250ms. Each `.next()` yields exactly one line. The unfold
/// inner loop continues until a real line is available (no empty ticks).
///
/// State: (path, byte offset, incomplete-trailing-line buffer, pending lines).
fn tail_lines(
    path: PathBuf,
    start_offset: u64,
) -> impl Stream<Item = String> {
    let state: TailState = (path, start_offset, String::new(), VecDeque::new());
    stream::unfold(state, move |mut s| async move {
        loop {
            // 1. If we have queued lines, emit the next one immediately.
            if let Some(line) = s.3.pop_front() {
                return Some((line, s));
            }

            // 2. Otherwise, wait and poll the file for new bytes.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let size = tokio::fs::metadata(&s.0)
                .await
                .map(|m| m.len())
                .unwrap_or(0);

            // File rotated / truncated — reset offset.
            if size < s.1 {
                s.1 = 0;
                s.2.clear();
            }
            if size <= s.1 {
                continue;
            }

            // 3. Read the new bytes, parse out complete lines, queue them.
            let Ok(mut file) = tokio::fs::File::open(&s.0).await else { continue };
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            if file.seek(std::io::SeekFrom::Start(s.1)).await.is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).await.is_err() {
                continue;
            }
            s.1 = size;
            s.2.push_str(&String::from_utf8_lossy(&buf));

            // Drain complete lines (ending in \n) into pending.
            while let Some(nl) = s.2.find('\n') {
                let line: String = s.2.drain(..nl).collect();
                s.2.drain(..1);
                if !line.is_empty() {
                    s.3.push_back(line);
                }
            }
            // s.2 now holds the incomplete trailing chunk (if any).
            // Loop back to top — if pending now non-empty, we'll emit it.
        }
    })
}

type TailState = (PathBuf, u64, String, VecDeque<String>);

/// SSE wrapper around `tail_lines`. Maps each line to an `Event` with
/// the line as `data:` payload. axum's `KeepAlive` layer handles
/// liveness comments during quiet periods.
fn build_tail_stream(
    path: PathBuf,
    start_offset: u64,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> {
    use futures::stream::StreamExt;
    tail_lines(path, start_offset).map(|line| Ok(Event::default().data(line)))
}

/// Long-poll Redis `XREAD BLOCK` and yield each matching record as a
/// JSON line. Tail-from-now semantics — pre-existing entries are NOT
/// replayed.
///
/// **Eager subscription**: `redis_tail_lines` spawns a background
/// tokio task at call time (not lazily on first poll). The task
/// resolves the current `last-generated-id` via `XINFO STREAM`
/// immediately, pins that as the watermark, and starts `XREAD BLOCK`
/// against it. Records pass through an unbounded mpsc channel; the
/// returned Stream is just the receiver. Eagerness matters because
/// otherwise — between `redis_tail_lines(...)` returning and the
/// caller's first `await` — every XADD slips below `"$"` (which
/// `XREAD` re-interprets as "current max RIGHT NOW" on every call).
///
/// Filter: emit only records whose `ts` field begins with
/// `date_filter` (the UTC date the viewer is looking at). On Redis
/// errors mid-stream the task backs off 500ms per attempt and tracks
/// consecutive failures. The background task exits via two paths:
///
/// 1. **Receiver-drop**: consumer disconnected — `tx.send` returns
///    Err on the next attempted produce, task returns silently
///    (channel close not separately signaled). (#270 PR-B)
/// 2. **Persistent failure**: `MAX_CONSECUTIVE_XREAD_FAILURES`
///    (10) consecutive XREAD errors → emit a synthetic
///    `stream.error` flow record + return. Operator with a viewer
///    tab open during a Redis outage sees a clean terminal event
///    + the channel closes. (#293)
fn redis_tail_lines(
    url: String,
    stream_name: String,
    date_filter: String,
) -> impl Stream<Item = String> {
    // Bounded channel — hyper's TCP backpressure only reaches as far
    // as the SSE stream's `.poll_next`; the channel between the
    // spawned XREAD task and the SSE stream is an application-layer
    // buffer not covered by that backpressure. With the pre-#294
    // `unbounded_channel`, a slow consumer (paused tab, bad network,
    // pathological reader) let the producer fill the channel
    // indefinitely — daemon RAM grew linearly with the duration of
    // the consumer's lag. The fleet-wide /flow data surface makes
    // this a daemon-level DoS surface a single misbehaving tab can
    // hit. Bound at `SSE_MPSC_CAPACITY` records; drop-newest on full
    // with operator-visible log. (#294)
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(SSE_MPSC_CAPACITY);

    tokio::spawn(async move {
        // Eager: resolve the watermark NOW, before the consumer can
        // race with us.
        let url_for_resolve = url.clone();
        let stream_for_resolve = stream_name.clone();
        let mut last_id = match tokio::task::spawn_blocking(move || {
            resolve_current_last_id(&url_for_resolve, &stream_for_resolve)
        })
        .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                eprintln!(
                    "darkmux serve: XINFO STREAM on {stream_name} failed ({e}); \
                     starting at 0-0"
                );
                "0-0".to_string()
            }
            Err(e) => {
                eprintln!(
                    "darkmux serve: XINFO blocking task join error ({e}); starting at 0-0"
                );
                "0-0".to_string()
            }
        };

        // #293 — bounded retry budget. The XREAD loop pre-fix retried
        // forever on any error, sleeping 500ms per attempt. On a
        // permanent-Redis-failure scenario (Studio drops; operator
        // left a viewer tab open) this produced "XREAD failed ...
        // backing off 500ms" 2x/sec forever per open SSE stream;
        // tasks leaked because the receiver-drop check at `tx.send`
        // only fires when the task tries to PRODUCE, which never
        // happens on a permanent failure.
        //
        // After MAX_CONSECUTIVE_XREAD_FAILURES, emit a synthetic
        // stream-error record + return. The SSE consumer sees a
        // clean terminal event and can reconnect (or surface the
        // failure to the user); the spawned task exits.
        let mut consecutive_failures: u32 = 0;
        // Local counter — only this spawn task increments + reads via
        // the eprintln below. Plain u64 (not Arc<AtomicU64>) keeps
        // the hot path zero-overhead; if a future operator-visible
        // counter surfaces (metrics endpoint, /healthz), promote to
        // shared atomic at that time.
        let mut dropped_records: u64 = 0;

        loop {
            let url_call = url.clone();
            let stream_call = stream_name.clone();
            let last_id_call = last_id.clone();
            let date_filter_call = date_filter.clone();
            let result = tokio::task::spawn_blocking(move || {
                xread_block_once(&url_call, &stream_call, &last_id_call, &date_filter_call)
            })
            .await;
            match result {
                Ok(Ok((records, new_last_id))) => {
                    last_id = new_last_id;
                    consecutive_failures = 0; // recovered
                    for r in records {
                        match try_send_or_drop_newest(&tx, r) {
                            SendOutcome::Sent => {}
                            SendOutcome::Dropped => {
                                dropped_records += 1;
                                // Log on every drop — operator visibility
                                // matters more than log noise. Rate-limit
                                // is a future hardening if it surfaces.
                                eprintln!(
                                    "{}",
                                    darkmux_types::style::warn(&format!(
                                        "darkmux serve: SSE channel full ({SSE_MPSC_CAPACITY}); \
                                         dropping newest record on stream `{stream_name}` \
                                         (total dropped: {dropped_records})"
                                    ))
                                );
                            }
                            SendOutcome::Closed => {
                                // Receiver dropped — consumer disconnected.
                                return;
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    consecutive_failures += 1;
                    eprintln!(
                        "{}",
                        darkmux_types::style::warn(&format!(
                            "darkmux serve: XREAD on {stream_name} failed ({e}); \
                             backing off 500ms (attempt {consecutive_failures}/{MAX})",
                            MAX = MAX_CONSECUTIVE_XREAD_FAILURES
                        ))
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_XREAD_FAILURES {
                        let synthetic = synthetic_stream_error_record(
                            &stream_name,
                            consecutive_failures,
                            &format!("XREAD failed: {e}"),
                        );
                        // Best-effort synthetic delivery. If the
                        // channel is closed (receiver gone) or full,
                        // the operator still sees the eprintln above —
                        // but log the synthetic-drop separately so the
                        // daemon log preserves the "we tried" record
                        // for forensics. (#293 reviewer B1 follow-up
                        // surfaced via #294 review)
                        if let Err(e) = tx.try_send(synthetic) {
                            eprintln!(
                                "{}",
                                darkmux_types::style::error(&format!(
                                    "darkmux serve: stream-error synthetic not delivered \
                                     on stream `{stream_name}` ({e}); SSE consumer will see \
                                     silent stream close instead of explicit terminal record"
                                ))
                            );
                        }
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    eprintln!(
                        "{}",
                        darkmux_types::style::warn(&format!(
                            "darkmux serve: XREAD blocking task join error ({e}); \
                             backing off 500ms (attempt {consecutive_failures}/{MAX})",
                            MAX = MAX_CONSECUTIVE_XREAD_FAILURES
                        ))
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_XREAD_FAILURES {
                        let synthetic = synthetic_stream_error_record(
                            &stream_name,
                            consecutive_failures,
                            &format!("blocking task join error: {e}"),
                        );
                        // Best-effort synthetic delivery. If the
                        // channel is closed (receiver gone) or full,
                        // the operator still sees the eprintln above —
                        // but log the synthetic-drop separately so the
                        // daemon log preserves the "we tried" record
                        // for forensics. (#293 reviewer B1 follow-up
                        // surfaced via #294 review)
                        if let Err(e) = tx.try_send(synthetic) {
                            eprintln!(
                                "{}",
                                darkmux_types::style::error(&format!(
                                    "darkmux serve: stream-error synthetic not delivered \
                                     on stream `{stream_name}` ({e}); SSE consumer will see \
                                     silent stream close instead of explicit terminal record"
                                ))
                            );
                        }
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// Max consecutive `XREAD` failures before `redis_tail_lines` emits
/// a synthetic stream-error record and exits the spawned task (#293).
/// 10 × 500ms backoff = ~5s total budget — operator who left a
/// topology viewer tab open during a Studio drop sees the failure
/// surface within seconds rather than the task leaking + log-spam
/// forever.
const MAX_CONSECUTIVE_XREAD_FAILURES: u32 = 10;

// ── #294 SSE channel bounding ────────────────────────────────────────

/// Bounded SSE channel capacity (#294). 256 entries × ~1 KB/record
/// typical = ~256 KB worst-case buffer per open SSE stream. Picked
/// to be generous enough that a healthy consumer on a normal network
/// never hits the bound, but small enough that a single pathological
/// tab can't grow the daemon's RAM without limit.
///
/// **Overflow strategy: drop-newest.** On Full, the new record is
/// discarded and existing buffered records are preserved. Trade-off
/// considered (and rejected):
/// - `broadcast::Sender` with drop-oldest semantics — preserves
///   recent records (which is arguably what a "live tail" UX wants).
///   Rejected because the broadcast channel returns `Result<T,
///   BroadcastStreamRecvError>` from Stream, complicating the SSE
///   producer's error-handling story, and the multi-receiver shape
///   is overkill for a one-receiver-per-stream daemon.
///
/// Drop-newest preserves timeline arrival order for the consumer
/// that is keeping up; the consumer that fell behind sees a gap
/// for the most-recent records. Acceptable: SSE clients typically
/// reconnect to recover, and the topology UI's record-display
/// already handles gaps gracefully.
const SSE_MPSC_CAPACITY: usize = 256;

/// Outcome of a `try_send_or_drop_newest` call. `Closed` lets the
/// caller distinguish "consumer left" (exit cleanly) from "consumer
/// is slow" (drop + continue).
#[derive(Debug, PartialEq)]
enum SendOutcome {
    Sent,
    Dropped,
    Closed,
}

/// Try to send `item` on a bounded mpsc channel. Returns a
/// `SendOutcome` so the caller can distinguish the three cases:
/// - `Sent` — record delivered to the consumer
/// - `Dropped` — channel was full; record discarded. Caller logs
///   + increments its own local drop counter
/// - `Closed` — receiver dropped; caller should exit
///
/// Drop-newest is the chosen overflow strategy (see the comment
/// near `SSE_MPSC_CAPACITY`). Extracted as a pure helper for
/// unit-testability without spinning up a tokio runtime per test.
fn try_send_or_drop_newest(
    tx: &tokio::sync::mpsc::Sender<String>,
    item: String,
) -> SendOutcome {
    use tokio::sync::mpsc::error::TrySendError;
    match tx.try_send(item) {
        Ok(()) => SendOutcome::Sent,
        Err(TrySendError::Full(_dropped_item)) => SendOutcome::Dropped,
        Err(TrySendError::Closed(_)) => SendOutcome::Closed,
    }
}

/// Build a synthetic flow-record line representing an unrecoverable
/// stream-error condition. Emitted by `redis_tail_lines` before exit
/// when consecutive XREAD failures exceed the budget. The SSE
/// consumer (topology viewer) sees this as a `data:` line with a
/// recognizable action and can surface it to the user.
fn synthetic_stream_error_record(stream_name: &str, attempts: u32, reason: &str) -> String {
    // Field values MUST match the FlowRecord schema (`flow::Tier`,
    // `Category`, `Stage`, `Level` enums) so a strict consumer
    // (AuditFileSink path, `flow integrity-check`) deserializing
    // the synthetic as FlowRecord doesn't reject it. `tier: "local"`
    // because the daemon process emitting this IS a local-tier
    // observation; `stage: "scope"` because the event is about the
    // stream's lifecycle (not a dispatch / review / ship). Note:
    // the topology viewer's EDGE_STYLES filter at
    // docs/topology/index.html doesn't currently render
    // `stage: scope` records as edges — separate follow-up to add
    // a stream-error pill / toast in the viewer surface.
    serde_json::json!({
        "ts": darkmux_flow::ts_utc_now(),
        "level": "warn",
        "category": "audit",
        "tier": "local",
        "stage": "scope",
        "action": "stream.error",
        "handle": "redis_tail_lines",
        "reasoning": format!(
            "redis tail exited after {attempts} consecutive failures on stream `{stream_name}`: {reason}"
        ),
    })
    .to_string()
}

/// Query `XINFO STREAM <stream>` for `last-generated-id`. Returns
/// `"0-0"` when the stream doesn't exist yet (Redis errors with
/// `ERR no such key`) — meaning "watch every future XADD" since auto-
/// assigned ids will all be greater than `0-0`.
fn resolve_current_last_id(
    url: &str,
    stream_name: &str,
) -> Result<String, anyhow::Error> {
    use anyhow::Context as _;
    let client = redis::Client::open(url)
        .with_context(|| format!("opening Redis for XINFO on {stream_name}"))?;
    // Bounded by REDIS_CONNECT_TIMEOUT (#278).
    let mut conn = client
        .get_connection_with_timeout(darkmux_flow::REDIS_CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to Redis for XINFO on {stream_name}"))?;
    let raw: redis::RedisResult<redis::Value> = redis::cmd("XINFO")
        .arg("STREAM")
        .arg(stream_name)
        .query(&mut conn);
    let items = match raw {
        Ok(redis::Value::Array(v)) => v,
        Ok(other) => return Err(anyhow::anyhow!("XINFO STREAM unexpected shape: {other:?}")),
        Err(e) if e.code() == Some("ERR") => {
            // Stream doesn't exist yet — "0-0" is the right watermark
            // (every future auto-id will sort above it).
            return Ok("0-0".to_string());
        }
        Err(e) => return Err(anyhow::anyhow!("XINFO STREAM {stream_name}: {e}")),
    };
    let mut i = 0;
    while i + 1 < items.len() {
        if redis_value_as_str(&items[i]) == Some("last-generated-id") {
            if let Some(id) = redis_value_as_str(&items[i + 1]) {
                return Ok(id.to_string());
            }
        }
        i += 2;
    }
    // Defensive: XINFO didn't surface last-generated-id — start fresh.
    Ok("0-0".to_string())
}

/// One BLOCK iteration of `XREAD`. Returns `(matching_records,
/// updated_last_id)`. Synchronous — call site wraps in
/// `spawn_blocking`. Uses BLOCK=500ms so the unfold loop can poll
/// without burning CPU but still surfaces new records quickly.
fn xread_block_once(
    url: &str,
    stream_name: &str,
    last_id: &str,
    date_filter: &str,
) -> Result<(Vec<String>, String), anyhow::Error> {
    use anyhow::Context as _;
    let client = redis::Client::open(url)
        .with_context(|| format!("opening Redis for XREAD on {stream_name}"))?;
    // Bounded by REDIS_CONNECT_TIMEOUT (#278). NOTE: the per-iteration
    // XREAD BLOCK timeout below (500ms) is separate — that's the
    // long-poll budget, not the connect budget.
    let mut conn = client
        .get_connection_with_timeout(darkmux_flow::REDIS_CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to Redis for XREAD on {stream_name}"))?;
    let raw: redis::Value = redis::cmd("XREAD")
        .arg("BLOCK")
        .arg(500u64)
        .arg("COUNT")
        .arg(100u64)
        .arg("STREAMS")
        .arg(stream_name)
        .arg(last_id)
        .query(&mut conn)
        .with_context(|| format!("XREAD BLOCK on {stream_name}"))?;

    // XREAD returns:
    //   - Nil (BLOCK timeout) — no new entries; keep current last_id
    //   - [[stream_name, [[id, [k, v, ...]], ...]]] — one or more entries
    let mut records = Vec::new();
    let mut new_last_id = last_id.to_string();
    let outer = match raw {
        redis::Value::Nil => return Ok((records, new_last_id)),
        redis::Value::Array(v) => v,
        other => {
            return Err(anyhow::anyhow!(
                "XREAD: unexpected outer shape: {other:?}"
            ));
        }
    };
    for stream_block in outer {
        let pair = match stream_block {
            redis::Value::Array(v) if v.len() >= 2 => v,
            _ => continue,
        };
        let entries = match &pair[1] {
            redis::Value::Array(v) => v,
            _ => continue,
        };
        for entry in entries {
            let parts = match entry {
                redis::Value::Array(v) if v.len() >= 2 => v,
                _ => continue,
            };
            // Entry id — update last_id no matter whether we emit, so
            // a filtered-out record still advances the cursor.
            if let Some(id) = redis_value_as_str(&parts[0]) {
                new_last_id = id.to_string();
            }
            let fields = match &parts[1] {
                redis::Value::Array(v) => v,
                _ => continue,
            };
            let Some(record_json) = extract_record_field(fields) else {
                continue;
            };
            if record_ts_matches_date(record_json, date_filter) {
                records.push(record_json.to_string());
            }
        }
    }
    Ok((records, new_last_id))
}

/// Extract the `record` field's string value from an XADD/XREAD/XRANGE
/// flat field-vector. Returns None if absent. Shared by the JSON
/// backfill path (#270 PR-A) and the SSE tail path (#270 PR-B).
fn extract_record_field(fields: &[redis::Value]) -> Option<&str> {
    let mut i = 0;
    while i + 1 < fields.len() {
        let key = redis_value_as_str(&fields[i]);
        if key == Some("record") {
            return redis_value_as_str(&fields[i + 1]);
        }
        i += 2;
    }
    None
}

/// True if the record's `ts` field begins with `date` (YYYY-MM-DD).
/// Matches `flow::ts_utc_now()` format (`YYYY-MM-DDTHH:MM:SSZ`).
fn record_ts_matches_date(record_json: &str, date: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(record_json)
        .ok()
        .and_then(|v| v.get("ts").and_then(|t| t.as_str()).map(String::from))
        .map(|ts| ts.starts_with(date))
        .unwrap_or(false)
}

fn redis_value_as_str(v: &redis::Value) -> Option<&str> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok(),
        redis::Value::SimpleString(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Wait for SIGINT or SIGTERM to trigger graceful shutdown. SIGTERM
/// matters under systemd / Docker / launchd where SIGINT isn't sent.
async fn shutdown_signal() {
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sig) = signal(SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::{fs, path::PathBuf};
    use tower::util::ServiceExt;
    use tempfile::TempDir;

    /// XSS guard, layer 1: the viewer must contain ZERO inline event-handler
    /// attributes. All clicks route through one delegated listener reading
    /// `data-act`/`data-arg` (dataset values are plain strings — no JS-string
    /// context for a malicious flow-record identifier to break out of). A new
    /// `onclick="…"` carrying a record-derived value is exactly the injection
    /// shape this PR removed; fail the build on any reintroduction. JS-side
    /// property assignment (`$("play").onclick=…`) is fine and not matched.
    #[test]
    fn viewer_has_no_inline_event_handlers() {
        let html = include_str!("../assets/viewer.html");
        // Scan the WHOLE inline-handler attribute class, not a fixed name list:
        // an HTML `on<event>=` attribute is leading whitespace, `on`, lowercase
        // letters, optional whitespace, `=`, then a quote. Matches onclick/
        // ontoggle/oninput/onkeydown/onsubmit/… alike, so reintroducing ANY of
        // them fails the build. (`$("x").onclick=` JS property assignment is NOT
        // matched — it has no leading-whitespace `on…="` attribute shape.)
        let bytes = html.as_bytes();
        for (i, w) in bytes.windows(3).enumerate() {
            if matches!(w[0], b' ' | b'\t' | b'\n') && w[1] == b'o' && w[2] == b'n' {
                let mut j = i + 3;
                while j < bytes.len() && bytes[j].is_ascii_lowercase() {
                    j += 1;
                }
                let mut k = j;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
                    k += 1;
                }
                if j > i + 3 && bytes.get(k) == Some(&b'=') {
                    let mut q = k + 1;
                    while q < bytes.len() && matches!(bytes[q], b' ' | b'\t') {
                        q += 1;
                    }
                    if matches!(bytes.get(q), Some(b'"') | Some(b'\'')) {
                        let attr = String::from_utf8_lossy(&bytes[i + 1..j]);
                        panic!(
                            "viewer.html contains inline event handler `{attr}=…` \
                             (byte {i}) — use data-act/data-arg + the delegated \
                             listener instead (XSS hardening)"
                        );
                    }
                }
            }
        }
        // No `javascript:` URL sinks either (an href/src carrying a
        // record-derived scheme would dodge the attribute scan above).
        assert!(
            !html.to_lowercase().contains("javascript:"),
            "viewer.html contains a javascript: URL — not allowed (XSS hardening)"
        );
    }

    /// XSS guard, layer 2: tripwire for raw (unescaped) interpolation of
    /// record-derived values into rendered templates. Curated needles — each
    /// is an exact pattern this hardening pass replaced with an `esc()`-wrapped
    /// form; matching one means an escape was dropped. NOT a proof of safety
    /// (string construction that's escaped downstream is legitimate and
    /// unmatched) — the proof is review + the malicious fixture walkthrough
    /// (tests/fixtures/xss-flow.jsonl).
    #[test]
    fn viewer_has_no_raw_record_interpolations() {
        let html = include_str!("../assets/viewer.html");
        for needle in [
            "${missionLabel()}", "${DATA_SOURCE}", "${nameOf(", "${specOf(",
            "${m}", "${sid}", "${pm}", "${mid}", "${q}", "${a}",
            "${role}", "${handle}", "${model}", "${mission}", "${tag}", "${turns}",
            "${state.machine}", "${state.session}",
            "${s.handle}", "${s.model}", "${s.mission_id}", "${s.role}", "${s.machine}",
            "${n.sid}", "${n.handle}", "${n.model}", "${n.mission}",
            "${r.category}", "${r.machine_id}", "${r.session_id}",
            "${f.k}", "${f.d}", "${f.f}", "${f.s}",
        ] {
            assert!(
                !html.contains(needle),
                "viewer.html interpolates `{needle}` without esc() — wrap it \
                 (record-derived values must be escaped at the template edge)"
            );
        }
    }

    /// (#794) The live SSE tail must be idempotent — a re-delivered record
    /// (reconnect / snapshot-stream overlap; the stream is at-least-once) must
    /// not be re-counted, or cumulative readouts (the savings hero) inflate
    /// past the persisted truth and "reset" on refresh. Lock the dedup so it
    /// can't be removed without this failing.
    #[test]
    fn live_tail_dedups_records() {
        let html = include_str!("../assets/viewer.html");
        assert!(
            html.contains("const SEEN_KEYS=new Set();") && html.contains("const recKey="),
            "viewer.html lost the live-tail dedup primitives (SEEN_KEYS / recKey)"
        );
        // The SSE onmessage handler must consult SEEN_KEYS before RAW.push so a
        // re-delivered record is dropped rather than re-counted.
        let onmsg = html
            .split("LIVE_ES.onmessage")
            .nth(1)
            .expect("viewer.html has no LIVE_ES.onmessage handler");
        let guard = onmsg.find("if(SEEN_KEYS.has(k))return;");
        let push = onmsg.find("RAW.push(rec);");
        assert!(
            matches!((guard, push), (Some(g), Some(p)) if g < p),
            "the live SSE onmessage must guard on SEEN_KEYS.has(k) BEFORE RAW.push(rec) \
             (idempotent append — #794)"
        );
    }

    /// (#803) The savings hero's token-class breakdown + hybrid note. Locks:
    /// (a) the educational class labels exist (generated / fresh / re-read —
    /// the decomposition IS the feature), (b) the hint stays TOKENS-ONLY
    /// (names rate classes, supplies no currency or rate figure), (c) the
    /// hybrid note's record-derived mission id is esc()-wrapped.
    #[test]
    fn savings_hero_breakdown_is_classed_and_currency_free() {
        let html = include_str!("../assets/viewer.html");
        for needle in ["generated", "fresh input", "re-read", "unclassified"] {
            assert!(
                html.contains(needle),
                "savings hero lost the `{needle}` token-class label (#803)"
            );
        }
        assert!(
            html.contains("${esc(mr.mission_id)}"),
            "hybridNote must esc() the mission id"
        );
        assert!(
            !html.contains("${mr.mission_id}"),
            "hybridNote interpolates mission_id without esc()"
        );
        // (#807) The real orchestrator-note channel: the tagged-note lookup
        // must exist, and note text (orchestrator-authored free text) must be
        // esc()-wrapped both on the hero line and in the history modal.
        assert!(
            html.contains("r.source===\"orchestrator\"&&!r.session_id"),
            "viewer lost the orchestrator-note source filter or its mission-level \
             (no session_id) restriction — session-scoped notes are adjudication-\
             shaped and never belong on the card (#807/#819)"
        );
        for (escaped, raw) in [
            ("${esc(last.handle)}", "${last.handle}"),
            ("${esc(n.handle)}", "${n.handle}"),
        ] {
            assert!(
                html.contains(escaped),
                "orchestrator note text lost its esc() wrap (`{escaped}`)"
            );
            assert!(
                !html.contains(raw),
                "orchestrator note text interpolated raw (`{raw}`)"
            );
        }
        assert!(
            html.contains("id=\"nmodalbg\"") && html.contains("data-act=\"notes\""),
            "notes-history modal or its dispatcher hook is missing (#807)"
        );
        // Tokens-only invariant: no currency or rate figure anywhere in the
        // hero region (hybridNote through the end of savingsHero, bounded by
        // the next top-level function) — the operator prices each class
        // themselves. Region-based extraction so a template reshuffle can't
        // silently shrink the scanned text (QA finding on the first cut).
        let start = html
            .find("function hybridNote(")
            .expect("viewer.html lost hybridNote");
        let end = html[start..]
            .find("function renderFleet(")
            .map(|i| start + i)
            .expect("renderFleet no longer follows the hero region");
        let hero = &html[start..end];
        for sym in ["USD", "$0", "$1", "€", "£", "/M", "per million"] {
            assert!(
                !hero.contains(sym),
                "savings hero must not carry a currency/rate figure (`{sym}`) — tokens only"
            );
        }
    }

    /// (#756) The live-diff panel renders model-authored diff text — it must
    /// stay output-encoded at the template edge and must never fetch outside
    /// live mode (playback and the static demo have no daemon). Lock both
    /// invariants in source so a refactor can't quietly drop them.
    #[test]
    fn diff_panel_is_live_gated_and_escaped() {
        let html = include_str!("../assets/viewer.html");
        // The esc() implementation the assertions below depend on must itself
        // exist — if a refactor drops it, every wrapped interpolation throws
        // rather than escaping (QA finding, #756 viewer review).
        assert!(
            html.contains("const esc="),
            "viewer.html lost the esc() helper the output-encoding invariant rests on"
        );
        let panel = html
            .split("function diffPanel(")
            .nth(1)
            .expect("viewer.html lost the #756 diffPanel function");
        assert!(
            panel.contains("if(!document.body.classList.contains('live-mode'))return \"\";"),
            "diffPanel must early-return outside live mode (static demo / playback never fetch)"
        );
        // Every record/daemon-derived string must pass through esc() — the
        // diff body line, the file path chips, and the base ref label.
        for needle in ["${esc(l)", "${esc(f.path)}", "${esc(d.base)}", "${esc(p)}"] {
            assert!(
                panel.contains(needle),
                "diffPanel lost the esc() wrap on `{needle}` — diff content is \
                 model-authored text and must be output-encoded"
            );
        }
        for raw in ["${l}", "${f.path}", "${d.base}", "${d.diff}"] {
            assert!(
                !panel.contains(raw),
                "diffPanel interpolates `{raw}` without esc()"
            );
        }
    }

    /// The XSS regression fixture must stay a valid FLOW_SCHEMA day file —
    /// the manual walkthrough (copy to ~/.darkmux/flows/2026-01-01.jsonl,
    /// `darkmux serve`, open /play/2026-01-01, assert window.__xss is
    /// undefined) only proves anything if the records take the real ingest
    /// path rather than being skipped as malformed.
    #[test]
    fn xss_fixture_is_schema_valid() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/xss-flow.jsonl"
        );
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let mut n = 0;
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            serde_json::from_str::<darkmux_flow::FlowRecord>(line).unwrap_or_else(|e| {
                panic!(
                    "xss-flow.jsonl line {} is not a valid FlowRecord: {e}\n  {line}",
                    i + 1
                )
            });
            n += 1;
        }
        assert!(n >= 10, "xss fixture suspiciously small ({n} records)");
        // The payloads themselves must be present, or the walkthrough is a no-op.
        assert!(raw.contains("window.__xss"), "fixture lost its XSS payloads");
        assert!(raw.contains("onerror="), "fixture lost its HTML-injection payloads");
    }

    /// The darkmux.com/demo dataset is a *real* FLOW_SCHEMA flow file: the demo
    /// page is the canonical viewer in playback mode loading this `.jsonl`,
    /// identical to a local `/play`. Guard that it stays an exact `FlowRecord`
    /// set — if a line drifts off-schema the demo would render wrong (or the
    /// playback path would choke), so it must deserialize line-for-line.
    #[test]
    fn demo_flow_dataset_is_schema_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/demo/demo-flow.jsonl");
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let mut recs = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let r: darkmux_flow::FlowRecord = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("demo-flow.jsonl line {} is not a valid FlowRecord: {e}\n  {line}", i + 1)
            });
            recs.push(r);
        }
        assert!(!recs.is_empty(), "demo dataset is empty");
        // The demo flare is data-driven: the mission is named `demo`, so the
        // viewer's crumb reads `◆ demo` with zero viewer special-casing.
        assert!(
            recs.iter().any(|r| r.mission_id.as_deref() == Some("demo")),
            "demo dataset must carry mission_id=demo (the data-driven demo flare)"
        );
        assert!(
            recs.iter()
                .all(|r| r.mission_id.as_deref().map_or(true, |m| m == "demo")),
            "every mission-scoped demo record must be mission=demo"
        );
        let machines: std::collections::BTreeSet<_> =
            recs.iter().filter_map(|r| r.machine_id.as_deref()).collect();
        assert!(
            machines.len() >= 3,
            "demo should showcase a multi-machine fleet, got {machines:?}"
        );
    }

    #[tokio::test]
    async fn health_returns_200_with_versions() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let bytes = to_bytes(response.into_body(), 1024)
            .await
            .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!json["darkmux_version"].as_str().unwrap().is_empty());
        assert!(!json["flow_schema_version"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn root_serves_viewer_html() {
        // #554: GET / serves the observability viewer (same-origin host
        // for the viewer's /flow/:date fetches).
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("text/html"), "content-type was `{ct}`");

        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let lower = body.to_lowercase();
        assert!(
            lower.contains("<!doctype") || lower.contains("<html"),
            "GET / body is not HTML"
        );
        // #624 follow-up: GET / injects darkmux-mode=live so the viewer's
        // boot() knows to start the SSE tail. No darkmux-date meta on the
        // live route — the viewer falls back to URL-driven targetDate().
        assert!(
            body.contains(r#"<meta name="darkmux-mode" content="live">"#),
            "live meta absent from GET / body"
        );
        assert!(
            !body.contains(r#"<meta name="darkmux-date""#),
            "live route should not inject a date meta"
        );
    }

    #[tokio::test]
    async fn play_serves_viewer_html_with_playback_meta() {
        // #624 follow-up: GET /play/<date> serves the same viewer HTML with
        // darkmux-mode=play + darkmux-date=<date> so boot() skips the SSE
        // tail and stays in scrubber-replay mode for the captured day.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/play/2026-06-03")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"<meta name="darkmux-mode" content="play">"#),
            "play meta absent from GET /play/<date> body"
        );
        assert!(
            body.contains(r#"<meta name="darkmux-date" content="2026-06-03">"#),
            "date meta absent from GET /play/<date> body"
        );
    }

    #[tokio::test]
    async fn play_rejects_malformed_date_with_404() {
        // The handler delegates date validation to `is_valid_date`, which
        // checks SHAPE (10 chars, dashes at positions 4 and 7, digits
        // elsewhere) but not calendar values. "2026-13-99" passes the shape
        // check; the daemon returns 200 + empty viewer for it because the
        // resulting /flow/<date> lookup returns no records — that's
        // acceptable. We test only shape-rejection cases here.
        let app = build_router(PathBuf::new());
        for bad in ["garbage", "26-6-3", "2026/06/03", "2026-06-3", "2026-6-03"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/play/{bad}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "expected 404 for /play/{bad}"
            );
        }
    }

    #[test]
    fn inject_mode_meta_inserts_after_head_open_tag() {
        let html = "<!doctype html><html><head>\n<title>x</title></head><body></body></html>";
        let live = inject_mode_meta(html, "live", None);
        assert!(live.contains(r#"<meta name="darkmux-mode" content="live">"#));
        assert!(!live.contains(r#"<meta name="darkmux-date""#));
        // Order matters: meta must come AFTER <head> open tag, BEFORE <title>
        let head_pos = live.find("<head>").unwrap();
        let meta_pos = live.find(r#"<meta name="darkmux-mode""#).unwrap();
        let title_pos = live.find("<title>").unwrap();
        assert!(head_pos < meta_pos && meta_pos < title_pos);

        let play = inject_mode_meta(html, "play", Some("2026-06-03"));
        assert!(play.contains(r#"<meta name="darkmux-mode" content="play">"#));
        assert!(play.contains(r#"<meta name="darkmux-date" content="2026-06-03">"#));
    }

    #[tokio::test]
    async fn root_route_does_not_shadow_api_routes() {
        // Adding GET / must not collide with the noun-prefixed API routes.
        // `/health` has no external deps, so a clean 200 here proves the
        // router still dispatches the API surface after `/` was added.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "/health regressed after adding GET /");
    }

    #[tokio::test]
    async fn fleet_sessions_live_returns_json_array() {
        // GET /fleet/sessions/live (#638) must always answer 200 with a JSON
        // array — `[]` when Redis is unset/unreachable (CI), the live-session
        // beats when it is. The live viewer keys "running" on this, so it must
        // never 500 on a Redis blip. Asserting "200 + is_array" is robust in
        // both environments (content depends on whether a dispatch is live).
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fleet/sessions/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.is_array(), "live-sessions response must be a JSON array");
    }

    #[tokio::test]
    async fn fleet_machines_live_returns_json_array() {
        // GET /fleet/machines/live (#638) — the same robust contract as the
        // sessions endpoint: always 200 + JSON array (`[]` when Redis is
        // unset/unreachable, the presence beats when it is). The live viewer
        // unions this into machines(), so a Redis blip must degrade to "no
        // live machines", never a 500.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fleet/machines/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.is_array(), "live-machines response must be a JSON array");
    }

    #[tokio::test]
    async fn model_status_returns_200_with_structured_body() {
        // The handler calls into `lms::list_loaded()`, which shells out to
        // the `lms` binary. CI runners don't have it on PATH, so we expect
        // `lms_unreachable: true` and `models: []` rather than a 500. This
        // is the contract the viewer's pill relies on — degraded state
        // shows up as a UI hint, not as a fetch error.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/model/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // Structural assertions — operator-facing fields the viewer reads.
        assert!(json.get("models").is_some(), "missing `models` array");
        assert!(json["models"].is_array());
        assert!(
            json.get("lms_unreachable").is_some(),
            "missing `lms_unreachable` flag"
        );
        assert!(json["lms_unreachable"].is_boolean());
        let generated = json["generated_at_ms"]
            .as_u64()
            .expect("`generated_at_ms` must be a u64 epoch-millis");
        // Sanity: timestamp should be after 2024 and before year 2100 — a
        // wide check, just to ensure it's actually populated.
        assert!(generated > 1_700_000_000_000);
        assert!(generated < 4_000_000_000_000);
    }

    // ─── #275 PR-A: /machine/specs endpoint ──────────────────────────

    /// GET /machine/specs returns a JSON object with the local machine's
    /// versioned spec sheet — version + machine_id + RAM + CPU + OS
    /// + loaded models + redacted Redis URL. Tested at the contract level
    /// rather than the value level so the test doesn't depend on the
    /// machine actually running it.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_returns_versioned_payload() {
        // Defensive — make sure no env from another test bleeds in.
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "expected 200 from /machine/specs");

        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        // Top-level contract — fields are present (values may be null on
        // non-macOS / missing-lms runners, but the keys must exist).
        for key in [
            "darkmux_version",
            "flow_schema_version",
            "machine_id",
            "os",
            "ram_total_bytes",
            "ram_free_for_ai_bytes",
            "cpu_brand",
            "loaded_models",
            "redis_url_redacted",
            "generated_at_ms",
        ] {
            assert!(
                json.get(key).is_some(),
                "/machine/specs response missing key `{key}`: {json}"
            );
        }
        // `darkmux_version` must equal the build's CARGO_PKG_VERSION.
        assert_eq!(
            json["darkmux_version"].as_str(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // `loaded_models` is an array even when lms is unreachable.
        assert!(json["loaded_models"].is_array());
    }

    // ─── #1286: /machine/memory endpoint (the memory-ledger lens feed) ───

    /// GET /machine/memory returns the memory ledger with the fields the
    /// machine lens reads, plus the recorded cadence knob (`cache_ttl_ms`)
    /// and the observer-cost stamp (`gather_ms`) — the #1286 observer-effect
    /// constraints made visible in the payload. `lms` is pointed at a
    /// nonexistent binary so the test NEVER calls the operator's real
    /// LMStudio (and with no ls entries the arch reader opens no files);
    /// probe failures must degrade to `warnings`, never a 500.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_memory_endpoint_returns_ledger_with_observer_stamps() {
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMS_BIN", "/nonexistent/darkmux-serve-test-lms");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/memory")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
        assert_eq!(response.status(), StatusCode::OK, "expected 200 from /machine/memory");
        let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        for key in [
            "schema_version",
            "generated_at_ms",
            "gather_ms",
            "limit_bytes",
            "limit_source",
            "pool",
            "pressure",
            "models",
            "machine",
            "attribution",
            "attribution_note",
            "warnings",
            "cache_ttl_ms",
        ] {
            assert!(
                json.get(key).is_some(),
                "/machine/memory response missing key `{key}`: {json}"
            );
        }
        assert!(json["models"].is_array());
        assert!(json["machine"]["state"].is_string());
        // The nonexistent lms degrades loud — the payload documents it.
        assert!(
            json["warnings"].as_array().is_some_and(|w| !w.is_empty()),
            "missing-lms probes must surface as warnings: {json}"
        );
        assert_eq!(json["cache_ttl_ms"].as_u64(), Some(2_000));
    }

    /// /machine/specs MUST redact the Redis URL's password. Wide-open
    /// redaction is already in place via `flow::RawRedisUrl` (#216); this
    /// test pins that the new endpoint doesn't bypass it.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_redacts_redis_password() {
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", "redis://user:s3cr3t-p4ss@example.com:6379");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&bytes).into_owned();
        // The literal password must NOT appear anywhere in the body.
        assert!(
            !body_str.contains("s3cr3t-p4ss"),
            "Redis password leaked through /machine/specs: {body_str}"
        );
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let redacted = json["redis_url_redacted"]
            .as_str()
            .expect("redis_url_redacted must be a string when DARKMUX_REDIS_URL is set");
        // Sanity: redaction surface should mention example.com (it's the
        // host, not the secret) so the operator can confirm WHICH Redis
        // is being targeted without exposing creds.
        assert!(
            redacted.contains("example.com"),
            "redacted form should keep the host visible: {redacted}"
        );
    }

    /// /machine/specs reports operator-stamped provenance — `machine_id`
    /// from `DARKMUX_MACHINE_ID` (default: hostname). The former
    /// `machine_tier` field was dropped in #590 (machine-capacity tier
    /// no longer routes work).
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_reports_machine_id_from_env() {
        unsafe {
            std::env::set_var("DARKMUX_MACHINE_ID", "test-laptop");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["machine_id"].as_str(),
            Some("test-laptop"),
            "machine_id env not surfaced: {json}"
        );
        assert!(
            json.get("machine_tier").is_none(),
            "machine_tier must be absent post-#590: {json}"
        );
    }

    #[tokio::test]
    async fn flow_returns_empty_array_for_missing_date() {
        // Post-#701: a missing day is an empty JSON array (200), not a 404 —
        // "flow records for date X" is an empty collection, not an absent
        // resource. (The old `.jsonl` route returned 404 here; it's retired.)
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2999-01-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(bytes.as_ref(), b"[]");
    }

    #[tokio::test]
    async fn flow_since_filters_records_inclusively() {
        // (#1173) The live viewer's incremental reconcile passes `?since=<iso-ts>`
        // so a long-open tab stops re-parsing the whole day. Flow ts are uniform
        // second-precision UTC, so the filter is a plain lexical `>=` (inclusive).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-07-03.jsonl"),
            "{\"ts\":\"2026-07-03T00:00:10Z\",\"action\":\"a\",\"session_id\":\"S\"}\n\
             {\"ts\":\"2026-07-03T00:00:20Z\",\"action\":\"b\",\"session_id\":\"S\"}\n\
             {\"ts\":\"2026-07-03T00:00:30Z\",\"action\":\"c\",\"session_id\":\"S\"}\n",
        )
        .unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-07-03?since=2026-07-03T00:00:20Z")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let recs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let actions: Vec<&str> = recs.iter().filter_map(|r| r["action"].as_str()).collect();
        assert_eq!(actions, vec!["b", "c"], "since is inclusive; the earlier record drops");
    }

    #[tokio::test]
    async fn flow_without_since_returns_all_records() {
        // No `?since=` → the whole day, unchanged (backward-compatible).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-07-03.jsonl"),
            "{\"ts\":\"2026-07-03T00:00:10Z\",\"action\":\"a\"}\n\
             {\"ts\":\"2026-07-03T00:00:20Z\",\"action\":\"b\"}\n",
        )
        .unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-07-03")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let recs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recs.len(), 2, "no since → all records returned");
    }

    #[tokio::test]
    async fn flow_days_lists_days_newest_first_with_summaries() {
        // #691: the catalog endpoint scans the flows dir and summarizes each
        // day so the viewer's picker can offer historical playback by date.
        let tmp = TempDir::new().unwrap();
        // Two days; one with a mission + a dispatch, one bare. A schema header
        // line must not count as a record. A non-date file is ignored.
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n\
             {\"action\":\"dispatch.start\",\"handle\":\"coder\",\"session_id\":\"S1\",\"mission_id\":\"demo\"}\n\
             {\"action\":\"dispatch.turn\",\"handle\":\"coder\",\"session_id\":\"S1\",\"mission_id\":\"demo\"}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"action\":\"note\",\"handle\":\"x\"}\n",
        )
        .unwrap();
        fs::write(tmp.path().join("not-a-day.jsonl"), "garbage\n").unwrap();
        fs::write(tmp.path().join("README.txt"), "ignore me\n").unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow-days")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let days = json["days"].as_array().expect("days array");

        assert_eq!(days.len(), 2, "two valid date files (non-date files ignored)");
        // Newest first.
        assert_eq!(days[0]["date"], "2026-05-14");
        assert_eq!(days[1]["date"], "2026-05-12");
        // Summary: schema header excluded → 2 records; mission + 1 dispatch.
        assert_eq!(days[0]["records"], 2);
        assert_eq!(days[0]["dispatches"], 1);
        assert_eq!(days[0]["missions"], serde_json::json!(["demo"]));
        assert_eq!(days[1]["records"], 1);
        assert_eq!(days[1]["dispatches"], 0);
        assert_eq!(days[1]["missions"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn flow_days_empty_dir_is_empty_array() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow-days")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["days"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn flow_missions_rolls_up_a_mission_across_days() {
        // #691: a mission spanning two day files is ONE catalog entry, not two.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n\
             {\"action\":\"dispatch.start\",\"session_id\":\"S1\",\"mission_id\":\"m1\",\"machine_id\":\"studio\",\"ts\":\"2026-05-12T10:00:00Z\"}\n\
             {\"action\":\"dispatch.turn\",\"session_id\":\"S1\",\"mission_id\":\"m1\",\"machine_id\":\"studio\",\"ts\":\"2026-05-12T10:01:00Z\"}\n",
        ).unwrap();
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"action\":\"dispatch.start\",\"session_id\":\"S2\",\"mission_id\":\"m1\",\"machine_id\":\"laptop\",\"ts\":\"2026-05-14T09:00:00Z\"}\n\
             {\"action\":\"dispatch.turn\",\"session_id\":\"S2\",\"mission_id\":\"m1\",\"machine_id\":\"laptop\",\"ts\":\"2026-05-14T09:01:00Z\"}\n\
             {\"action\":\"dispatch.start\",\"session_id\":\"S3\",\"mission_id\":\"m2\",\"machine_id\":\"studio\",\"ts\":\"2026-05-14T09:05:00Z\"}\n",
        ).unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-missions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let missions = json["missions"].as_array().expect("missions array");
        assert_eq!(missions.len(), 2, "two distinct missions");
        let m1 = missions.iter().find(|m| m["mission_id"] == "m1").expect("m1 present");
        assert_eq!(m1["records"], 4, "m1's records span BOTH day files");
        assert_eq!(m1["dispatches"], 2, "S1 + S2");
        assert_eq!(m1["first_date"], "2026-05-12");
        assert_eq!(m1["last_date"], "2026-05-14");
        assert_eq!(m1["first_ts"], "2026-05-12T10:00:00Z");
        assert_eq!(m1["last_ts"], "2026-05-14T09:01:00Z");
        assert_eq!(m1["machines"], serde_json::json!(["laptop", "studio"]));
        // Newest activity first: m2's last dispatch (09:05) is newer than m1's (09:01).
        assert_eq!(missions[0]["mission_id"], "m2");
    }

    #[tokio::test]
    async fn flow_mission_returns_records_across_days_chronologically() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"session_id\":\"S1\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:00:00Z\"}\n",
        ).unwrap();
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"session_id\":\"S2\",\"mission_id\":\"m1\",\"ts\":\"2026-05-14T09:00:00Z\"}\n\
             {\"session_id\":\"S3\",\"mission_id\":\"m2\",\"ts\":\"2026-05-14T09:05:00Z\"}\n",
        ).unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-mission/m1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let records = json["records"].as_array().expect("records array");
        assert_eq!(records.len(), 2, "both m1 records across days; m2 excluded");
        assert_eq!(json["truncated"], false);
        // Chronological: the 05-12 record precedes the 05-14 record.
        assert_eq!(records[0]["ts"], "2026-05-12T10:00:00Z");
        assert_eq!(records[1]["ts"], "2026-05-14T09:00:00Z");
    }

    #[tokio::test]
    async fn flow_session_returns_only_that_sessions_records() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"session_id\":\"S1\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:00:00Z\"}\n\
             {\"session_id\":\"S2\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:05:00Z\"}\n",
        ).unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-session/S1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let records = json["records"].as_array().expect("records array");
        assert_eq!(records.len(), 1, "only S1 (S2 excluded)");
        assert_eq!(records[0]["session_id"], "S1");
    }

    #[tokio::test]
    async fn flow_catalog_rejects_invalid_id() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let long = "a".repeat(200); // > 128-char cap → invalid
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/flow-mission/{long}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn flow_returns_400_for_malformed_date() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        // Non-date shape → rejected by the format validator.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/not-a-date")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // A trailing `.jsonl` is no longer a recognized shape (route retired
        // in #701) — it fails the bare-date validator → 400.
        let response2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-05-14.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response2.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_returns_records_as_json_array_when_present() {
        // The bare-date route reads the local `<date>.jsonl` file (Redis
        // unset) and returns its records as a JSON array.
        // (#811) The config tier is empty by construction in this test build
        // (the test-support dev-dep), so config.json can't supply a Redis URL.
        // The ENV tier is separate: drop DARKMUX_REDIS_URL (serial-guarded so it
        // can't race the sibling tests that set it) so redis_url() is "off" and
        // the handler reads the tmp local file instead of the operator's Redis.
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }
        let tmp = TempDir::new().unwrap();
        let content = "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n{\"action\":\"x\",\"handle\":\"test\"}\n";
        fs::write(tmp.path().join("2026-05-14.jsonl"), content).unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-05-14")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("application/json"), "content-type was `{ct}`");

        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Every non-empty JSON line is parsed (schema header + the record);
        // the viewer's adapter ignores non-record entries downstream.
        let items = arr.as_array().expect("expected a JSON array");
        assert_eq!(items.len(), 2, "schema header line + one record");
        assert!(
            items.iter().any(|r| r["handle"] == "test"),
            "the dispatch record should be present: {arr}"
        );
    }

    /// Localhost-origin requests get CORS headers (topology viewer on any
    /// local port can read the response).
    #[tokio::test]
    /// Post-#270/#273: localhost dev-server origins are NOT allowed by
    /// default. The same browser tab that runs an operator's localhost
    /// app should not be able to exfiltrate fleet-wide flow data via
    /// CORS. Operators with a legitimate dev-server-origin need set
    /// `DARKMUX_DAEMON_CORS_ORIGINS` to opt that origin in.
    ///
    /// `#[serial]` because the sibling `cors_allows_origin_in_env_override`
    /// sets `DARKMUX_DAEMON_CORS_ORIGINS` and the default-deny assertion
    /// races against it under cargo's parallel runner. (Race surfaced
    /// post-merge in CI on main; not flagged on PR-level CI.)
    #[serial_test::serial]
    async fn cors_denies_localhost_origin_by_default() {
        // Defensive — make sure no inherited env from another test changes the default behavior.
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "localhost origin must NOT receive CORS headers by default (#273)"
        );
    }

    /// 127.0.0.1 variant — same as localhost; denied by default post-#273.
    /// `#[serial]` for the same reason as `cors_denies_localhost_origin_by_default`.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_denies_loopback_ip_origin_by_default() {
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "127.0.0.1 origin must NOT receive CORS headers by default (#273)"
        );
    }

    /// Operator-opt-in: `DARKMUX_DAEMON_CORS_ORIGINS` extends the
    /// default null-only allowlist. Comma-separated. Origins in the
    /// list receive CORS headers. (#273)
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_allows_origin_in_env_override() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173,http://localhost:3000",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "origin in DARKMUX_DAEMON_CORS_ORIGINS must receive CORS headers (#273)"
        );
    }

    // ─── (#881) serve daemon auth ─────────────────────────────────────
    // Under test-support the config tier is empty (#811), so a serve token
    // resolves ONLY from the DARKMUX_SERVE_TOKEN env var — set/scrub it,
    // #[serial] to avoid racing other env-mutating tests.

    const TEST_TOKEN: &str = "sek-test-12345";
    fn remote_peer() -> ConnectInfo<SocketAddr> {
        ConnectInfo("10.0.0.9:5555".parse::<SocketAddr>().unwrap())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn diff_requires_token_when_configured() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(Request::builder().uri("/diff/some-session").body(Body::empty()).unwrap())
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        // /diff is gated even on loopback when a token is set.
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn diff_accepts_matching_bearer_token() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/diff/some-session")
                    .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        // The matching token passes the gate (the handler then runs).
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn diff_rejects_wrong_bearer_token() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/diff/some-session")
                    .header("Authorization", "Bearer not-the-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn diff_open_when_no_token_configured() {
        // Default install: no token → /diff is NOT gated (the bundled viewer
        // keeps working on loopback).
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(Request::builder().uri("/diff/some-session").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_open_on_loopback_even_with_token() {
        // The router-wide gate exempts loopback peers; a oneshot request has no
        // ConnectInfo and is treated as loopback.
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(Request::builder().uri("/flow-days").body(Body::empty()).unwrap())
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_requires_token_from_remote_peer() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/flow-days").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// (#1247 Part 3 / #1262 review round) The lab routes must ride the SAME
    /// remote-only bearer gate as the rest of the read surface — guards
    /// against a future refactor special-casing `/lab/*` out of `auth_mw`
    /// (they serve local run artifacts, no less sensitive than flow records).
    #[tokio::test]
    #[serial_test::serial]
    async fn lab_runs_requires_token_from_remote_peer() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/lab/runs").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_accepts_remote_peer_with_token() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder()
            .uri("/flow-days")
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn health_exempt_from_remote_gate() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        // /health must answer even an unauthenticated remote peer (doctor's
        // reachability probe + external liveness checks).
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn bind_gate_allows_loopback_without_token() {
        assert!(bind_requires_token("127.0.0.1", false).is_ok());
        assert!(bind_requires_token("::1", false).is_ok());
        assert!(bind_requires_token("127.0.0.5", false).is_ok());
    }

    #[test]
    fn bind_gate_refuses_nonloopback_without_token() {
        assert!(bind_requires_token("0.0.0.0", false).is_err());
        assert!(bind_requires_token("100.64.0.5", false).is_err()); // a Tailnet IP
        assert!(bind_requires_token("192.168.1.10", false).is_err());
        assert!(bind_requires_token("::", false).is_err()); // IPv6 unspecified
        assert!(bind_requires_token("2001:db8::1", false).is_err()); // global IPv6
    }

    #[test]
    fn bind_gate_allows_nonloopback_with_token() {
        assert!(bind_requires_token("0.0.0.0", true).is_ok());
        assert!(bind_requires_token("100.64.0.5", true).is_ok());
    }

    #[test]
    fn bind_gate_treats_unparseable_as_nonloopback() {
        // A bind string that isn't an IP is conservatively non-loopback.
        assert!(bind_requires_token("garbage", false).is_err());
        assert!(bind_requires_token("garbage", true).is_ok());
    }

    #[test]
    fn tokens_match_is_exact() {
        assert!(tokens_match(b"abc", b"abc"));
        assert!(!tokens_match(b"abc", b"abd"));
        assert!(!tokens_match(b"abc", b"abcd")); // length differs
        assert!(!tokens_match(b"", b"x"));
    }

    // ─── #294 try_send_or_drop_newest ─────────────────────────────────

    /// Bounded channel + producer outpaces consumer → SendOutcome::
    /// Dropped returned; the records that fit make it through. Pre-
    /// #294, the channel was unbounded and would grow indefinitely.
    #[tokio::test]
    async fn try_send_or_drop_newest_drops_when_channel_full() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);

        assert_eq!(
            try_send_or_drop_newest(&tx, "a".to_string()),
            SendOutcome::Sent
        );
        assert_eq!(
            try_send_or_drop_newest(&tx, "b".to_string()),
            SendOutcome::Sent
        );
        // Capacity is 2; third send should drop (newest-dropped).
        assert_eq!(
            try_send_or_drop_newest(&tx, "c".to_string()),
            SendOutcome::Dropped
        );
        assert_eq!(
            try_send_or_drop_newest(&tx, "d".to_string()),
            SendOutcome::Dropped
        );

        // The consumer pulls — gets the records that fit (a, b);
        // c and d were dropped (drop-newest semantics).
        assert_eq!(rx.recv().await, Some("a".to_string()));
        assert_eq!(rx.recv().await, Some("b".to_string()));
    }

    /// Receiver dropped → SendOutcome::Closed returned.
    #[tokio::test]
    async fn try_send_or_drop_newest_signals_closed_on_receiver_drop() {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
        drop(rx); // consumer leaves
        assert_eq!(
            try_send_or_drop_newest(&tx, "post-drop".to_string()),
            SendOutcome::Closed
        );
    }

    /// Consumer that drains keeps up — no drops.
    #[tokio::test]
    async fn try_send_or_drop_newest_no_drops_when_consumer_keeps_up() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);
        for i in 0..10 {
            assert_eq!(
                try_send_or_drop_newest(&tx, format!("r{i}")),
                SendOutcome::Sent
            );
            // Consumer drains immediately after each send.
            let _ = rx.recv().await;
        }
    }

    // ─── #288 normalize trailing-slash + case + reject wildcard ─────────

    /// Operator types `http://localhost:5173/` (trailing slash, what
    /// you copy out of a browser address bar) → must allow the
    /// browser's actual Origin `http://localhost:5173` (no trailing
    /// slash). Pre-#288 this was a silent deny.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_normalizes_trailing_slash_in_origin() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173/", // trailing slash
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173") // no slash
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "trailing slash in env entry should still allow the canonical Origin (#288)"
        );
    }

    /// Operator types `HTTP://Localhost:5173` (uppercase scheme+host)
    /// → must allow the browser's lowercase Origin. Pre-#288 silent
    /// deny.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_normalizes_uppercase_scheme_and_host() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "HTTP://Localhost:5173",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "uppercase scheme+host in env entry should still allow the canonical Origin (#288)"
        );
    }

    /// Literal `*` in the env value is non-functional (browsers don't
    /// send `*` as Origin) — the parser MUST reject it with a stderr
    /// warning so operators who paste a wildcard don't think it works.
    /// The end-to-end behavior: a request with arbitrary external
    /// Origin must still be denied.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_rejects_literal_wildcard() {
        unsafe { std::env::set_var("DARKMUX_DAEMON_CORS_ORIGINS", "*"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "https://malicious.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "literal `*` in env must NOT wildcard-allow arbitrary origins (#288)"
        );
    }

    // ─── parse_cors_origins / normalize_cors_origin unit tests ──────────

    #[test]
    fn normalize_cors_origin_strips_trailing_slash() {
        assert_eq!(
            normalize_cors_origin("http://localhost:5173/"),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_cors_origin("http://localhost:5173"),
            "http://localhost:5173"
        );
    }

    #[test]
    fn normalize_cors_origin_lowercases_scheme_and_host() {
        assert_eq!(
            normalize_cors_origin("HTTP://Localhost:5173"),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_cors_origin("HTTPS://Example.COM"),
            "https://example.com"
        );
    }

    /// Realistic operator paste-from-browser-bar shape: both case +
    /// trailing slash. Each transform tested independently above;
    /// this guards the combined case (which is what operators
    /// actually paste).
    #[test]
    fn normalize_cors_origin_handles_uppercase_and_trailing_slash_together() {
        assert_eq!(
            normalize_cors_origin("HTTP://Localhost:5173/"),
            "http://localhost:5173"
        );
    }

    #[test]
    fn normalize_cors_origin_handles_no_scheme() {
        // Edge case: operator types just `localhost:5173` without scheme.
        // Defensible: lowercase as a string.
        assert_eq!(normalize_cors_origin("Localhost:5173"), "localhost:5173");
    }

    #[test]
    fn parse_cors_origins_filters_empty_and_normalizes() {
        let result = parse_cors_origins("HTTP://Localhost:5173/,, http://other:3000 ,");
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"http://localhost:5173".to_string()));
        assert!(result.contains(&"http://other:3000".to_string()));
    }

    #[test]
    fn parse_cors_origins_rejects_literal_wildcard() {
        let result = parse_cors_origins("*,http://localhost:5173");
        assert_eq!(result, vec!["http://localhost:5173".to_string()]);
        // Wildcard entry filtered; non-wildcard entry preserved + normalized.
    }

    #[test]
    fn parse_cors_origins_empty_returns_empty_vec() {
        assert!(parse_cors_origins("").is_empty());
        assert!(parse_cors_origins("   ").is_empty());
        assert!(parse_cors_origins(",,,").is_empty());
    }

    /// Negative side of the env-override: a localhost port NOT in the
    /// override list still gets denied.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_denies_localhost_port_not_in_env_override() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "localhost port outside the env override list must still be denied (#273)"
        );
    }

    /// `null` origin = topology viewer opened directly from disk (file://).
    #[tokio::test]
    async fn cors_allows_file_protocol_origin() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "null")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "file:// (null) origin must receive CORS headers for the topology viewer"
        );
    }

    /// Arbitrary external origins must NOT get CORS headers — this is the
    /// cross-origin exfiltration guard (#225). The response body is still
    /// returned (CORS is browser-enforced); the missing header causes the
    /// browser to block the script from reading it.
    #[tokio::test]
    async fn cors_denies_external_origin() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "https://malicious.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "external origin must NOT receive CORS headers"
        );
    }

    // ─── SSE / tail_lines tests ─────────────────────────────────────────
    //
    // Tests assert against `tail_lines` (the testable core of `build_tail_stream`)
    // so they verify actual line content. The thin `Event::default().data(...)`
    // wrapper in `build_tail_stream` is small enough that visual review suffices.

    use futures::StreamExt;
    use std::time::Duration;

    /// Pop the next line off a stream within `timeout`. Returns None if
    /// the stream produced nothing in the window.
    async fn next_line<S>(stream: &mut S, timeout: Duration) -> Option<String>
    where
        S: futures::Stream<Item = String> + Unpin,
    {
        tokio::time::timeout(timeout, stream.next()).await.ok().flatten()
    }

    #[tokio::test]
    async fn tail_stream_emits_appended_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::File::create(&path).unwrap();

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        // Append a line after the stream started.
        let written = r#"{"action":"hi"}"#;
        std::fs::write(&path, format!("{written}\n")).unwrap();

        let got = next_line(&mut stream, Duration::from_millis(1500)).await;
        assert_eq!(got.as_deref(), Some(written), "expected appended line verbatim");
    }

    #[tokio::test]
    async fn tail_stream_starts_from_current_eof_not_replay() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::write(&path, "{\"old\":1}\n{\"old\":2}\n{\"old\":3}\n").unwrap();

        let start = std::fs::metadata(&path).unwrap().len();
        let stream = tail_lines(path.clone(), start);
        tokio::pin!(stream);

        // 600ms of polling should yield nothing — old lines are below start_offset.
        let got = next_line(&mut stream, Duration::from_millis(600)).await;
        assert!(got.is_none(), "tail-from-current must not replay history; got {got:?}");

        // Append a new line — only the new line should be emitted.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{{\"new\":1}}").unwrap();
        drop(file);

        let got2 = next_line(&mut stream, Duration::from_millis(1500)).await;
        assert_eq!(got2.as_deref(), Some(r#"{"new":1}"#));
    }

    #[tokio::test]
    async fn tail_stream_handles_missing_file_gracefully() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-yet.jsonl");
        assert!(!path.exists());

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        // Create the file with content after the stream starts polling.
        let path2 = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::fs::write(&path2, "{\"late\":1}\n").unwrap();
        });

        let got = next_line(&mut stream, Duration::from_millis(2000)).await;
        assert_eq!(got.as_deref(), Some(r#"{"late":1}"#));
    }

    #[tokio::test]
    async fn tail_stream_emits_multiple_lines_from_single_append() {
        // Regression: ensure pending VecDeque drains across multiple
        // unfold iterations — three lines in one write should produce
        // three sequential events.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::File::create(&path).unwrap();

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        std::fs::write(&path, "a\nb\nc\n").unwrap();

        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("a")
        );
        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("b")
        );
        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("c")
        );
    }

    #[tokio::test]
    async fn stream_returns_400_for_malformed_date() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/not-a-date/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // The is_addr_reachable / nudge probe tests moved with the helpers to
    // `darkmux_flow::daemon_probe` (#463 cycle-break).

    fn sample_addr() -> std::net::SocketAddr {
        "127.0.0.1:8765".parse().expect("addr parses")
    }

    #[test]
    fn startup_banner_contains_core_info() {
        let flows = PathBuf::from("/tmp/darkmux-flows-banner-test");
        let missions = PathBuf::from("/tmp/darkmux-missions-banner-test");
        let sprints = PathBuf::from("/tmp/darkmux-sprints-banner-test");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &sprints, true, 3, 9, None,
        );

        // Title carries the binary version that operators bump via cargo install.
        let joined = lines.join("\n");
        assert!(joined.contains("darkmux serve · v"), "title line present: {joined}");
        assert!(joined.contains("viewer:"), "viewer line present");
        assert!(
            joined.contains("http://127.0.0.1:8765/"),
            "viewer shows the addr with trailing slash"
        );
        assert!(joined.contains("flow schema:"), "schema line present");
        assert!(joined.contains(darkmux_flow::FLOW_SCHEMA_VERSION), "schema version shown");
        assert!(joined.contains("/tmp/darkmux-flows-banner-test"), "flows dir shown");
        assert!(joined.contains("missions:       3 loaded"), "mission count rendered");
        assert!(joined.contains("sprints:        9 loaded"), "sprint count rendered");
        assert!(joined.contains("ready"), "ready line present");
        assert!(joined.contains("Ctrl-C"), "Ctrl-C hint present");
    }

    #[test]
    fn startup_banner_warns_on_missing_flows_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-missing-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-present-missions");
        let sprints = PathBuf::from("/tmp/darkmux-banner-present-sprints");
        let lines = build_startup_banner(
            &sample_addr(), &flows, false, &missions, true, &sprints, true, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("flows dir doesn't exist yet"),
            "expected flows-dir warning; got: {joined}"
        );
        assert!(
            !joined.contains("missions dir not found"),
            "should not warn about missions when missions dir exists"
        );
        assert!(
            !joined.contains("sprints dir not found"),
            "should not warn about sprints when sprints dir exists"
        );
    }

    #[test]
    fn startup_banner_warns_on_missing_missions_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-present-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-missing-missions");
        let sprints = PathBuf::from("/tmp/darkmux-banner-present-sprints");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, false, &sprints, true, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("missions dir not found"),
            "expected missions warning; got: {joined}"
        );
        assert!(
            joined.contains("/tmp/darkmux-banner-missing-missions"),
            "missions warning should include the path"
        );
        assert!(
            !joined.contains("sprints dir not found"),
            "should not warn about sprints when sprints dir exists"
        );
    }

    #[test]
    fn startup_banner_warns_on_missing_sprints_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-present-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-present-missions");
        let sprints = PathBuf::from("/tmp/darkmux-banner-missing-sprints");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &sprints, false, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("sprints dir not found"),
            "expected sprints warning; got: {joined}"
        );
        assert!(
            joined.contains("/tmp/darkmux-banner-missing-sprints"),
            "sprints warning should include the path"
        );
    }

    #[test]
    fn startup_banner_no_warnings_when_state_is_clean() {
        let flows = PathBuf::from("/some/flows");
        let missions = PathBuf::from("/some/missions");
        let sprints = PathBuf::from("/some/sprints");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &sprints, true, 1, 4, None,
        );
        let joined = lines.join("\n");
        assert!(!joined.contains("doesn't exist yet"), "no flows warning");
        assert!(!joined.contains("missions dir not found"), "no missions warning");
        assert!(!joined.contains("sprints dir not found"), "no sprints warning");
    }

    /// (#1247 Part 3) The banner names whichever lab-dir state is true —
    /// configured (shows the path) or unconfigured (shows the enable hint) —
    /// so the operator never has to wonder why the lab lens is empty.
    #[test]
    fn startup_banner_reports_lab_dir_state() {
        let flows = PathBuf::from("/some/flows");
        let missions = PathBuf::from("/some/missions");
        let sprints = PathBuf::from("/some/sprints");
        let unconfigured = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &sprints, true, 0, 0, None,
        )
        .join("\n");
        assert!(
            unconfigured.contains("lab dir:") && unconfigured.contains("--lab-dir"),
            "expected the enable hint when unconfigured: {unconfigured}"
        );

        let lab = PathBuf::from("/some/lab-runs");
        let configured = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &sprints, true, 0, 0, Some(&lab),
        )
        .join("\n");
        assert!(
            configured.contains("/some/lab-runs"),
            "expected the configured lab dir path: {configured}"
        );
    }

    // ─── #270 Redis aggregation tests ─────────────────────────────────
    //
    // Verify `GET /flow/<date>` (no `.jsonl`) returns a JSON array,
    // aggregating from Redis when `DARKMUX_REDIS_URL` is set + reachable
    // and falling back to the local file otherwise. POSIX-only because
    // the tests spawn a real `redis-server`. Tagged `#[serial]` because
    // they mutate `DARKMUX_REDIS_URL`.

    #[cfg(unix)]
    mod redis_aggregation {
        use super::*;
        use serial_test::serial;
        use std::process::{Child, Command, Stdio};
        use std::time::Instant;

        const REDIS_READY_TIMEOUT: Duration = Duration::from_secs(5);

        fn redis_server_available() -> bool {
            Command::new("redis-server")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        struct RedisFixture {
            child: Child,
            url: String,
        }

        impl Drop for RedisFixture {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        fn spawn_redis() -> RedisFixture {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind ephemeral port");
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            // clippy's zombie-processes lint can't see through the
            // `Drop` impl below that kill+waits the child. Suppress at
            // the spawn site; the Drop guarantees no leaks.
            #[allow(clippy::zombie_processes)]
            let child = Command::new("redis-server")
                .args([
                    "--port", &port.to_string(),
                    "--save", "",
                    "--appendonly", "no",
                    "--bind", "127.0.0.1",
                    "--protected-mode", "no",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("redis-server spawn");

            let url = format!("redis://127.0.0.1:{port}");
            let client = redis::Client::open(url.as_str()).expect("redis client");
            let start = Instant::now();
            while start.elapsed() < REDIS_READY_TIMEOUT {
                if let Ok(mut conn) = client.get_connection() {
                    let ping: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
                    if let Ok(s) = ping {
                        if s == "PONG" {
                            return RedisFixture { child, url };
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("redis-server failed to come ready within {REDIS_READY_TIMEOUT:?}");
        }

        fn xadd_flow_record(url: &str, record_json: &str) {
            let client = redis::Client::open(url).expect("redis client");
            let mut conn = client.get_connection().expect("conn");
            let _: String = redis::cmd("XADD")
                .arg("darkmux:flow")
                .arg("*")
                .arg("schema")
                .arg("1.8.0")
                .arg("record")
                .arg(record_json)
                .query(&mut conn)
                .expect("XADD");
        }

        fn today_utc_date() -> String {
            darkmux_flow::day_utc_now()
        }

        async fn body_as_array(
            response: axum::response::Response,
        ) -> Vec<serde_json::Value> {
            let bytes = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body bytes");
            let body: serde_json::Value = serde_json::from_slice(&bytes)
                .expect("body parses as JSON");
            body.as_array().expect("body is JSON array").clone()
        }

        /// New behavior: GET /flow/<date> (no `.jsonl`) returns a JSON
        /// array sourced from Redis when DARKMUX_REDIS_URL is reachable.
        /// Records from other dates are filtered out.
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_reads_from_redis_when_url_set() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            let today_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"redis-today","machine_id":"laptop"}}"#
            );
            let other_record = r#"{"ts":"2020-01-01T12:00:00Z","action":"redis-other","machine_id":"laptop"}"#;
            xadd_flow_record(&redis.url, &today_record);
            xadd_flow_record(&redis.url, other_record);

            // SAFETY: serial-tagged test owns the env mutation window.
            unsafe { std::env::set_var("DARKMUX_REDIS_URL", &redis.url); }

            let tmp = TempDir::new().unwrap();
            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert_eq!(response.status(), StatusCode::OK, "expected 200");
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("redis-today")),
                "expected `redis-today` record in response: {arr:?}"
            );
            assert!(
                arr.iter().all(|r| r.get("action").and_then(|v| v.as_str()) != Some("redis-other")),
                "expected `redis-other` (other date) filtered out: {arr:?}"
            );
        }

        /// Fallback path: DARKMUX_REDIS_URL set but pointing at an
        /// unreachable endpoint → daemon serves the local file's records
        /// as a JSON array.
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_falls_back_to_file_when_redis_unreachable() {
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let local_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"local-fallback","machine_id":"local"}}"#
            );
            fs::write(
                tmp.path().join(format!("{today}.jsonl")),
                format!("{local_record}\n"),
            )
            .unwrap();

            unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://127.0.0.1:1"); }

            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert_eq!(response.status(), StatusCode::OK);
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("local-fallback")),
                "expected fallback to local file when Redis unreachable: {arr:?}"
            );
        }

        /// Regression: DARKMUX_REDIS_URL unset → local file as JSON array.
        /// (Today this URL shape returns 400 because the handler only
        /// accepts `<date>.jsonl`. Post-fix it returns the array.)
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_reads_local_file_when_redis_url_unset() {
            // (#811) The config tier is empty by construction in this test build
            // (the test-support dev-dep), so an unset DARKMUX_REDIS_URL means
            // Redis off — not the operator's config.json-assembled URL.
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let local_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"local-only","machine_id":"local"}}"#
            );
            fs::write(
                tmp.path().join(format!("{today}.jsonl")),
                format!("{local_record}\n"),
            )
            .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("local-only")),
                "expected local-only record: {arr:?}"
            );
        }

        #[tokio::test]
        async fn flow_file_read_caps_at_max_records_keeping_newest() {
            // (#900) A day-file larger than the cap must yield only the newest
            // MAX_FLOW_FILE_RECORDS (chronological), bounding the daemon's
            // memory — pre-fix the whole file was read + every line parsed.
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let total = MAX_FLOW_FILE_RECORDS + 5;
            let mut buf = String::with_capacity(total * 40);
            for i in 0..total {
                buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":{i}}}"#));
                buf.push('\n');
            }
            fs::write(tmp.path().join(format!("{today}.jsonl")), buf).unwrap();

            let records = read_flow_records_from_file(&today, tmp.path()).await;
            assert_eq!(records.len(), MAX_FLOW_FILE_RECORDS, "must cap at the max");
            // Oldest dropped, newest kept, chronological order preserved.
            assert_eq!(
                records.first().unwrap()["n"].as_u64().unwrap(),
                (total - MAX_FLOW_FILE_RECORDS) as u64
            );
            assert_eq!(records.last().unwrap()["n"].as_u64().unwrap(), (total - 1) as u64);
        }

        #[tokio::test]
        async fn flow_file_read_skips_a_pathological_oversize_line() {
            // (#925) A single line larger than MAX_FLOW_LINE_BYTES (a corrupt
            // or adversarial flows file with no newline) is skipped — drained
            // to its next newline — not read into one ballooning allocation.
            // The surrounding valid records are still returned, and empty /
            // non-JSON lines are tolerated (push_flow_line drops them).
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let mut buf = String::new();
            buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":1}}"#));
            buf.push('\n');
            buf.push('\n'); // empty line — skipped
            buf.push_str("not valid json\n"); // non-JSON — skipped
            buf.push_str(&"x".repeat(MAX_FLOW_LINE_BYTES + 100)); // over-cap — skipped
            buf.push('\n');
            buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":2}}"#));
            buf.push('\n');
            fs::write(tmp.path().join(format!("{today}.jsonl")), buf).unwrap();

            let records = read_flow_records_from_file(&today, tmp.path()).await;
            assert_eq!(records.len(), 2, "oversize/empty/non-JSON lines skipped, valid kept");
            assert_eq!(records[0]["n"].as_u64().unwrap(), 1);
            assert_eq!(records[1]["n"].as_u64().unwrap(), 2);
        }

        // ─── #270 PR-B: redis_tail_lines (SSE Redis tail) ────────────────

        /// XADD'd records after the stream opens are emitted as lines.
        /// Parallel to `tail_stream_emits_appended_lines` for the file-tail
        /// path. Tail-from-now: existing entries in the stream are not
        /// replayed (semantics matches the file-tail handler).
        #[tokio::test]
        #[serial]
        async fn redis_tail_lines_emits_xadded_records_for_today() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            // Pre-existing entry — must NOT be emitted (tail-from-now).
            let pre = format!(
                r#"{{"ts":"{today}T08:00:00Z","action":"pre-existing"}}"#
            );
            xadd_flow_record(&redis.url, &pre);

            let stream = redis_tail_lines(
                redis.url.clone(),
                "darkmux:flow".to_string(),
                today.clone(),
            );
            tokio::pin!(stream);

            // Give the stream a moment to read the current last-id before
            // we XADD the next one.
            tokio::time::sleep(Duration::from_millis(150)).await;

            let post = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"post-open"}}"#
            );
            xadd_flow_record(&redis.url, &post);

            let got = next_line(&mut stream, Duration::from_millis(3000)).await;
            assert!(
                got.as_ref().is_some_and(|s| s.contains("post-open")),
                "expected post-open record to arrive over the tail; got {got:?}"
            );
            assert!(
                !got.as_ref().unwrap().contains("pre-existing"),
                "tail-from-now must NOT replay history; got {got:?}"
            );
        }

        /// Records for OTHER dates are filtered out by the tail.
        #[tokio::test]
        #[serial]
        async fn redis_tail_lines_filters_records_for_other_dates() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            let stream = redis_tail_lines(
                redis.url.clone(),
                "darkmux:flow".to_string(),
                today.clone(),
            );
            tokio::pin!(stream);

            tokio::time::sleep(Duration::from_millis(150)).await;

            // First XADD: other date — should be filtered out.
            xadd_flow_record(
                &redis.url,
                r#"{"ts":"2020-01-01T12:00:00Z","action":"other-date"}"#,
            );
            // Second XADD: today — should be emitted.
            let today_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"matching-date"}}"#
            );
            xadd_flow_record(&redis.url, &today_record);

            // First emit must be the matching-date one — other-date was
            // dropped on the way through.
            let got = next_line(&mut stream, Duration::from_millis(3000)).await;
            assert!(
                got.as_ref().is_some_and(|s| s.contains("matching-date")),
                "expected matching-date record; got {got:?}"
            );
            assert!(
                !got.as_ref().unwrap().contains("other-date"),
                "other-date record must be filtered: {got:?}"
            );
        }

        /// #293 — `redis_tail_lines` against a permanently-unreachable
        /// Redis MUST eventually emit a synthetic `stream.error` record
        /// and exit the spawned task. Pre-fix it retried forever (log
        /// spam + leaked spawned tasks per open SSE connection); the
        /// receiver-drop check only fired on a successful produce,
        /// which never happens on permanent failure.
        ///
        /// Contract: after MAX_CONSECUTIVE_XREAD_FAILURES (10) failures
        /// at 500ms backoff (≈5s wall-clock), the stream sends a
        /// synthetic record with `action: "stream.error"` and the
        /// receiver sees the channel close on its next `.next()`
        /// after that record.
        ///
        /// **Coverage caveat**: this exercises the TCP-refuses-fast
        /// path (127.0.0.1:1). The operator's actual Studio-offline
        /// path is structurally similar (Tailscale-routed peer,
        /// REDIS_CONNECT_TIMEOUT=500ms per connect → also ≈10s total
        /// budget) but takes 2× longer per iteration. Both are
        /// bounded by the 10s test budget, but only the
        /// refused-connect case is DIRECTLY verified here — would
        /// need a netem/tarpit fixture for the silent-timeout case.
        #[tokio::test]
        async fn redis_tail_lines_exits_after_persistent_xread_failures() {
            use futures::StreamExt;
            // 127.0.0.1:1 — privileged port; nothing listens. TCP
            // refuses fast → XREAD errors fast → ~10 failures × 500ms
            // ≈ 5s before the synthetic record + exit.
            let stream = redis_tail_lines(
                "redis://127.0.0.1:1".to_string(),
                "darkmux:flow".to_string(),
                "2026-05-23".to_string(),
            );
            tokio::pin!(stream);

            let read_synthetic = async {
                while let Some(line) = stream.next().await {
                    if line.contains("\"stream.error\"") {
                        return Some(line);
                    }
                }
                None
            };
            // Budget: MAX × 500ms backoff + slack = 10s wall.
            let synthetic = tokio::time::timeout(Duration::from_secs(10), read_synthetic)
                .await
                .expect("redis_tail_lines should emit synthetic + exit within 10s");
            assert!(
                synthetic.is_some(),
                "expected a stream.error synthetic record before the channel closed"
            );
            let line = synthetic.unwrap();
            assert!(
                line.contains("redis_tail_lines"),
                "synthetic record's handle should name the source; got: {line}"
            );
            assert!(
                line.contains("darkmux:flow"),
                "synthetic record's reasoning should name the stream; got: {line}"
            );

            // The next pull should yield None (channel closed because
            // the spawned task returned after emitting the synthetic).
            let post = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("channel close should be observable within 2s of synthetic");
            assert!(
                post.is_none(),
                "channel must close after the synthetic; got: {post:?}"
            );
        }

        /// Handler-level smoke: GET /flow/<date>/stream with Redis URL set
        /// returns 200 + content-type `text/event-stream` and surfaces an
        /// XADD'd record as an SSE `data: ...` line. End-to-end coverage
        /// for the integration path.
        #[tokio::test]
        #[serial]
        async fn flow_stream_handler_uses_redis_when_url_set() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();
            unsafe { std::env::set_var("DARKMUX_REDIS_URL", &redis.url); }

            let tmp = TempDir::new().unwrap();
            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}/stream"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            assert!(
                content_type.starts_with("text/event-stream"),
                "expected SSE content-type; got {content_type}"
            );

            // Pull the SSE body as a stream; XADD; look for the line.
            use futures::StreamExt;
            let mut body = response.into_body().into_data_stream();

            // Give the handler a beat to probe Redis + start XREAD.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"sse-redis-end-to-end"}}"#
            );
            xadd_flow_record(&redis.url, &record);

            // Read up to 3s waiting for the SSE chunk that includes the line.
            let read_fut = async {
                let mut acc = Vec::new();
                while let Some(Ok(chunk)) = body.next().await {
                    acc.extend_from_slice(&chunk);
                    let s = String::from_utf8_lossy(&acc);
                    if s.contains("sse-redis-end-to-end") {
                        return s.into_owned();
                    }
                }
                String::from_utf8_lossy(&acc).into_owned()
            };
            let got = tokio::time::timeout(Duration::from_secs(3), read_fut)
                .await
                .unwrap_or_default();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert!(
                got.contains("sse-redis-end-to-end"),
                "expected XADD'd record to surface as SSE event; got: {got:?}"
            );
        }
    }

    // ─── #756: diff endpoint tests ─────────────────────────────────────

    #[test]
    fn validate_base_ref_accepts_valid_refs() {
        assert!(validate_base_ref("main"));
        assert!(validate_base_ref("origin/main"));
        assert!(validate_base_ref("abc123"));
        assert!(validate_base_ref("v1.0.0"));
        assert!(validate_base_ref("feature/my-branch"));
        assert!(validate_base_ref("a_b.c-d"));
    }

    #[test]
    fn validate_base_ref_rejects_invalid_refs() {
        assert!(!validate_base_ref(""));
        assert!(!validate_base_ref("-rev"));
        assert!(!validate_base_ref("$(x)"));
        assert!(!validate_base_ref("a b")); // space
    }

    #[test]
    fn worktree_contained_accepts_path_under_base() {
        let tmp = TempDir::new().unwrap();
        // Create a real directory structure.
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        assert!(worktree_contained(
            &tmp.path().join("sub"),
            tmp.path(),
        ));
    }

    #[test]
    fn worktree_contained_rejects_path_outside_base() {
        let tmp = TempDir::new().unwrap();
        // A path outside the base dir.
        assert!(
            !worktree_contained(
                StdPath::new("/tmp"),
                tmp.path(),
            )
        );
    }

    #[test]
    fn worktree_contained_rejects_missing_worktree() {
        let tmp = TempDir::new().unwrap();
        // Non-existent worktree path.
        assert!(
            !worktree_contained(
                &tmp.path().join("does_not_exist"),
                tmp.path(),
            )
        );
    }

    #[test]
    fn resolve_session_finds_matching_record() {
        let tmp = TempDir::new().unwrap();
        let record = serde_json::json!({
            "action": "mission.run.start",
            "session_id": "abc123",
            "payload": {
                "worktree": "/tmp/wt",
                "base": "main",
                "branch": "darkmux/abc123"
            }
        });
        fs::write(
            tmp.path().join("2026-01-15.jsonl"),
            format!("{}\n", record),
        )
        .unwrap();

        let result = resolve_session("abc123", tmp.path());
        assert!(result.is_some(), "expected session to resolve");
        let (wt, base, branch) = result.unwrap();
        assert_eq!(wt, "/tmp/wt");
        assert_eq!(base, "main");
        assert_eq!(branch, "darkmux/abc123");
    }

    #[test]
    fn resolve_session_returns_none_for_unknown() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-01-15.jsonl"),
            r#"{"action":"mission.run.start","session_id":"other","payload":{"worktree":"/tmp/wt","base":"main","branch":"x"}}\n"#,
        )
        .unwrap();

        let result = resolve_session("unknown", tmp.path());
        assert!(result.is_none(), "expected no match for unknown session");
    }

    /// A diff larger than the 256KB cap must come back truncated and bounded.
    /// This exercises the streaming read path in `compute_git_diff` (which caps
    /// git's stdout as it reads instead of buffering the whole diff first) and
    /// asserts the returned text never exceeds the cap.
    #[test]
    fn compute_git_diff_caps_a_huge_diff() {
        let wt = TempDir::new().unwrap();
        let dir = wt.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git command");
        };
        git(&["init"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "T"]);
        fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let base = String::from_utf8(
            Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // Commit a file far larger than the 256KB cap, then diff HEAD vs base.
        fs::write(dir.join("big.txt"), "x".repeat(600 * 1024)).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "add big"]);

        let (diff_text, _files, _untracked, truncated) =
            compute_git_diff(dir, &base).expect("compute_git_diff");

        assert!(truncated, "a >256KB diff must be marked truncated");
        assert!(
            diff_text.len() <= 262_144,
            "diff text must be capped at 256KB, was {} bytes",
            diff_text.len()
        );
    }

    /// The other half of the cap contract: a normal sub-cap diff comes back with
    /// the FULL text and `truncated == false` (guards against a cap-logic change
    /// silently flipping small diffs to truncated).
    #[test]
    fn compute_git_diff_small_diff_not_truncated() {
        let wt = TempDir::new().unwrap();
        let dir = wt.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git command");
        };
        git(&["init"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "T"]);
        fs::write(dir.join("f.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let base = String::from_utf8(
            Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        fs::write(dir.join("f.txt"), "two\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "change"]);

        let (diff_text, _files, _untracked, truncated) =
            compute_git_diff(dir, &base).expect("compute_git_diff");
        assert!(!truncated, "a small diff must not be truncated");
        assert!(diff_text.contains("two"), "diff must carry the full change");
    }

    #[tokio::test]
    // (#881) serial: the always-on /diff gate keys on DARKMUX_SERVE_TOKEN, which
    // the serial auth tests set/scrub — without this, that token can leak into
    // this test's build_router window and 401 the request (CI-only race).
    #[serial_test::serial]
    async fn diff_handler_returns_available_true_with_git_diff() {
        // Create a temp dir acting as the worktrees base containing a real git repo.
        let wt_base = TempDir::new().unwrap();

        // Create a real git repo inside the base.
        let wt_dir = wt_base.path().join("myrepo").join("sprint1");
        std::fs::create_dir_all(&wt_dir).unwrap();

        // Initialize git repo.
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["init"])
            .output()
            .expect("git init");

        // Configure git user for commits.
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["config", "user.email", "test@test.com"])
            .output()
            .expect("git config");
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["config", "user.name", "Test"])
            .output()
            .expect("git config");

        // Create and commit a file as base.
        let test_file = wt_dir.join("hello.txt");
        fs::write(&test_file, "original content\n").unwrap();

        Command::new("git")
            .current_dir(&wt_dir)
            .args(["add", "."])
            .output()
            .expect("git add");
        let _commit_out = Command::new("git")
            .current_dir(&wt_dir)
            .args(["commit", "-m", "initial"])
            .output()
            .expect("git commit");
        // Use git rev-parse to get just the hash.
        let base_ref = Command::new("git")
            .current_dir(&wt_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse")
            .stdout;
        let base_ref = String::from_utf8(base_ref).unwrap().trim().to_string();

        // Modify the file and add an untracked one.
        fs::write(&test_file, "modified content\n").unwrap();
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["add", "."])
            .output()
            .expect("git add");
        fs::write(wt_dir.join("new.txt"), "untracked\n").unwrap();

        // Create a flows dir with a day file pointing at this worktree.
        let flows_dir = TempDir::new().unwrap();
        let record = serde_json::json!({
            "action": "mission.run.start",
            "session_id": "test-session-1",
            "payload": {
                "worktree": wt_dir.to_str().unwrap(),
                "base": &base_ref,
                "branch": "darkmux/test"
            }
        });
        fs::write(
            flows_dir.path().join("2026-01-15.jsonl"),
            format!("{}\n", record),
        )
        .unwrap();

        let app = build_router_with_worktrees_base(
            flows_dir.path().to_path_buf(),
            wt_base.path().to_path_buf(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/diff/test-session-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");

        assert!(json["available"].as_bool().unwrap_or(false), "expected available:true");
        assert_eq!(json["session_id"].as_str(), Some("test-session-1"));
        assert!(!json["base"].as_str().unwrap_or("").is_empty());
        assert!(!json["branch"].as_str().unwrap_or("").is_empty());

        // files[] should contain "hello.txt" with additions/deletions.
        let files = json["files"].as_array().expect("expected files array");
        assert!(!files.is_empty(), "expected at least one file in files[]");
        let hello = files.iter().find(|f| f["path"] == "hello.txt");
        assert!(hello.is_some(), "expected hello.txt in files[]");

        // untracked should contain "new.txt".
        let untracked = json["untracked"].as_array().expect("expected untracked array");
        assert!(
            untracked.iter().any(|u| u == "new.txt"),
            "expected new.txt in untracked: {untracked:?}"
        );

        // diff text should contain the change.
        let diff = json["diff"].as_str().unwrap_or("");
        assert!(
            diff.contains("original content") || diff.contains("modified content"),
            "expected diff text to contain file content; got: {diff}"
        );
    }

    #[tokio::test]
    // (#881) serial — see diff_handler_returns_available_true_with_git_diff:
    // the /diff gate keys on DARKMUX_SERVE_TOKEN set by the serial auth tests.
    #[serial_test::serial]
    async fn diff_handler_returns_available_false_for_unknown_session() {
        let flows_dir = TempDir::new().unwrap();
        // No records at all.

        let app = build_router(flows_dir.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/diff/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        assert!(!json["available"].as_bool().unwrap_or(false));
    }

    // ─── #1247 Part 3: lab observer lens ───────────────────────────────────
    //
    // Every fixture below is SYNTHETIC — fabricated field-for-field to match
    // the real `FunnelEnvelope` / `FlowRecord` / `ScoresDoc` shapes (verified
    // against the actual types via `darkmux_lab::lab::{funnel,scores}`), but
    // with invented model names / case ids / crew names. No content from any
    // private corpus.

    /// Write a "finished" synthetic funnel run at `dir`: `funnels.json` (one
    /// envelope), `funnel-events.jsonl` (a couple of records), and
    /// `scores.json` (minimal valid doc). `mtime_offset_secs` lets a test
    /// stagger two runs' mtimes deterministically (rather than racing on
    /// filesystem timestamp resolution) — 0 for "now", a larger value for
    /// "older" by touching the file's mtime backward via `filetime`-free
    /// manual `set_file_mtime` (std has no stable API pre-1.75 filetime
    /// crate; sidestepped by writing older runs FIRST in test bodies instead
    /// — see the callers).
    fn write_synthetic_funnel_run(dir: &StdPath, case_id: &str, crew: &str) {
        fs::create_dir_all(dir).unwrap();
        let funnels = serde_json::json!([{
            "case_id": case_id,
            "crew": crew,
            "mode": "sequential",
            "members": [],
            "steps": [],
            "bundles": 5,
            "raw_flags": 8,
            "deduped_flags": 6,
            "flags": [],
            "judged": [],
            "confirmed": 2,
            "needs_check": 1,
            "archived": 3,
            "fingerprint": {},
            "staffing": {
                "probes": [
                    {"name": "demo-probe", "model": "darkmux:demo-probe-model", "k": 1, "n_ctx": 32768, "max_tokens": 3000}
                ],
                "judge": {"name": "demo-judge", "model": "darkmux:demo-judge-model", "k": 3, "n_ctx": 65536, "max_tokens": 20000}
            }
        }]);
        fs::write(dir.join("funnels.json"), serde_json::to_vec_pretty(&funnels).unwrap()).unwrap();

        let events = format!(
            "{}\n{}\n",
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "funnel.task",
                "handle": crew, "session_id": case_id, "source": "funnel",
                "payload": {"case_id": case_id, "crew": crew, "exec_mode": "sequential", "status": "started", "bundles": 5}
            }),
            serde_json::json!({
                "ts": "2026-01-01T00:05:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "funnel.task",
                "handle": crew, "session_id": case_id, "source": "funnel",
                "payload": {"status": "finished", "confirmed": 2, "needs_check": 1, "archived": 3}
            }),
        );
        fs::write(dir.join("funnel-events.jsonl"), events).unwrap();

        let scores = serde_json::json!({
            "schema_version": "1.0.0",
            "provenance": {
                "run_id": format!("demo-run-{case_id}"),
                "ts": "2026-01-01T00:05:00Z",
                "machine": {"machine_id": "test-machine"},
                "profile": "demo-profile",
                "loaded_drift": false
            },
            "rows": [],
            "crew": crew,
            "exec_mode": "sequential",
            "mode": "funnel",
            "k": "(profile default)"
        });
        fs::write(dir.join("scores.json"), serde_json::to_vec_pretty(&scores).unwrap()).unwrap();
    }

    /// Write a LIVE synthetic run at `dir`: only `funnel-events.jsonl` with a
    /// single "started" `funnel.task` record — no `funnels.json`/
    /// `scores.json` yet, matching a run whose first case hasn't completed.
    fn write_synthetic_live_run(dir: &StdPath, case_id: &str, crew: &str) {
        fs::create_dir_all(dir).unwrap();
        let record = serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "level": "info", "category": "work",
            "tier": "local", "stage": "dispatch", "action": "funnel.task",
            "handle": crew, "session_id": case_id, "source": "funnel",
            "payload": {"case_id": case_id, "crew": crew, "exec_mode": "parallel", "status": "started", "bundles": 12}
        });
        fs::write(dir.join("funnel-events.jsonl"), format!("{record}\n")).unwrap();
    }

    #[test]
    fn resolve_lab_run_dir_accepts_valid_relative_subdir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("case-a/run1")).unwrap();
        let resolved = resolve_lab_run_dir(tmp.path(), "case-a/run1");
        assert_eq!(resolved, Some(tmp.path().join("case-a/run1")));
    }

    #[test]
    fn resolve_lab_run_dir_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "/etc/passwd"), None);
    }

    #[test]
    fn resolve_lab_run_dir_rejects_parent_dir_traversal() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("case-a")).unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "case-a/../../etc"), None);
        assert_eq!(resolve_lab_run_dir(tmp.path(), "../outside"), None);
    }

    #[test]
    fn resolve_lab_run_dir_empty_resolves_to_lab_dir_itself() {
        // QA finding (#1247 PR review round): a run matched AT `lab_dir`'s
        // own root gets `""` as its `dir` — that identifier must round-trip
        // back to `lab_dir`, or such a run is listed but permanently
        // un-drillable (a 400 on every detail/events request).
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), ""), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn resolve_lab_run_dir_rejects_nonexistent_subdir() {
        // worktree_contained canonicalizes both sides; a dir that was never
        // created can't resolve — the endpoint would 400 rather than 500 on
        // a typo'd `dir` param.
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "does-not-exist"), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_lab_run_dir_rejects_symlink_escape() {
        // A symlink INSIDE lab_dir pointing OUTSIDE it must be rejected —
        // the string-shape checks (`..`, absolute) can't catch this; only
        // the canonicalize + prefix check in `worktree_contained` can, since
        // canonicalize resolves the link to its real (outside) target before
        // the prefix comparison runs.
        let lab = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("secret")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), lab.path().join("escape")).unwrap();
        assert_eq!(resolve_lab_run_dir(lab.path(), "escape"), None);
    }

    #[test]
    fn scan_lab_runs_finds_a_finished_run_and_computes_headline_counts() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_funnel_run(&tmp.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1, "expected exactly one run cluster: {runs:?}");
        let r = &runs[0];
        assert_eq!(r.dir, "case-a/run1");
        assert_eq!(r.case_ids, vec!["demo-case-a".to_string()]);
        assert_eq!(r.crew.as_deref(), Some("demo-crew"));
        assert_eq!(r.exec_mode.as_deref(), Some("sequential"));
        assert_eq!(r.bundles, 5);
        assert_eq!(r.raw_flags, 8);
        assert_eq!(r.deduped_flags, 6);
        assert_eq!(r.confirmed, 2);
        assert_eq!(r.needs_check, 1);
        assert_eq!(r.archived, 3);
        assert!(!r.degenerate);
        assert!(r.finished, "scores.json present => finished");
        assert!(r.has_funnels);
        assert!(r.has_events);
        let staffing = r.staffing.as_ref().expect("staffing snapshot present");
        assert_eq!(staffing.probes.len(), 1);
        assert_eq!(staffing.probes[0].model, "darkmux:demo-probe-model");
        assert_eq!(staffing.judge.as_ref().unwrap().model, "darkmux:demo-judge-model");
    }

    #[test]
    fn scan_lab_runs_finds_a_live_run_via_events_prefix_scan() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_live_run(&tmp.path().join("case-b/gate-1"), "demo-case-b", "demo-gate-crew");
        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.dir, "case-b/gate-1");
        assert_eq!(r.case_ids, vec!["demo-case-b".to_string()]);
        assert_eq!(r.crew.as_deref(), Some("demo-gate-crew"));
        assert_eq!(r.exec_mode.as_deref(), Some("parallel"));
        assert!(!r.finished, "no scores.json yet => not finished");
        assert!(!r.has_funnels, "no case has completed yet");
        assert!(r.has_events);
        assert!(r.staffing.is_none(), "no envelope snapshot exists yet");
    }

    #[test]
    fn scan_lab_runs_does_not_descend_into_a_matched_runs_worktrees() {
        // A matched run dir's own `worktrees/<case>` subtree is a full repo
        // checkout that could itself contain marker-file-shaped noise (e.g.
        // a nested fixture) — `LAB_SCAN_SKIP_DIRS` must keep the scan from
        // ever walking into it looking for more runs, unconditionally
        // (independent of whether the parent itself matched — see the next
        // test for why "stop entirely on match" is NOT the mechanism this
        // relies on).
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("probe-27b-527");
        write_synthetic_funnel_run(&run_dir, "demo-case-c", "demo-crew");
        // Plant a decoy inside the run's own worktrees/ subtree that WOULD
        // match the run-cluster criterion if the scanner ever walked there.
        let decoy = run_dir.join("worktrees/demo-case-c/nested");
        fs::create_dir_all(&decoy).unwrap();
        fs::write(decoy.join("scores.json"), "{}").unwrap();

        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1, "the decoy under worktrees/ must not surface as a second run: {runs:?}");
        assert_eq!(runs[0].dir, "probe-27b-527");
    }

    /// Regression (caught live against a real corpus during #1247 manual
    /// verification, NOT in unit tests originally): a stray marker file
    /// left directly at `lab_dir` — e.g. a leftover `funnels.json` from an
    /// earlier ad-hoc `--scores-out` pointed at the corpus root — must not
    /// swallow every run nested beneath it. An earlier version of this scan
    /// returned early the instant ANY directory matched, which made
    /// `lab_dir` itself matching hide the entire tree below it.
    #[test]
    fn scan_lab_runs_a_stray_marker_at_lab_dir_root_does_not_hide_nested_runs() {
        let tmp = TempDir::new().unwrap();
        // A stray funnels.json sitting directly at the scan root — matches
        // the run-cluster criterion on its own, with no sibling scores.json
        // or funnel-events.jsonl (exactly the leftover-artifact shape).
        fs::write(tmp.path().join("funnels.json"), "[]").unwrap();
        write_synthetic_funnel_run(&tmp.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        write_synthetic_funnel_run(&tmp.path().join("case-b/run1"), "demo-case-b", "demo-crew");

        let runs = scan_lab_runs(tmp.path());
        let dirs: Vec<&str> = runs.iter().map(|r| r.dir.as_str()).collect();
        assert!(dirs.contains(&""), "the root's own stray marker still surfaces as a run: {dirs:?}");
        assert!(dirs.contains(&"case-a/run1"), "nested runs must survive a matching root: {dirs:?}");
        assert!(dirs.contains(&"case-b/run1"), "nested runs must survive a matching root: {dirs:?}");
        assert_eq!(runs.len(), 3, "{runs:?}");
    }

    #[test]
    fn scan_lab_runs_respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        // 6 levels deep — past LAB_SCAN_MAX_DEPTH (4) from lab_dir.
        let deep = tmp
            .path()
            .join("a/b/c/d/e/f");
        write_synthetic_funnel_run(&deep, "too-deep", "demo-crew");
        let runs = scan_lab_runs(tmp.path());
        assert!(runs.is_empty(), "a run past the depth cap must not surface: {runs:?}");
    }

    #[test]
    fn scan_lab_runs_multiple_case_dirs_sorted_newest_first() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_funnel_run(&tmp.path().join("older/run1"), "case-older", "crew-x");
        // Force a deterministic newer mtime on the second run's marker files
        // rather than racing filesystem timestamp resolution.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_synthetic_funnel_run(&tmp.path().join("newer/run1"), "case-newer", "crew-x");

        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].dir, "newer/run1", "newest run sorts first: {runs:?}");
        assert_eq!(runs[1].dir, "older/run1");
    }

    #[test]
    fn tail_lab_events_backfills_from_zero_then_returns_only_new_lines() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let first = tail_lab_events(dir, 0);
        assert_eq!(first.lines.len(), 2);
        assert!(!first.finished);
        assert!(first.next_offset > 0);

        // Nothing new yet — same offset, empty backfill.
        let stale = tail_lab_events(dir, first.next_offset);
        assert!(stale.lines.is_empty());
        assert_eq!(stale.next_offset, first.next_offset);

        // Append a third record and re-poll from the returned offset.
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(dir.join("funnel-events.jsonl")).unwrap();
        writeln!(f, "{{\"a\":3}}").unwrap();
        drop(f);

        let second = tail_lab_events(dir, first.next_offset);
        assert_eq!(second.lines.len(), 1, "only the newly-appended line: {second:?}");
        assert_eq!(second.lines[0]["a"], 3);
    }

    #[test]
    fn tail_lab_events_leaves_a_torn_trailing_line_for_the_next_poll() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // A complete line followed by a partial one with NO trailing
        // newline — simulates reading mid-flush.
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n{\"a\":2").unwrap();

        let resp = tail_lab_events(dir, 0);
        assert_eq!(resp.lines.len(), 1, "the torn line must not be parsed yet: {resp:?}");
        assert_eq!(resp.lines[0]["a"], 1);
        // next_offset must land exactly after the complete line's newline,
        // not past the torn trailing bytes.
        assert_eq!(resp.next_offset, "{\"a\":1}\n".len() as u64);
    }

    #[test]
    fn tail_lab_events_reports_finished_once_scores_json_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n").unwrap();
        assert!(!tail_lab_events(dir, 0).finished);
        fs::write(dir.join("scores.json"), "{}").unwrap();
        assert!(tail_lab_events(dir, 0).finished);
    }

    #[test]
    fn tail_lab_events_missing_file_returns_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        let resp = tail_lab_events(tmp.path(), 0);
        assert!(resp.lines.is_empty());
        assert_eq!(resp.next_offset, 0);
        assert!(!resp.finished);
    }

    #[tokio::test]
    async fn lab_runs_handler_reports_unconfigured_when_no_lab_dir() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(Request::builder().uri("/lab/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["configured"], false);
        assert_eq!(json["runs"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn lab_runs_handler_lists_a_configured_runs_directory() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(Request::builder().uri("/lab/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["configured"], true);
        let runs = json["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["dir"], "case-a/run1");
        assert_eq!(runs[0]["crew"], "demo-crew");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_returns_funnels_and_scores() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=case-a%2Frun1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["dir"], "case-a/run1");
        let funnels = json["funnels"].as_array().unwrap();
        assert_eq!(funnels.len(), 1);
        assert_eq!(funnels[0]["case_id"], "demo-case-a");
        assert_eq!(json["scores"]["provenance"]["profile"], "demo-profile");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_resolves_a_run_matched_at_lab_dir_root() {
        // QA finding (#1247 PR review round): `lab_dir` ITSELF matching the
        // run-cluster criteria (an operator pointing `--lab-dir` at one
        // run's folder, or a stray marker left at the root) gets `dir: ""`
        // from `/lab/runs` — that identifier must actually be drillable, not
        // a permanent 400.
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(lab.path(), "demo-case-root", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["funnels"].as_array().unwrap()[0]["case_id"], "demo-case-root");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_rejects_traversal_dir() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=..%2F..%2Fetc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn lab_run_detail_handler_404s_when_not_configured() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=case-a%2Frun1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lab_run_events_handler_backfills_then_deltas_across_two_polls() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        let run_dir = lab.path().join("live/gate-1");
        write_synthetic_live_run(&run_dir, "demo-case-b", "demo-gate-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=live%2Fgate-1&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let bytes = to_bytes(first.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["lines"].as_array().unwrap().len(), 1);
        assert!(!json["finished"].as_bool().unwrap());
        let next_offset = json["next_offset"].as_u64().unwrap();
        assert!(next_offset > 0);

        // Append a second record directly (simulating the driver emitting
        // another step) and re-poll from next_offset.
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("funnel-events.jsonl"))
            .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "ts": "2026-01-01T00:01:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "funnel.step",
                "handle": "demo-gate-crew", "session_id": "demo-case-b", "source": "funnel",
                "payload": {"step_id": "bundle", "kind": "procedural", "status": "finished", "items_in": 1, "items_out": 12, "wall_ms": 0}
            })
        )
        .unwrap();
        drop(f);

        let second = app
            .oneshot(
                Request::builder()
                    .uri(format!("/lab/run/events?dir=live%2Fgate-1&offset={next_offset}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let bytes = to_bytes(second.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let lines = json["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "only the newly-appended record: {lines:?}");
        assert_eq!(lines[0]["action"], "funnel.step");
    }

    #[tokio::test]
    async fn lab_run_events_handler_rejects_traversal_dir() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_live_run(&lab.path().join("live/gate-1"), "demo-case-b", "demo-gate-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=%2Fetc%2Fpasswd&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn lab_run_events_handler_404s_when_not_configured() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=live%2Fgate-1&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
