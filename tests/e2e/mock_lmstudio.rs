//! In-process mock LMStudio HTTP server for e2e testing.
//!
//! Binds to a random free port; responds to `POST /v1/chat/completions`
//! with a canned response. Lets cross-machine dispatch tests run
//! without needing a real LMStudio + model on the machine.
//!
//! ## Shape of canned responses
//!
//! Returns the OpenAI chat-completions response shape darkmux expects:
//!
//! ```json
//! {
//!   "id": "mock-<n>",
//!   "object": "chat.completion",
//!   "created": <unix-ms>,
//!   "model": "mock-lmstudio",
//!   "choices": [{"index": 0, "message": {"role": "assistant", "content": "<canned>"}, "finish_reason": "stop"}],
//!   "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
//! }
//! ```
//!
//! The `content` defaults to `MOCK_LMSTUDIO_DEFAULT_CONTENT`; tests can
//! override via the `MockLmStudio` builder.
//!
//! ## Not yet wired
//!
//! - SSE streaming (the runtime's `chat_streaming` path). v1 only
//!   covers non-streaming. Streaming tests will need an SSE-aware mock.
//! - Tool-call responses. Real models emit `finish_reason: "tool_calls"`
//!   with `tool_calls: [...]` arrays. The v1 mock returns plain text
//!   completions; tool-call scenarios will need a per-test override.
//! - Per-fixture canned responses by role/prompt. v1 returns one
//!   response for all requests. Multi-scenario tests can add a
//!   request-matching layer later.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};

/// Default mock-content payload returned to every chat completion when
/// the test doesn't override. Short enough to read in test output;
/// recognizable so a real LMStudio response can't accidentally pass
/// for a mock one.
pub const MOCK_LMSTUDIO_DEFAULT_CONTENT: &str =
    "[mock-lmstudio] canned completion — replace via MockLmStudio::with_content() in tests";

/// Default artificial latency per `/v1/chat/completions` call. Zero by
/// default; tests that exercise queue-depth or timeout behavior
/// override via `with_response_delay()`.
const DEFAULT_RESPONSE_DELAY: Duration = Duration::from_millis(0);

/// Handle to a running mock LMStudio server. Returned by `spawn()`;
/// drop semantics are best-effort (axum doesn't have a clean shutdown
/// hook in this v1 shape — the process-exit kills the task).
///
/// The harness keeps the handle alive for the full test; `addr()`
/// returns the bound address suitable for `DARKMUX_LMSTUDIO_URL` or the
/// runtime's `--base-url`.
pub struct MockLmStudio {
    addr: SocketAddr,
    #[allow(dead_code)] // consumed by Wave-E.2+ scenarios (parallel_dispatch_measured, etc.)
    request_count: Arc<Mutex<usize>>,
    _runtime_handle: JoinHandle<()>,
}

#[derive(Clone)]
struct MockState {
    content: Arc<Mutex<String>>,
    response_delay: Arc<Mutex<Duration>>,
    request_count: Arc<Mutex<usize>>,
}

impl MockLmStudio {
    /// Spawn the mock on a random free port. The HTTP server runs on a
    /// dedicated tokio runtime inside a `std::thread`, so callers don't
    /// need to be tokio-aware — fits the test runner's sync style.
    pub fn spawn() -> std::io::Result<Self> {
        Self::spawn_with_content(MOCK_LMSTUDIO_DEFAULT_CONTENT)
    }

    pub fn spawn_with_content(content: &str) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        // Convert to a tokio-compatible listener inside the runtime
        // thread so we don't have to thread a tokio handle out.
        let std_listener = listener;
        std_listener.set_nonblocking(true)?;

        let state = MockState {
            content: Arc::new(Mutex::new(content.to_string())),
            response_delay: Arc::new(Mutex::new(DEFAULT_RESPONSE_DELAY)),
            request_count: Arc::new(Mutex::new(0)),
        };
        let request_count = state.request_count.clone();

        let app = Router::new()
            .route("/v1/chat/completions", post(handle_completion))
            .route("/v1/models", get(handle_models))
            .with_state(state);

        let handle = std::thread::Builder::new()
            .name("mock-lmstudio".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("mock-lmstudio tokio runtime");
                rt.block_on(async move {
                    let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
                        .expect("convert std listener");
                    axum::serve(tokio_listener, app)
                        .await
                        .expect("mock-lmstudio serve");
                });
            })?;

        Ok(Self {
            addr,
            request_count,
            _runtime_handle: handle,
        })
    }

    /// Bound address for daemons / runtime to point at.
    /// Use as `format!("http://{}/v1", mock.addr())`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Number of `/v1/chat/completions` requests served so far. Tests
    /// assert against this to verify dispatch fan-out actually reached
    /// the mock. (Wave-E.2+ scenario hook.)
    #[allow(dead_code)]
    pub fn request_count(&self) -> usize {
        *self.request_count.lock().expect("request_count lock")
    }
}

async fn handle_completion(
    State(state): State<MockState>,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let mut count = state.request_count.lock().unwrap();
        *count += 1;
    }
    let delay = *state.response_delay.lock().unwrap();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let content = state.content.lock().unwrap().clone();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "id": format!("mock-{now_unix}"),
        "object": "chat.completion",
        "created": now_unix,
        "model": "mock-lmstudio",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": content.split_whitespace().count(),
            "total_tokens": content.split_whitespace().count()
        }
    })))
}

async fn handle_models(_state: State<MockState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "object": "list",
        "data": [{
            "id": "mock-lmstudio",
            "object": "model",
            "created": 0,
            "owned_by": "darkmux-test"
        }]
    }))
}
