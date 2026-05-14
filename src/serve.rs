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
        .route("/model/status", get(model_status_handler))
        .route("/missions", get(missions_handler))
        .route("/sprints", get(sprints_handler))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
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
/// waiting for them to drain. Exposed as a const so the integration
/// test can assert against it.
pub const SHUTDOWN_GRACE_SECS: u64 = 3;

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

/// GET /flow/:date.jsonl — returns file contents for validated dates.
async fn flow_handler(
    State(state): State<AppState>,
    Path(segment): Path<String>,
) -> impl IntoResponse {
    let Some(date) = is_valid_date_jsonl(&segment) else {
        return (StatusCode::BAD_REQUEST, "bad date format").into_response();
    };
    let file = state.flows_dir.join(format!("{date}.jsonl"));
    match tokio::fs::read(&file).await {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/x-ndjson")],
            bytes,
        ).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
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

    #[tokio::test]
    async fn cors_headers_present_on_responses() {
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

        let headers = response.headers();
        assert!(headers.contains_key("access-control-allow-origin"));
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

    /// Test the actual return-value contract by binding a transient
    /// listener (so its port IS reachable) and probing both that port
    /// and a definitely-closed port. Doesn't depend on operator's
    /// running daemon state at 127.0.0.1:8765.
    #[test]
    fn is_addr_reachable_returns_true_for_listening_port_false_for_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let open_addr = listener.local_addr().expect("local_addr");

        // Open port: reachable.
        assert!(is_addr_reachable(open_addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));

        // Close it; same address should now be unreachable.
        drop(listener);
        // OS may take a moment to release; the connect_timeout still
        // either reports refused (fast) or times out within budget.
        assert!(!is_addr_reachable(open_addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));
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

    #[test]
    fn shutdown_grace_secs_is_short_enough_to_feel_responsive() {
        // Operators expect Ctrl-C to feel like Ctrl-C. Anything beyond
        // ~5s and people start hammering it / killing the PID by hand,
        // which is the exact pain #121 was filed to fix.
        assert!(
            SHUTDOWN_GRACE_SECS <= 5,
            "grace period {SHUTDOWN_GRACE_SECS}s is too long — operators will fall back to kill <pid>"
        );
        assert!(
            SHUTDOWN_GRACE_SECS >= 1,
            "grace period {SHUTDOWN_GRACE_SECS}s is too short — clean disconnects deserve a beat"
        );
    }
}
