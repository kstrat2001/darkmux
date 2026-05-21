//! `darkmux serve` — minimal HTTP daemon for flow record retrieval.

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, sse::{Event, KeepAlive, Sse}},
    routing::get,
    Router,
};
use futures::stream::{self, Stream};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

/// Application state shared across handlers.
#[derive(Clone)]
struct AppState {
    flows_dir: PathBuf,
}

/// Validate a path segment as `YYYY-MM-DD.jsonl` format.
fn is_valid_date_jsonl(segment: &str) -> Option<&str> {
    let date = segment.strip_suffix(".jsonl")?;
    is_valid_date(date)
}

/// Validate a bare date string (`YYYY-MM-DD`) without `.jsonl` suffix.
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

/// Build the HTTP router with a configurable flows directory.
pub fn build_router(flows_dir: PathBuf) -> Router {
    let state = AppState { flows_dir };
    Router::new()
        .route("/health", get(health))
        .route("/flow/:date", get(flow_handler))
        .route("/flow/:date/stream", get(flow_stream_handler))
        .route("/flow-status", get(flow_status_handler))
        .route("/model/status", get(model_status_handler))
        .route("/machine/specs", get(machine_specs_handler))
        .route("/missions", get(missions_handler))
        .route("/sprints", get(sprints_handler))
        .layer(local_only_cors())
        .with_state(state)
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

    let extra_origins: Vec<String> = std::env::var("DARKMUX_DAEMON_CORS_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    tower_http::cors::CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, _parts: &axum::http::request::Parts| {
                let s = origin.to_str().unwrap_or("");
                if s == "null" {
                    return true;
                }
                extra_origins.iter().any(|allowed| allowed == s)
            },
        ))
        .allow_methods([Method::GET])
}

/// Default address the local `darkmux serve` daemon binds to. Used by
/// the pre-dispatch reachability nudge (#104 Sprint 3) so an operator
/// running a dispatch with the daemon down sees a single-line heads-up
/// rather than discovering the silence only when they open the viewer.
pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:8765";

/// Probe-budget timeout for the every-dispatch reachability check.
/// Shared between the production hardcoded probe and the test helpers
/// so a future drift doesn't leave the budget assertions and the
/// actual probe disagreeing.
pub const PROBE_TIMEOUT_MS: u64 = 300;

/// Best-effort TCP probe of the local daemon. Returns `true` when a
/// connection can be opened to `DEFAULT_DAEMON_ADDR` within
/// `PROBE_TIMEOUT_MS`. Intentionally lightweight (no HTTP request) —
/// the more thorough `/health` probe lives in
/// `doctor::check_daemon_reachable` and is run on operator-explicit
/// `darkmux doctor` invocation; this helper is for the every-dispatch
/// pre-flight nudge where probe cost matters.
pub fn is_daemon_reachable() -> bool {
    let addr: std::net::SocketAddr = match DEFAULT_DAEMON_ADDR.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    is_addr_reachable(addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS))
}

/// Pure-probe helper: TCP connect with timeout, no `/health` request.
/// Extracted so tests can verify the return-value contract against a
/// known-closed port without depending on the operator's running
/// daemon state (`is_daemon_reachable` hardcodes the address, which
/// would make a return-false assertion brittle in CI where 8765 may
/// or may not be in use).
fn is_addr_reachable(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Print the one-line stderr nudge if the daemon isn't reachable.
/// Non-blocking: the dispatch always proceeds; this is purely
/// situational awareness so an operator who closed the daemon tab
/// last week doesn't lose visibility into a multi-minute dispatch
/// before realizing it.
///
/// `verb_hint` is the verb the operator just ran (e.g. "crew dispatch"
/// or "sprint review"); used in the nudge to make the message
/// context-specific.
pub fn nudge_if_daemon_unreachable(verb_hint: &str) {
    if is_daemon_reachable() {
        return;
    }
    eprintln!(
        "[!] darkmux serve isn't reachable on {} — `{}` will write flow records to disk \
         but you won't see them live. To enable live viewing, run `darkmux serve` in another tab.",
        DEFAULT_DAEMON_ADDR, verb_hint
    );
}

/// Grace period (seconds) between receiving a shutdown signal and
/// force-exiting the process. SSE streams hold connections open
/// indefinitely; axum's graceful shutdown would otherwise block forever
/// waiting for them to drain.
pub const SHUTDOWN_GRACE_SECS: u64 = 3;

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
/// `crate::crew::loader::load_missions`/`load_sprints` in production;
/// tests pass synthetic counts.
fn build_startup_banner(
    addr: &std::net::SocketAddr,
    flows_dir: &std::path::Path,
    flows_dir_exists: bool,
    crew_root: &std::path::Path,
    crew_root_exists: bool,
    mission_count: usize,
    sprint_count: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let version = env!("CARGO_PKG_VERSION");
    let flow_schema = crate::flow::FLOW_SCHEMA_VERSION;

    lines.push(format!("darkmux serve · v{version}"));
    lines.push(format!("  bind:           http://{addr}"));
    lines.push(format!("  flow schema:    {flow_schema}"));
    lines.push(format!("  flows dir:      {}", flows_dir.display()));
    lines.push(format!("  missions:       {mission_count} loaded"));
    lines.push(format!("  sprints:        {sprint_count} loaded"));

    // CORS allowlist surface (#273) — operators with a localhost dev
    // server origin need to opt in via DARKMUX_DAEMON_CORS_ORIGINS;
    // surface the resolved state so failure-to-connect is debuggable.
    let cors_extras = std::env::var("DARKMUX_DAEMON_CORS_ORIGINS").unwrap_or_default();
    let extra_list: Vec<&str> = cors_extras
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if extra_list.is_empty() {
        lines.push(
            "  cors allowlist: null (file://) only — set DARKMUX_DAEMON_CORS_ORIGINS to opt in"
                .to_string(),
        );
    } else {
        lines.push(format!(
            "  cors allowlist: null (file://) + {}",
            extra_list.join(", ")
        ));
    }

    if !flows_dir_exists {
        lines.push(
            "  ! flows dir doesn't exist yet — will be created on first record write".to_string(),
        );
    }
    if !crew_root_exists {
        lines.push(format!(
            "  ! crew root not found at {} (missions/sprints endpoints will return empty)",
            crew_root.display()
        ));
    }

    lines.push("  ready — Ctrl-C to stop".to_string());
    lines
}

/// Start the HTTP daemon, binding on `bind:port`. Blocks until a
/// shutdown signal (SIGINT or SIGTERM) is received. After the signal,
/// axum gets `SHUTDOWN_GRACE_SECS` to drain in-flight connections
/// before the process force-exits — SSE streams to the viewer would
/// otherwise keep the daemon alive forever.
pub fn run(port: u16, bind: String, flows_dir: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let app = build_router(flows_dir.clone());
        let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
        let listener = tokio::net::TcpListener::bind(addr).await?;

        // Banner: print after bind succeeds so we don't claim "listening"
        // before we actually are.
        let flows_dir_exists = flows_dir.exists();
        let crew_root = crate::crew::loader::crew_root();
        let crew_root_exists = crew_root.exists();
        let mission_count = crate::crew::loader::load_missions()
            .map(|v| v.len())
            .unwrap_or(0);
        let sprint_count = crate::crew::loader::load_sprints()
            .map(|v| v.len())
            .unwrap_or(0);
        for line in build_startup_banner(
            &addr,
            &flows_dir,
            flows_dir_exists,
            &crew_root,
            crew_root_exists,
            mission_count,
            sprint_count,
        ) {
            println!("{line}");
        }

        // Spawn the fleet work-queue worker thread (#246 PR-C.2). Runs
        // on a dedicated std::thread (not a tokio task) so the sync
        // redis client + sync crew::dispatch::dispatch don't saturate
        // the tokio executor. Worker self-disables when its
        // prerequisites (DARKMUX_REDIS_URL + DARKMUX_MACHINE_TIER) aren't
        // declared — single-machine fleets continue to work unchanged.
        // The thread runs for the daemon's lifetime; the process
        // force-exit in the SHUTDOWN_GRACE_SECS path kills it cleanly.
        let _worker_handle = crate::fleet::spawn_worker_thread();

        // Shutdown plumbing: multiplex one signal to two consumers
        // (axum's graceful shutdown future and the force-exit timer).
        // `watch::channel` is the right shape — both consumers wait_for
        // the same latch flip.
        let (shutdown_tx, mut shutdown_rx_axum) = tokio::sync::watch::channel(false);
        let mut shutdown_rx_force = shutdown_tx.subscribe();

        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        });

        tokio::spawn(async move {
            let _ = shutdown_rx_force.wait_for(|&v| v).await;
            eprintln!(
                "\ndarkmux serve: shutdown signal received, {SHUTDOWN_GRACE_SECS}s grace for in-flight connections"
            );
            tokio::time::sleep(Duration::from_secs(SHUTDOWN_GRACE_SECS)).await;
            eprintln!(
                "darkmux serve: force exit (open connections — typically SSE streams to the viewer — blocked graceful drain)"
            );
            std::process::exit(0);
        });

        let axum_shutdown = async move {
            let _ = shutdown_rx_axum.wait_for(|&v| v).await;
        };

        axum::serve(listener, app)
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
        "flow_schema_version": crate::flow::FLOW_SCHEMA_VERSION,
    }))
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
    let result = tokio::task::spawn_blocking(crate::crew::loader::load_missions).await;
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
    let result = tokio::task::spawn_blocking(crate::crew::loader::load_sprints).await;
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

/// GET /flow-status — diagnostic snapshot of the flow substrate. Same
/// data shape as `darkmux flow status --json`; the shared shell's
/// store-status pill polls this every 30s. (#170)
///
/// `flow::collect_status()` opens a Redis connection when Redis is
/// configured, so runs on the blocking pool to keep the axum executor
/// free.
async fn flow_status_handler() -> axum::Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(crate::flow::collect_status).await;
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
    let result = tokio::task::spawn_blocking(crate::lms::list_loaded).await;
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

/// GET /machine/specs — local-machine spec sheet for `darkmux fleet
/// status --deep` aggregation. Composes:
/// - identity (machine_id, machine_tier from env)
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
    let lms_result = tokio::task::spawn_blocking(crate::lms::list_loaded).await;
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

    let machine_id = crate::flow::resolve_machine_id();
    let machine_tier = crate::flow::resolve_machine_tier();
    let redis_url_redacted = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|raw| crate::flow::redact_url_creds(&raw));

    let generated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    axum::Json(serde_json::json!({
        "darkmux_version": env!("CARGO_PKG_VERSION"),
        "flow_schema_version": crate::flow::FLOW_SCHEMA_VERSION,
        "machine_id": machine_id,
        "machine_tier": machine_tier,
        "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "ram_total_bytes": ram_total,
        "ram_free_for_ai_bytes": ram_free,
        "cpu_brand": cpu_brand,
        "loaded_models": loaded_models,
        "lms_unreachable": lms_unreachable,
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
    let gb = crate::doctor::reclaimable_gb_for_specs()?;
    let resident_gb = crate::lms::list_loaded()
        .ok()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| crate::eureka::parse_size_gb(&m.size))
                .sum::<f64>()
        })
        .unwrap_or(0.0);
    let safety_gb = crate::doctor::RAM_SAFETY_MARGIN_GB_FOR_SPECS as f64;
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

    // Probe Redis once at request time. If the URL is set AND we can
    // open a connection, use the XREAD path. Otherwise fall back to
    // the file-tail. Probe is cheap (~ms) and bounds the failure mode
    // to "operator misconfigured DARKMUX_REDIS_URL → degraded but
    // serving" rather than 500.
    let redis_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let redis_reachable = if let Some(url) = &redis_url {
        let probe_url = url.clone();
        tokio::task::spawn_blocking(move || {
            redis::Client::open(probe_url.as_str())
                .and_then(|c| c.get_connection().map(|_| ()))
                .is_ok()
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
            let stream_name = std::env::var("DARKMUX_REDIS_STREAM")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "darkmux:flow".to_string());
            Box::pin(
                redis_tail_lines(url, stream_name, date_owned)
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

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// GET /flow/:date — returns flow records for a UTC day.
///
/// Two response shapes, picked by URL suffix:
/// - `<date>.jsonl` → newline-delimited JSON (`application/x-ndjson`),
///   served from the local file. Used by the legacy `/flow` viewer
///   page; behavior unchanged since pre-#270.
/// - `<date>` (no extension) → JSON array (`application/json`). When
///   `DARKMUX_REDIS_URL` is set + reachable, the array is aggregated
///   from Redis (`darkmux:flow` stream, filtered by date) — the
///   **fleet-wide** view across every machine writing to the same
///   stream. When Redis is unconfigured OR unreachable, the array
///   comes from the local `<date>.jsonl` file (#270). The topology
///   viewer's backfill consumes this shape.
async fn flow_handler(
    State(state): State<AppState>,
    Path(segment): Path<String>,
) -> impl IntoResponse {
    // Legacy ndjson path — preserve byte-for-byte for the /flow page.
    if let Some(date) = is_valid_date_jsonl(&segment) {
        let file = state.flows_dir.join(format!("{date}.jsonl"));
        return match tokio::fs::read(&file).await {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/x-ndjson")],
                bytes,
            )
                .into_response(),
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    // JSON-array path — used by the topology viewer. Aggregates from
    // Redis when available; falls back to the local file otherwise.
    let Some(date) = is_valid_date(&segment) else {
        return (StatusCode::BAD_REQUEST, "bad date format").into_response();
    };
    let records = aggregate_flow_records_for_date(date, &state.flows_dir).await;
    axum::Json(records).into_response()
}

/// Aggregate the day's flow records — Redis-when-available, file-otherwise.
///
/// Resolution order:
/// 1. `DARKMUX_REDIS_URL` set + reachable → `XRANGE darkmux:flow - + COUNT N`,
///    parse each entry's `record` field, filter by `record.ts.starts_with(date)`.
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
    let redis_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(url) = redis_url {
        let date_owned = date.to_string();
        let url_owned = url.clone();
        let redis_result = tokio::task::spawn_blocking(move || {
            read_flow_records_from_redis(&url_owned, &date_owned)
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
    let client = redis::Client::open(url)
        .with_context(|| format!("opening Redis client for /flow aggregation: {url}"))?;
    let mut conn = client
        .get_connection()
        .with_context(|| format!("connecting to Redis for /flow aggregation: {url}"))?;
    let stream = std::env::var("DARKMUX_REDIS_STREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "darkmux:flow".to_string());
    // Bounded by DARKMUX_REDIS_MAXLEN at write time (default 10k) — same
    // count as wait_for_completion's XRANGE in fleet.rs:1332.
    let raw: redis::Value = redis::cmd("XRANGE")
        .arg(&stream)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(10000)
        .query(&mut conn)
        .with_context(|| format!("XRANGE on {stream}"))?;
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
    Ok(records)
}

/// Parse `<flows_dir>/<date>.jsonl` into a Vec of JSON values. Missing
/// file = empty Vec (not an error).
async fn read_flow_records_from_file(
    date: &str,
    flows_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    let path = flows_dir.join(format!("{date}.jsonl"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
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
/// errors mid-stream the task backs off 500ms and retries; the
/// consumer sees a quiet stream until Redis returns. The background
/// task exits when the receiver is dropped (channel closes on next
/// send). (#270 PR-B)
fn redis_tail_lines(
    url: String,
    stream_name: String,
    date_filter: String,
) -> impl Stream<Item = String> {
    // Unbounded channel — flow records are small (~1 KB) and
    // bounded by Redis stream MAXLEN upstream. Backpressure on the
    // SSE client side is handled by hyper, not here.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();

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
                    for r in records {
                        if tx.send(r).is_err() {
                            // Receiver dropped — consumer disconnected.
                            return;
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!(
                        "darkmux serve: XREAD on {stream_name} failed ({e}); backing off 500ms"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    eprintln!(
                        "darkmux serve: XREAD blocking task join error ({e}); backing off 500ms"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });

    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
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
    let mut conn = client
        .get_connection()
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
    let mut conn = client
        .get_connection()
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
    /// versioned spec sheet — version + machine_id/tier + RAM + CPU + OS
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
            std::env::remove_var("DARKMUX_MACHINE_TIER");
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
            "machine_tier",
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
    /// from `DARKMUX_MACHINE_ID` (default: hostname) and `machine_tier`
    /// from `DARKMUX_MACHINE_TIER` (no default — surfaces `null` when
    /// unset, which is the contract `flow::resolve_machine_tier` uses).
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_reports_machine_id_and_tier_from_env() {
        unsafe {
            std::env::set_var("DARKMUX_MACHINE_ID", "test-laptop");
            std::env::set_var("DARKMUX_MACHINE_TIER", "inference");
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
            std::env::remove_var("DARKMUX_MACHINE_TIER");
        }
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["machine_id"].as_str(),
            Some("test-laptop"),
            "machine_id env not surfaced: {json}"
        );
        assert_eq!(
            json["machine_tier"].as_str(),
            Some("inference"),
            "machine_tier env not surfaced: {json}"
        );
    }

    #[tokio::test]
    async fn flow_returns_404_for_missing_date() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2999-01-01.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn flow_returns_400_for_malformed_date() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        // Truly malformed — no .jsonl suffix.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/not-a-date.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Valid format but invalid date (month 13) — passes validation,
        // so hits FS layer → 404.
        let response2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-13-99.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response2.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn flow_returns_file_contents_when_present() {
        let tmp = TempDir::new().unwrap();
        let content = "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n{\"action\":\"x\",\"handle\":\"test\"}\n";
        fs::write(tmp.path().join("2026-05-14.jsonl"), content).unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-05-14.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify content-type header.
        let headers = response.headers();
        assert!(headers.contains_key("content-type"));

        let bytes = to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), content.as_bytes());
    }

    /// Localhost-origin requests get CORS headers (topology viewer on any
    /// local port can read the response).
    #[tokio::test]
    /// Post-#270/#273: localhost dev-server origins are NOT allowed by
    /// default. The same browser tab that runs an operator's localhost
    /// app should not be able to exfiltrate fleet-wide flow data via
    /// CORS. Operators with a legitimate dev-server-origin need set
    /// `DARKMUX_DAEMON_CORS_ORIGINS` to opt that origin in.
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
    #[tokio::test]
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

    // ─── is_daemon_reachable / nudge_if_daemon_unreachable (#104 S3) ─────

    /// Listening port reports reachable. Bound on an ephemeral
    /// loopback port so the assertion is deterministic.
    ///
    /// Split into a separate test (formerly one combined assertion with
    /// a drop-and-reprobe second leg, #188) because the drop+reprobe
    /// pattern raced macOS TIME_WAIT semantics: the kernel briefly
    /// kept the just-released port in a state where `connect_timeout`
    /// could still report reachable. Disjoint resources for each
    /// assertion eliminates the race.
    #[test]
    fn is_addr_reachable_returns_true_for_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let open_addr = listener.local_addr().expect("local_addr");
        assert!(is_addr_reachable(open_addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));
        // Listener drops at end of scope — no second probe, no race.
    }

    /// Closed port reports unreachable. Uses port 1 (tcpmux, reserved
    /// in IANA's well-known range; not bound by any process on a normal
    /// system). The connect attempt gets ECONNREFUSED essentially
    /// instantly, well under PROBE_TIMEOUT_MS.
    ///
    /// Picked deliberately over: (a) drop-and-reprobe an ephemeral —
    /// races TIME_WAIT (the #188 flake); (b) an arbitrary high port —
    /// non-zero collision probability with whatever happens to be
    /// running on the test machine.
    #[test]
    fn is_addr_reachable_returns_false_for_closed_port() {
        let closed: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(!is_addr_reachable(closed, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));
    }

    /// Lock the probe budget so a future timeout-doubling slip doesn't
    /// silently make the every-dispatch nudge a noticeable pre-flight tax.
    #[test]
    fn is_addr_reachable_respects_probe_timeout_budget() {
        // Probe a known-unroutable address (TEST-NET-1, RFC 5737) so
        // the timeout path is exercised, not the connect-refused path.
        let dead: std::net::SocketAddr = "192.0.2.1:1".parse().unwrap();
        let timeout = std::time::Duration::from_millis(PROBE_TIMEOUT_MS);
        let start = std::time::Instant::now();
        let result = is_addr_reachable(dead, timeout);
        let elapsed = start.elapsed();

        assert!(!result, "unroutable address must report unreachable");
        // 2x budget gives slack for slow CI without papering over a
        // regression that doubles the timeout (~600ms+ would catch).
        assert!(
            elapsed < std::time::Duration::from_millis(PROBE_TIMEOUT_MS * 2),
            "probe should respect ~{}ms budget, took {:?}",
            PROBE_TIMEOUT_MS,
            elapsed
        );
    }

    #[test]
    fn default_daemon_addr_is_127_0_0_1_8765() {
        // Lock the address — anything else surprises operators reading
        // the nudge stderr line for the first time.
        assert_eq!(DEFAULT_DAEMON_ADDR, "127.0.0.1:8765");
        let parsed: std::net::SocketAddr = DEFAULT_DAEMON_ADDR.parse().expect("must parse");
        assert_eq!(parsed.port(), 8765);
        assert!(parsed.ip().is_loopback());
    }

    fn sample_addr() -> std::net::SocketAddr {
        "127.0.0.1:8765".parse().expect("addr parses")
    }

    #[test]
    fn startup_banner_contains_core_info() {
        let flows = PathBuf::from("/tmp/darkmux-flows-banner-test");
        let crew = PathBuf::from("/tmp/darkmux-crew-banner-test");
        let lines = build_startup_banner(&sample_addr(), &flows, true, &crew, true, 3, 9);

        // Title carries the binary version that operators bump via cargo install.
        let joined = lines.join("\n");
        assert!(joined.contains("darkmux serve · v"), "title line present: {joined}");
        assert!(joined.contains("bind:"), "bind line present");
        assert!(joined.contains("http://127.0.0.1:8765"), "bind shows the addr");
        assert!(joined.contains("flow schema:"), "schema line present");
        assert!(joined.contains(crate::flow::FLOW_SCHEMA_VERSION), "schema version shown");
        assert!(joined.contains("/tmp/darkmux-flows-banner-test"), "flows dir shown");
        assert!(joined.contains("missions:       3 loaded"), "mission count rendered");
        assert!(joined.contains("sprints:        9 loaded"), "sprint count rendered");
        assert!(joined.contains("ready"), "ready line present");
        assert!(joined.contains("Ctrl-C"), "Ctrl-C hint present");
    }

    #[test]
    fn startup_banner_warns_on_missing_flows_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-missing-flows");
        let crew = PathBuf::from("/tmp/darkmux-banner-present-crew");
        let lines = build_startup_banner(&sample_addr(), &flows, false, &crew, true, 0, 0);
        let joined = lines.join("\n");
        assert!(
            joined.contains("flows dir doesn't exist yet"),
            "expected flows-dir warning; got: {joined}"
        );
        assert!(
            !joined.contains("crew root not found"),
            "should not warn about crew when crew root exists"
        );
    }

    #[test]
    fn startup_banner_warns_on_missing_crew_root() {
        let flows = PathBuf::from("/tmp/darkmux-banner-present-flows");
        let crew = PathBuf::from("/tmp/darkmux-banner-missing-crew");
        let lines = build_startup_banner(&sample_addr(), &flows, true, &crew, false, 0, 0);
        let joined = lines.join("\n");
        assert!(
            joined.contains("crew root not found"),
            "expected crew-root warning; got: {joined}"
        );
        assert!(
            joined.contains("/tmp/darkmux-banner-missing-crew"),
            "crew-root warning should include the path"
        );
        assert!(
            !joined.contains("flows dir doesn't exist yet"),
            "should not warn about flows when flows dir exists"
        );
    }

    #[test]
    fn startup_banner_no_warnings_when_state_is_clean() {
        let flows = PathBuf::from("/some/flows");
        let crew = PathBuf::from("/some/crew");
        let lines = build_startup_banner(&sample_addr(), &flows, true, &crew, true, 1, 4);
        let joined = lines.join("\n");
        assert!(!joined.contains("doesn't exist yet"), "no flows warning");
        assert!(!joined.contains("crew root not found"), "no crew warning");
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
            crate::flow::day_utc_now()
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
}
