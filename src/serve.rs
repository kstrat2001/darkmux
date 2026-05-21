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
        .route("/missions", get(missions_handler))
        .route("/sprints", get(sprints_handler))
        .layer(local_only_cors())
        .with_state(state)
}

/// CORS layer permitting only localhost-originating browser requests
/// (#225). Prevents cross-origin exfiltration of flow records (which
/// include `payload.reasoning_text` and crew structure) by arbitrary
/// web pages.
///
/// Allowed origins:
/// - `null`                  — `file://` pages (topology viewer from disk)
/// - `http://localhost*`     — any local dev server port
/// - `http://127.0.0.1*`    — explicit loopback
///
/// Non-browser clients (curl, darkmux CLI, probe) are unaffected — CORS
/// is browser-enforced; the header simply isn't set for unmatched origins
/// and the response is returned normally.
fn local_only_cors() -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderValue, Method};
    use tower_http::cors::AllowOrigin;
    tower_http::cors::CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            |origin: &HeaderValue, _parts: &axum::http::request::Parts| {
                let s = origin.to_str().unwrap_or("");
                s == "null"
                    || s.starts_with("http://localhost")
                    || s.starts_with("http://127.0.0.1")
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

/// GET /flow/:date/stream — streams new records appended to the file as SSE events.
/// Tail-from-current: never replays history.
async fn flow_stream_handler(
    State(state): State<AppState>,
    Path(date_raw): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, (StatusCode, &'static str)> {
    let Some(date) = is_valid_date(&date_raw) else {
        return Err((StatusCode::BAD_REQUEST, "bad date format"));
    };
    let path = state.flows_dir.join(format!("{date}.jsonl"));
    // Tail-from-current: start at the file's current EOF so existing
    // history isn't replayed. Treat missing file as offset 0 (file will
    // appear later; the poll loop picks it up).
    let start_offset = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let stream = build_tail_stream(path, start_offset);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
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
        let mut record_json: Option<&str> = None;
        let mut i = 0;
        while i + 1 < fields.len() {
            let key = match &fields[i] {
                redis::Value::BulkString(b) => std::str::from_utf8(b).ok(),
                redis::Value::SimpleString(s) => Some(s.as_str()),
                _ => None,
            };
            if key == Some("record") {
                record_json = match &fields[i + 1] {
                    redis::Value::BulkString(b) => std::str::from_utf8(b).ok(),
                    redis::Value::SimpleString(s) => Some(s.as_str()),
                    _ => None,
                };
                break;
            }
            i += 2;
        }
        let Some(json) = record_json else { continue };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        // Filter by record.ts prefix — `YYYY-MM-DDTHH:MM:SSZ` (see
        // flow::ts_utc_now). Records without a parseable ts are dropped
        // because they can't be assigned to a UTC day reliably.
        let matches_date = parsed
            .get("ts")
            .and_then(|v| v.as_str())
            .map(|ts| ts.starts_with(date))
            .unwrap_or(false);
        if matches_date {
            records.push(parsed);
        }
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
    async fn cors_allows_localhost_origin() {
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
            response.headers().contains_key("access-control-allow-origin"),
            "localhost origin must receive CORS headers"
        );
    }

    /// 127.0.0.1 variant — same localhost, explicit IP form.
    #[tokio::test]
    async fn cors_allows_loopback_ip_origin() {
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
            response.headers().contains_key("access-control-allow-origin"),
            "127.0.0.1 origin must receive CORS headers"
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
    }
}
