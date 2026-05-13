//! `darkmux serve` — minimal HTTP daemon for flow record retrieval.

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::path::PathBuf;

/// Application state shared across handlers.
#[derive(Clone)]
struct AppState {
    flows_dir: PathBuf,
}

/// Validate a path segment as `YYYY-MM-DD.jsonl` format.
/// Returns the date string without `.jsonl` suffix if valid, None otherwise.
fn is_valid_date_jsonl(segment: &str) -> Option<&str> {
    let date = segment.strip_suffix(".jsonl")?;
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
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

/// Start the HTTP daemon, binding on `bind:port`. Blocks until SIGINT.
pub fn run(port: u16, bind: String, flows_dir: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let app = build_router(flows_dir);
        let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
        eprintln!("darkmux serve listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
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
}
