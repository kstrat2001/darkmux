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
use darkmux_types::workdir::worktrees_base_dir;
use futures::stream::{self, Stream};
use std::collections::VecDeque;
use std::io::{BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Mission graph builder for `GET /mission/:id/graph.json` (#1284 Packet 5).
/// Kept as its own module (rather than inlined here, unlike most of this
/// crate's small single-file surface) because it owns a real data shape
/// (`GraphNode`/`GraphEdge`/`MissionGraph`) plus a unit-tested pure layering
/// function — see that module's doc for the schema-deviation note
/// (post-#1341, dependency edges are derived from `Task::depends_on`, not a
/// `Step::depends_on` that no longer exists). `pub` (#1402) so the root
/// `darkmux` binary crate can pin `mission_step_kind_display_name`'s static
/// `mission.*` table against `src/mission_run.rs`'s live `StepKind::
/// display_name()` impls — see that constant's own doc for why the table
/// can't be verified from THIS crate (the dependency edge runs the other
/// way).
pub mod mission_graph;

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

/// Mission graph lens page (#1284 Packet 5) — a SEPARATE file from
/// `VIEWER_HTML` by design; see `assets/mission-graph.html`'s own header
/// comment for why the two rendering models (flow-record timeline vs.
/// Phase/Task/Step node-link graph) don't share one file. Served at
/// `GET /mission/:id/graph`.
const MISSION_GRAPH_HTML: &str = include_str!("../assets/mission-graph.html");

/// Vendored React + ReactDOM + reactflow, bundled into one minified IIFE —
/// see `assets/vendor/README.md` for the pinned versions, licenses (all
/// MIT), and rebuild recipe. No CDN, no source map; served same-origin at
/// `GET /vendor/reactflow-bundle.min.js` so the mission-graph page never
/// makes an external network request.
const VENDOR_REACTFLOW_JS: &str = include_str!("../assets/vendor/reactflow-bundle.min.js");

/// reactflow's own stylesheet, minified — see `assets/vendor/README.md`.
/// Served at `GET /vendor/reactflow-bundle.min.css`.
const VENDOR_REACTFLOW_CSS: &str = include_str!("../assets/vendor/reactflow-bundle.min.css");

/// (#1403) Standalone-app shell assets — the operator runs the viewer as a
/// chromeless home-screen shortcut on mobile, so the app icon + web manifest
/// are served same-origin, matching the viewer's self-contained-by-convention
/// posture (no external fetches anywhere; there is no CSP header — XSS
/// defense is output-encoding plus the e2e gate). A dark cyan-diamond
/// monogram. `apple-touch-icon.png` is the iOS home-screen icon (iOS ignores
/// the manifest's `icons`); the 192/512 PNGs feed the manifest for other
/// standalone-capable browsers.
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../assets/apple-touch-icon.png");
const ICON_192_PNG: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512_PNG: &[u8] = include_bytes!("../assets/icon-512.png");

/// (#1403) The web app manifest — `display: standalone` so an installed
/// shortcut opens chromeless. Served at `GET /manifest.webmanifest`. Static
/// (no per-request templating); the icon paths are same-origin routes below.
const WEB_MANIFEST: &str = include_str!("../assets/manifest.webmanifest");

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
/// **Auth wiring (#881, narrowed #1387):** when a bearer token is configured
/// (`darkmux_flow::serve_token_present()`), a **remote-only** gate (`auth_mw`)
/// wraps the whole router — otherwise the router is byte-for-byte today's
/// behavior (zero friction for the loopback-only default install). Loopback
/// peers pass (the operator's own machine + the bundled viewer keep working),
/// non-loopback peers must present the token. `/health` is always exempt
/// (doctor's reachability probe). Layered INNER of CORS so preflight is
/// handled by the CORS layer first.
///
/// Prior to #1387 there was a SECOND, always-on gate wrapping only
/// `/diff/:session_id` (the live-diff endpoint, which shipped worktree source
/// text over HTTP and required the token even on loopback). That route and
/// its dedicated gate are retired — `/worktree-summary/:session_id` is its
/// numbers-only replacement and rides this same general remote-only gate like
/// every other read route; there is no route-specific gate left in this
/// router.
///
/// **`/lab/*` auth (#1247 Part 3):** the lab routes ride the same remote-only
/// gate — read-only local-run artifacts are no more sensitive than flow
/// records, and the lab lens is machine-local by design (never federated).
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
        .route("/machine/status", get(machine_status_handler))
        .route("/machine/specs", get(machine_specs_handler))
        .route("/machine/resources", get(machine_resources_handler))
        .route("/missions", get(missions_handler))
        .route("/phases", get(phases_handler))
        .route("/mission/:id/graph", get(mission_graph_html))
        .route("/mission/:id/graph.json", get(mission_graph_json_handler))
        .route("/vendor/reactflow-bundle.min.js", get(vendor_reactflow_js))
        .route("/vendor/reactflow-bundle.min.css", get(vendor_reactflow_css))
        .route("/manifest.webmanifest", get(web_manifest_handler))
        .route("/apple-touch-icon.png", get(apple_touch_icon_handler))
        .route("/icon-192.png", get(icon_192_handler))
        .route("/icon-512.png", get(icon_512_handler))
        .route("/fleet/sessions/live", get(fleet_sessions_live_handler))
        .route("/fleet/machines/live", get(fleet_machines_live_handler))
        .route("/lab/runs", get(lab_runs_handler))
        .route("/lab/run/detail", get(lab_run_detail_handler))
        .route("/lab/run/events", get(lab_run_events_handler))
        .route("/worktree-summary/:session_id", get(worktree_summary_handler))
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

/// (#881) Remote-only token gate for the whole read surface. Loopback peers
/// pass (the operator's own machine + the bundled same-origin viewer keep
/// working with zero friction); non-loopback peers must present the token.
/// `/health` is always exempt so doctor's reachability probe (and external
/// liveness checks) keep working. A missing `ConnectInfo` (the `oneshot` test
/// path, which has no connection) is treated as loopback.
///
/// **Trust assumption:** "loopback is trusted" holds for darkmux's deployment —
/// bound directly (no reverse proxy) over a Tailscale tailnet that preserves the
/// real peer IP. Behind a connection-terminating reverse proxy every peer would
/// appear loopback and this gate would be bypassed. If darkmux ever grows a
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
configured — the daemon would expose flow records, machine specs, mission state, and worktree \
what-changed summaries of in-flight dispatches to any reachable peer, unauthenticated.\n\
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
/// `mission_count` and `phase_count` are loaded via
/// `darkmux_crew::loader::load_missions`/`load_phases` in production;
/// tests pass synthetic counts.
#[allow(clippy::too_many_arguments)]
fn build_startup_banner(
    addr: &std::net::SocketAddr,
    flows_dir: &std::path::Path,
    flows_dir_exists: bool,
    missions_dir: &std::path::Path,
    missions_dir_exists: bool,
    phases_dir: &std::path::Path,
    phases_dir_exists: bool,
    mission_count: usize,
    phase_count: usize,
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
        "  phases:        {} loaded",
        darkmux_types::style::dim(&phase_count.to_string())
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
            "  auth:           {} (remote reads require a bearer token; loopback open)",
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
    if !phases_dir_exists {
        lines.push(darkmux_types::style::warn(&format!(
            "  ! phases dir not found at {} (/phases endpoint returns empty)",
            phases_dir.display()
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
        let phases_dir = darkmux_crew::loader::phases_dir();
        let phases_dir_exists = phases_dir.exists();
        let mission_count = darkmux_crew::loader::load_missions()
            .map(|v| v.len())
            .unwrap_or(0);
        let phase_count = darkmux_crew::loader::load_phases()
            .map(|v| v.len())
            .unwrap_or(0);
        for line in build_startup_banner(
            &addr,
            &flows_dir,
            flows_dir_exists,
            &missions_dir,
            missions_dir_exists,
            &phases_dir,
            phases_dir_exists,
            mission_count,
            phase_count,
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
        // change without re-checking `auth_mw`.
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

/// Resolve a session_id from flow records: find the LAST `"step result"`
/// record with `payload.kind == "mission.worktree"` matching that session
/// across the two lexicographically last `.jsonl` day files, and extract
/// payload.worktree/base/branch. (#1230 Packet 4: migrated off the retired
/// `mission.run.start` action — the worktree step now emits this generic
/// `"step result"` companion record instead; see `mission_run.rs`'s
/// `emit_step_result` doc.)
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
            if action != "step result" {
                continue;
            }
            let payload = record.get("payload");
            let kind = payload.and_then(|p| p.get("kind")).and_then(|k| k.as_str());
            if kind != Some("mission.worktree") {
                continue;
            }
            let sid = record
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if sid != session_id {
                continue;
            }
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

/// Compute a what-changed SUMMARY for a worktree against its base ref: file
/// count + total additions/deletions, from `git diff --numstat` alone. This
/// is the numbers-only replacement (#1387) for the retired `compute_git_diff`
/// (#756) — it never reads or returns diff TEXT, so worktree source content
/// never leaves the process, let alone the server.
pub fn compute_diff_summary(worktree: &StdPath, base: &str) -> Result<(u64, u64, u64), String> {
    let numstat_out = Command::new("git")
        .current_dir(worktree)
        .args(["diff", "--numstat", base])
        .output()
        .map_err(|_| "git error".to_string())?;
    let numstat_text = String::from_utf8_lossy(&numstat_out.stdout);

    let mut files = 0u64;
    let mut adds = 0u64;
    let mut dels = 0u64;
    for line in numstat_text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        files += 1;
        if let Some(n) = parse_numstat_count(parts[0]) {
            adds += n;
        }
        if let Some(n) = parse_numstat_count(parts[1]) {
            dels += n;
        }
    }
    Ok((files, adds, dels))
}

/// Parse a numstat count field: "-" means binary (null), otherwise parse as u64.
fn parse_numstat_count(s: &str) -> Option<u64> {
    if s == "-" {
        None
    } else {
        s.parse::<u64>().ok()
    }
}

/// Worktree what-changed summary response (#1387). Numbers + the worktree
/// path only — never diff content. `path` is what the viewer renders as the
/// copy/`zed://file/` handoff to the operator's own editor.
#[derive(serde::Serialize)]
pub struct WorktreeSummaryResponse {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dels: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// GET /worktree-summary/:session_id — file count + aggregate adds/dels for
/// a mission-run worktree, plus the worktree path (#1387). Replaces the
/// retired `/diff/:session_id` (#756): that route shipped the worktree's
/// unified diff TEXT over HTTP behind its own always-on auth gate; this one
/// returns numbers + the path only, and rides the general remote-read gate
/// like every other route (see `build_router_full`'s auth-wiring doc).
pub(crate) async fn worktree_summary_handler(
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
            return axum::Json(WorktreeSummaryResponse {
                available: false,
                session_id: None,
                base: None,
                branch: None,
                files: None,
                adds: None,
                dels: None,
                path: None,
                reason: Some("no mission-run record for session".to_string()),
            })
            .into_response();
        }
    };

    // 2. Security: containment check.
    if !worktree_contained(StdPath::new(&worktree), &worktrees_base) {
        return axum::Json(WorktreeSummaryResponse {
            available: false,
            session_id: None,
            base: None,
            branch: None,
            files: None,
            adds: None,
            dels: None,
            path: None,
            reason: Some("worktree outside managed base".to_string()),
        })
        .into_response();
    }

    // 3. Security: base ref validation.
    if !validate_base_ref(&base) {
        return axum::Json(WorktreeSummaryResponse {
            available: false,
            session_id: None,
            base: None,
            branch: None,
            files: None,
            adds: None,
            dels: None,
            path: None,
            reason: Some("invalid base ref".to_string()),
        })
        .into_response();
    }

    // 4. Compute the summary — one git subprocess, offloaded to the blocking
    // pool so a slow/hung git can't starve the async runtime.
    let summary_result = tokio::task::spawn_blocking({
        let worktree = worktree.clone();
        let base = base.clone();
        move || compute_diff_summary(StdPath::new(&worktree), &base)
    })
    .await
    .unwrap_or_else(|_| Err("worktree summary task panicked".to_string()));
    let (files, adds, dels) = match summary_result {
        Ok(r) => r,
        Err(reason) => {
            return axum::Json(WorktreeSummaryResponse {
                available: false,
                session_id: None,
                base: None,
                branch: None,
                files: None,
                adds: None,
                dels: None,
                path: None,
                reason: Some(reason),
            })
            .into_response();
        }
    };

    axum::Json(WorktreeSummaryResponse {
        available: true,
        session_id: Some(session_id),
        base: Some(base),
        branch: Some(branch),
        files: Some(files),
        adds: Some(adds),
        dels: Some(dels),
        path: Some(worktree),
        reason: None,
    })
    .into_response()
}

/// GET /machine/status — returns currently-loaded models (per `lms ps
/// --json`) as JSON. (#1426 — renamed from `/model/status` alongside the
/// `machine` CLI family; one route family, one name. Its consumer is the
/// `darkmux machine status <id>` peer read — the viewer does not fetch this
/// route; an earlier doc comment claiming a viewer toolbar-pill consumer,
/// from #87, was stale.)
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
/// durations and the phase-progress widget. Empty array on no missions
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

/// GET /phases — list of all phases from the JSON source-of-truth
/// (`~/.darkmux/crew/phases/`). Includes status + transition timestamps
/// (started_ts/completed_ts/abandoned_ts) so the viewer's wall-clock
/// graphic can render Running phases' live elapsed time + Complete
/// phases' frozen durations. Empty array on no phases; never errors.
async fn phases_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(darkmux_crew::loader::load_phases).await;
    let phases = match result {
        Ok(Ok(s)) => s,
        _ => Vec::new(),
    };
    axum::Json(serde_json::json!({
        "phases": phases,
        "generated_at_ms": current_millis(),
    }))
}

// ─── Mission graph lens (#1284 Packet 5) ────────────────────────────────

/// `GET /mission/:id/graph` — the dedicated mission-graph HTML page. Static
/// bytes (no per-mission templating; the id is already in the URL the
/// browser loaded, and the page's own JS reads it from `location.pathname`
/// — see `assets/mission-graph.html`'s `missionIdFromPath()`). Never 404s
/// on an unknown mission id here — that's `graph.json`'s job; the page
/// itself always loads and reports the fetch error inline (item 6's
/// graceful-degradation posture extends to "id doesn't exist" too, not just
/// "mission has no task/step data").
async fn mission_graph_html() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        MISSION_GRAPH_HTML,
    )
}

/// `GET /mission/:id/graph.json` — the node/edge snapshot
/// `mission_graph::build_mission_graph` builds from the persisted
/// Phase/Task/Step JSON. `404` when no mission with this id exists;
/// otherwise always `200` — a mission with phases but no task/step graph
/// still returns a (phases-only) graph with `legacy: true` + a `note`,
/// never an error (item 6).
async fn mission_graph_json_handler(Path(id): Path<String>) -> axum::response::Response {
    if !is_valid_catalog_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid mission id (alphanumeric + -_.: , <=128 chars)",
        )
            .into_response();
    }
    let id_owned = id.clone();
    let result =
        tokio::task::spawn_blocking(move || mission_graph::build_mission_graph(&id_owned)).await;
    match result {
        Ok(Ok(Some(graph))) => axum::Json(graph).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            format!("no mission with id `{id}` found\n"),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to build mission graph: {e:#}\n"),
        )
            .into_response(),
        Err(_join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "mission graph builder task panicked\n",
        )
            .into_response(),
    }
}

/// `GET /vendor/reactflow-bundle.min.js` — same-origin vendored bundle
/// (see `assets/vendor/README.md`). Long `Cache-Control` is safe: the
/// bundle is content-addressed by the daemon's own build (a new darkmux
/// version ships a new bundle at a byte-identical URL, but daemons are
/// short-lived personal processes, not a CDN-fronted deploy — a stale
/// browser cache is cleared by a normal page reload after upgrade, matching
/// how `VIEWER_HTML` itself is served with no special caching headers
/// either way).
async fn vendor_reactflow_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/javascript; charset=utf-8")],
        VENDOR_REACTFLOW_JS,
    )
}

/// `GET /vendor/reactflow-bundle.min.css` — see `vendor_reactflow_js`'s doc.
async fn vendor_reactflow_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/css; charset=utf-8")],
        VENDOR_REACTFLOW_CSS,
    )
}

/// (#1403) `GET /manifest.webmanifest` — the standalone-app manifest so a
/// home-screen shortcut opens chromeless (`display: standalone`). Same
/// self-contained, no-caching posture as the viewer HTML itself.
async fn web_manifest_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/manifest+json; charset=utf-8")],
        WEB_MANIFEST,
    )
}

/// (#1403) `GET /apple-touch-icon.png` — the iOS home-screen icon for the
/// standalone shortcut (iOS reads this link, not the manifest's `icons`).
async fn apple_touch_icon_handler() -> impl IntoResponse {
    png_response(APPLE_TOUCH_ICON_PNG)
}

/// (#1403) `GET /icon-192.png` — manifest icon for non-iOS standalone browsers.
async fn icon_192_handler() -> impl IntoResponse {
    png_response(ICON_192_PNG)
}

/// (#1403) `GET /icon-512.png` — manifest icon (also the maskable source).
async fn icon_512_handler() -> impl IntoResponse {
    png_response(ICON_512_PNG)
}

/// Shared PNG response builder for the standalone-shell icons (#1403). A long
/// `Cache-Control` is safe for the same reason the vendored bundle uses one —
/// the icon is content-addressed by the daemon build, and a stale browser
/// cache clears on a normal reload after upgrade.
fn png_response(bytes: &'static [u8]) -> axum::response::Response {
    (
        StatusCode::OK,
        [
            ("content-type", "image/png"),
            ("cache-control", "public, max-age=86400"),
        ],
        bytes,
    )
        .into_response()
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
    staffing: Option<darkmux_lab::lab::review::StaffingSnapshot>,
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
            if let Ok(envs) = serde_json::from_str::<Vec<darkmux_lab::lab::review::ReviewEnvelope>>(&text) {
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

    // (#1434) A brand-new live run with ONLY the events file (no case has
    // completed, so `funnels.json` doesn't exist) leaves case_ids empty until
    // its first case lands. The former best-effort prefix scan keyed on the
    // retired per-run task-started record, which the graph path no longer
    // emits — so it matched nothing in current output and was removed rather
    // than ported (the graph's `step result` companions carry no equivalent
    // case/crew/exec_mode-on-start payload). The run card simply shows minimal
    // metadata until the first case completes.

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


#[derive(serde::Deserialize)]
struct LabDirQuery {
    dir: String,
}

#[derive(serde::Serialize, Default)]
struct LabRunDetailResponse {
    dir: String,
    funnels: Vec<darkmux_lab::lab::review::ReviewEnvelope>,
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
        let funnels: Vec<darkmux_lab::lab::review::ReviewEnvelope> =
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

async fn machine_status_handler() -> axum::Json<serde_json::Value> {
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

/// (#1286) GET /machine/resources cache TTL. Computed on request with a short
/// in-process cache so a phone-viewer poll burst costs ONE gather; the TTL
/// is recorded in the payload (`cache_ttl_ms`) per the observer-effect
/// constraint — cadence is a visible knob, never adaptive-silent.
const MACHINE_RESOURCES_CACHE_TTL: Duration = Duration::from_secs(2);
static MACHINE_RESOURCES_CACHE: std::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>> =
    std::sync::Mutex::new(None);

/// Single-flight gate for the machine-resources gather (#1286): concurrent
/// cold-cache requests serialize on this async lock so a phone-viewer poll
/// burst costs exactly ONE gather, not one per request (the shell-out gather
/// is the observer-cost the ledger is built to keep negligible). See the
/// double-checked flow in [`machine_resources_handler`].
fn machine_resources_gather_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Fresh cached machine-resources payload, or `None` when absent/stale. The std
/// mutex is held only for the clone — never across an `.await`.
fn machine_resources_cached_fresh() -> Option<serde_json::Value> {
    let guard = MACHINE_RESOURCES_CACHE.lock().ok()?;
    let (at, v) = guard.as_ref()?;
    (at.elapsed() < MACHINE_RESOURCES_CACHE_TTL).then(|| v.clone())
}

/// GET /machine/resources (#1286) — the live machine-scope memory ledger:
/// resident models with potential-vs-current + color state, pool/limit
/// lines, and the unified-memory pressure rows (swap / compressor /
/// memory-pressure free%). Exactly `darkmux machine resources --json`, served —
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
async fn machine_resources_handler() -> axum::Json<serde_json::Value> {
    // Fast path: a fresh cache serves without touching the gather lock.
    if let Some(v) = machine_resources_cached_fresh() {
        return axum::Json(v);
    }
    // Single-flight (#1286): serialize cold-cache refreshers on the gather
    // lock, then RE-CHECK the cache under it — a request that waited behind an
    // in-flight gather returns that gather's fresh result instead of launching
    // a second one, so a poll burst costs one gather.
    let _permit = machine_resources_gather_lock().lock().await;
    if let Some(v) = machine_resources_cached_fresh() {
        return axum::Json(v);
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
            serde_json::json!(MACHINE_RESOURCES_CACHE_TTL.as_millis() as u64),
        );
    }
    if let Ok(mut guard) = MACHINE_RESOURCES_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), value.clone()));
    }
    axum::Json(value)
}

/// GET /machine/specs — local-machine spec sheet for `darkmux machine list
/// --deep` aggregation. Composes:
/// - identity (machine_id from env)
/// - hardware (ram_total_bytes, ram_free_for_ai_bytes, cpu_brand, os)
/// - software (darkmux_version, flow_schema_version)
/// - state (loaded_models from lms ps; redacted Redis URL from env)
///
/// All fields are best-effort: a missing sysctl, an unreachable `lms`,
/// or an unset env var degrades to `null` (or `[]` / `0` for typed
/// fields) rather than 500-ing. This is the contract `machine list
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
                .filter_map(|m| darkmux_types::size::parse_size_gb(&m.size))
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
#[path = "lib_tests.rs"]
mod tests;
