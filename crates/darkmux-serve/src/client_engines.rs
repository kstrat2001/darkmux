//! (#3569) `GET /client/engines` and `POST /client/engines`: which engines
//! darkmux can run work on are installed on this Mac, whether any is usable,
//! and adding one darkmux starts servers for.
//!
//! darkmux recommends nothing: the reply lists what it found
//! (`darkmux_profiles::engine::detect`), and `usable: false` is what an app
//! reads to decide whether to offer one. Detection is read-only and bounded
//! (each engine's own version and model listing), and starts, loads and
//! pulls nothing.
//!
//! An add writes `profiles.json` (`darkmux_profiles::engine::add`), so an app
//! sends it only after its user confirms (an Approve card): the request is
//! that confirmation. It adds an endpoint and a profile running the model
//! the request names (or the engine's only one) at the context it names (or
//! the one the engine states), and nothing darkmux chose.

use crate::client_pairing::{
    authenticate, err, internal, json_body, ApprovedClient, ClientMachine, ClientServerState,
};
use crate::client_wire::RequestVia;
use anyhow::anyhow;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use darkmux_profiles::engine::add::{AddRequest, Planned, Refusal};
use darkmux_profiles::engine::detect::{Detected, DetectedEngine, DetectedKind};
use darkmux_types::client_code::ClientCode;
use darkmux_types::ProfileRegistry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Every engine found on this Mac, read-only and bounded.
pub type DetectEngines = Arc<dyn Fn() -> Detected + Send + Sync>;
/// Record one line in the `client serve` log.
pub type LogAdd = Arc<dyn Fn(&str) + Send + Sync>;
/// Add one, writing `profiles.json`.
pub type AddEngine = Arc<dyn Fn(&AddRequest) -> Result<Planned, Refusal> + Send + Sync>;

/// `GET /client/engines`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ClientEngines {
    /// Some engine found is `usable`. `false` when none is installed or none
    /// found can be used.
    pub usable: bool,
    /// Each engine found (an engine not installed is not listed).
    pub detected: Vec<DetectedEngine>,
    pub machine: ClientMachine,
}

/// `POST /client/engines`: the engine to add, after the user confirmed.
/// Only an app on this Mac may send it (a device on the tailnet is refused
/// `forbidden`), and every add is recorded in the `client serve` log.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddEngineBody {
    /// `tensorfold` or `flint`: an engine darkmux starts servers for, as
    /// `detected` lists it with `can_add`.
    pub engine: DetectedKind,
    /// Which of its `models` to add; may be left out when it holds one.
    #[serde(default)]
    pub model: Option<String>,
    /// The context to run it at, in tokens; may be left out when the model's
    /// `max_context` is stated, which is then used.
    #[serde(default)]
    pub n_ctx: Option<u32>,
}

/// `POST /client/engines`, 200: what was added to `profiles.json`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EngineAdded {
    pub engine: DetectedKind,
    /// The new endpoint's id.
    pub endpoint: String,
    /// The loopback port darkmux serves the engine on.
    pub port: u16,
    /// The new profile, which `POST /client/work` names to run on it.
    pub profile: String,
    pub model: String,
    pub n_ctx: u32,
}

impl EngineAdded {
    fn of(engine: DetectedKind, p: &Planned) -> Self {
        EngineAdded {
            engine,
            endpoint: p.endpoint.clone(),
            port: p.port,
            profile: p.profile.clone(),
            model: p.model.clone(),
            n_ctx: p.n_ctx,
        }
    }
}

/// An add's refusal as the API answers it.
pub(crate) fn refusal_response(r: &Refusal) -> Response {
    let (status, code) = match r {
        Refusal::Unavailable(_) => (StatusCode::CONFLICT, ClientCode::EngineUnavailable),
        Refusal::Configured(_) | Refusal::Changed(_) => {
            (StatusCode::CONFLICT, ClientCode::Conflict)
        }
        Refusal::BadRequest(_) => (StatusCode::BAD_REQUEST, ClientCode::BadRequest),
        Refusal::Registry(_) => (StatusCode::UNPROCESSABLE_ENTITY, ClientCode::Unprocessable),
    };
    err(status, code, r.to_string())
}

pub(crate) async fn engines_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let detect = state.detect_engines.clone();
    match tokio::task::spawn_blocking(move || detect()).await {
        Ok(found) => Json(ClientEngines {
            usable: found.usable,
            detected: found.detected,
            machine: state.machine.clone(),
        })
        .into_response(),
        Err(e) => internal(anyhow!("engine detection panicked: {e}")),
    }
}

/// What an add refuses from a device on the tailnet: it writes this Mac's
/// profile registry, so only an app on this Mac may ask for one.
pub(crate) const ADD_ON_THIS_MAC_ONLY: &str =
    "an engine is added only by an app on this Mac: adding one writes \
    its profile registry. Ask from the app on the Mac, or run `darkmux init --add-engine` there";

pub(crate) async fn engine_add_handler(
    State(state): State<ClientServerState>,
    via: Option<Extension<RequestVia>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if via.map_or(RequestVia::Local, |v| v.0) != RequestVia::Local {
        return err(
            StatusCode::FORBIDDEN,
            ClientCode::Forbidden,
            ADD_ON_THIS_MAC_ONLY,
        );
    }
    let req: AddEngineBody = match json_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let add = state.add_engine.clone();
    let asked = AddRequest {
        engine: req.engine,
        model: req.model,
        n_ctx: req.n_ctx,
    };
    match tokio::task::spawn_blocking(move || add(&asked)).await {
        Ok(Ok(planned)) => {
            (state.log_add)(&added_line(&client, &planned));
            Json(EngineAdded::of(req.engine, &planned)).into_response()
        }
        Ok(Err(refused)) => refusal_response(&refused),
        Err(e) => internal(anyhow!("engine add panicked: {e}")),
    }
}

/// The serve log's record of an add: which client (and the signing
/// identity that approved it, when one did) on this Mac, and what was
/// written. No token, no path.
pub(crate) fn added_line(client: &ApprovedClient, p: &Planned) -> String {
    use darkmux_types::config_enum::ConfigEnum;
    let signed = client
        .auto_approved
        .as_ref()
        .map(|s| format!(", signed {}/{}", s.team, s.bundle))
        .unwrap_or_default();
    darkmux_types::user_files::escape_text(&format!(
        "darkmux client serve: client `{}` ({}{signed}, an app on this Mac) added engine `{}` to profiles.json: endpoint `{}` on port {}, profile `{}` running `{}` at {} tokens",
        client.name, client.id, p.engine.token(), p.endpoint, p.port, p.profile, p.model, p.n_ctx
    ))
}

/// The production [`DetectEngines`]: this Mac's registry says which are
/// configured.
pub fn detect_here() -> Detected {
    let registry = darkmux_profiles::profiles::load_registry_quiet(None)
        .ok()
        .map(|l| l.registry);
    darkmux_profiles::engine::detect::detect(registry.as_ref())
}

/// The production [`AddEngine`]: [`add_in`] the registry dispatch reads,
/// asking this Mac's installed engines and ports.
pub fn add_here(req: &AddRequest) -> Result<Planned, Refusal> {
    let path = darkmux_profiles::profiles::registry_path(None);
    add_in(
        path.as_deref(),
        req,
        &darkmux_profiles::engine::detect::detect_servers,
        &port_free,
    )
}

/// What the engines found by `detect` in a registry.
pub type DetectIn<'a> = &'a dyn Fn(Option<&ProfileRegistry>) -> Vec<DetectedEngine>;

/// Plan the add against the registry at `path` and write it: the request
/// is the user's confirmation.
pub fn add_in(
    path: Option<&std::path::Path>,
    req: &AddRequest,
    detect: DetectIn<'_>,
    port_free: &dyn Fn(u16) -> bool,
) -> Result<Planned, Refusal> {
    let path = path.ok_or_else(|| {
        Refusal::Registry("this Mac has no profile registry; run `darkmux init` first".into())
    })?;
    if !path.exists() {
        return Err(Refusal::Registry(
            "this Mac has no profile registry; run `darkmux init` first".into(),
        ));
    }
    // Which engines are installed does not depend on the registry; whether
    // one is configured is read from the registry under the lock (`apply`).
    let found = detect(None);
    darkmux_profiles::engine::add::apply(path, &found, req, port_free)
}

/// Nothing listens on `port` (a host that cannot say counts as busy).
fn port_free(port: u16) -> bool {
    darkmux_platform::current()
        .tcp_listeners(port)
        .is_some_and(|pids| pids.is_empty())
}
