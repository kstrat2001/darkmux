//! The daemon's route table: the one place a route exists.
//!
//! From this release the routes and response shapes of the daemon are semver
//! contracts. The router is built from [`table`], and the golden
//! (`route-table.golden`, checked by `route_table_matches_the_golden`) pins
//! every entry's method, path and response type, so adding, removing or
//! renaming a route fails a test until the golden is regenerated on purpose
//! (`DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve routes`).
//!
//! A JSON route names its response type through the `json!`
//! macro, which checks at compile time that the type exists; that type is one of
//! [`crate::wire`]'s (or lives beside the thing it describes), and its
//! TypeScript twin is generated, which `every_json_response_type_has_a_generated_twin`
//! checks.

use axum::routing::MethodRouter;
use axum::Router;
use std::time::Duration;

use crate::AppState;

/// What a route answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reply {
    /// A JSON body that is one serialized value of the named type.
    Json(&'static str),
    /// The viewer document.
    Html,
    /// A static image the standalone shell uses.
    Png,
    /// The web app manifest: a static, browser-specified document.
    Manifest,
    /// A server-sent event stream whose events are flow-record JSON lines.
    EventStream,
}

/// A type's name without its module path: what the generated TypeScript calls it.
#[cfg(test)]
fn bare(ty: &str) -> &str {
    ty.rsplit("::").next().unwrap_or(ty)
}

#[cfg(test)]
impl Reply {
    fn render(self) -> String {
        match self {
            Reply::Json(t) => format!("json {}", bare(t)),
            Reply::Html => "html".to_string(),
            Reply::Png => "png".to_string(),
            Reply::Manifest => "manifest".to_string(),
            Reply::EventStream => "sse FlowRecord".to_string(),
        }
    }
}

/// One `GET` route.
pub(crate) struct Route {
    pub(crate) path: &'static str,
    pub(crate) reply: Reply,
    handler: MethodRouter<AppState>,
    /// The handler redacts for a remote caller itself (the console panels,
    /// whose terminal output needs escape-aware filtering), so the JSON layer
    /// stays off it.
    redacts_itself: bool,
}

/// A JSON route: the type is checked to exist, and named in the golden.
macro_rules! json {
    ($ty:ty, $path:literal, $handler:expr) => {{
        let _: Option<$ty> = None;
        Route { path: $path, reply: Reply::Json(stringify!($ty)), handler: get($handler), redacts_itself: false }
    }};
}

fn plain(path: &'static str, reply: Reply, handler: MethodRouter<AppState>) -> Route {
    Route { path, reply, handler, redacts_itself: false }
}

/// Every route the viewer daemon serves.
pub(crate) fn table() -> Vec<Route> {
    use crate::wire::*;
    use crate::*;
    vec![
        plain("/", Reply::Html, get(root_html)),
        plain("/play/:date", Reply::Html, get(play_html)),
        json!(HealthResponse, "/health", health),
        json!(FlowRecordsResponse, "/flow/:date", flow_handler),
        plain("/flow/:date/stream", Reply::EventStream, get(flow_stream_handler)),
        json!(FlowDaysResponse, "/flow-days", flow_days_handler),
        json!(FlowMissionsResponse, "/flow-missions", flow_missions_handler),
        json!(FlowRecordsResponse, "/flow-mission/:id", flow_mission_handler),
        json!(FlowRecordsResponse, "/flow-dispatch/:id", flow_dispatch_handler),
        json!(MachineStatusResponse, "/machine/status", machine_status_handler),
        json!(MachineSpecsResponse, "/machine/specs", machine_specs_handler),
        json!(MachineResourcesResponse, "/machine/resources", machine_resources_handler),
        json!(MissionsResponse, "/missions", missions_handler),
        json!(RunsResponse, "/runs", runs_handler),
        Route { redacts_itself: true, ..json!(PanelResponse, "/panel/:id", panel::panel_handler) },
        json!(PhasesResponse, "/phases", phases_handler),
        json!(mission_graph::MissionGraph, "/mission/:id/graph.json", mission_graph_json_handler),
        plain("/manifest.webmanifest", Reply::Manifest, get(web_manifest_handler)),
        plain("/apple-touch-icon.png", Reply::Png, get(apple_touch_icon_handler)),
        plain("/icon-192.png", Reply::Png, get(icon_192_handler)),
        plain("/favicon-32.png", Reply::Png, get(favicon_32_handler)),
        plain("/favicon-16.png", Reply::Png, get(favicon_16_handler)),
        plain("/icon-512.png", Reply::Png, get(icon_512_handler)),
        plain("/icon-512-maskable.png", Reply::Png, get(icon_512_maskable_handler)),
        json!(FleetDispatchesLiveResponse, "/fleet/dispatches/live", fleet_dispatches_live_handler),
        json!(FleetMachinesLiveResponse, "/fleet/machines/live", fleet_machines_live_handler),
        json!(FleetRosterResponse, "/fleet/roster", fleet_roster_handler),
        json!(fleet_view::FleetView, "/fleet/view", fleet_view::fleet_view_handler),
        json!(LabRunsResponse, "/lab/runs", lab_runs_handler),
        json!(LabRunDetailResponse, "/lab/run/detail", lab_run_detail_handler),
        json!(LabRunEventsResponse, "/lab/run/events", lab_run_events_handler),
    ]
}

// ─── The client listener (`darkmux client serve`) ─────────────────────────

/// Adds a type's JSON schema to a generator's definitions and answers the
/// schema that names it (a `$ref`), or, for a query, the type's own schema.
pub(crate) type SchemaFn = fn(&mut schemars::SchemaGenerator) -> schemars::Schema;

/// One route of `darkmux client serve`: its method, path, response type,
/// request body and query, and handler. The client listener is its own
/// listener (127.0.0.1 only, behind its own gate,
/// `client_pairing::only_this_machines_apps`), so this table is never
/// mounted on the daemon; it is a table so its routes are declared in one
/// place and pinned by the same golden, and (#3494) so the API's OpenAPI
/// document (`client_openapi`) is built from it, never by hand.
pub(crate) struct ClientRoute {
    /// Read by the golden and the OpenAPI document (the router takes the
    /// method from `handler`).
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    /// The JSON body a success answers with (read by the golden).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) reply: &'static str,
    /// That body's schema.
    pub(crate) reply_schema: SchemaFn,
    /// The JSON body the route reads, if any.
    pub(crate) body: Option<RequestBody>,
    /// The query the route reads, if any: its type's own schema, one
    /// parameter per property.
    pub(crate) query: Option<SchemaFn>,
    /// What the route takes in place of a token, if anything
    /// (`every_route_is_open_or_not_as_the_table_declares` holds the
    /// handlers to it).
    pub(crate) auth: ClientAuth,
    handler: MethodRouter<crate::client_pairing::ClientServerState>,
}

/// How a client route knows who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientAuth {
    /// `Authorization: Bearer <token>`, the token a pairing delivered.
    Bearer,
    /// Nothing: any app on this machine may ask (`POST /client/pair`).
    Open,
    /// The `X-Pair-Secret` header, the `pair_secret` the pairing request
    /// sent (`GET /client/pair/{request_id}`), and no token.
    PairSecret,
}

/// The JSON body a client route reads.
#[derive(Clone, Copy)]
pub(crate) struct RequestBody {
    pub(crate) schema: SchemaFn,
    /// False for a body the route reads only when one is sent.
    pub(crate) required: bool,
}

/// A client route: the reply type (and any body or query type) is checked
/// to exist and to have a schema, and named in the goldens. A route that
/// reads a JSON body names its type with `body = T` (`optional_body = T`
/// when it may be absent), one that reads a query with `query = T`. A route
/// that takes no token says what it takes instead: `auth = open` or `auth =
/// pair_secret`.
macro_rules! client_json {
    ($method:ident, $ty:ty, $path:expr, $handler:expr $(, $key:ident = $arg:tt)*) => {{
        #[allow(unused_mut)]
        let mut route = ClientRoute {
            method: stringify!($method),
            path: $path,
            reply: stringify!($ty),
            reply_schema: |g| g.subschema_for::<$ty>(),
            body: None,
            query: None,
            auth: ClientAuth::Bearer,
            handler: axum::routing::$method($handler),
        };
        $( client_json!(@ route, $key, $arg); )*
        route
    }};
    (@ $r:ident, body, $t:ty) => { $r.body = Some(RequestBody { schema: |g| g.subschema_for::<$t>(), required: true }) };
    (@ $r:ident, optional_body, $t:ty) => { $r.body = Some(RequestBody { schema: |g| g.subschema_for::<$t>(), required: false }) };
    (@ $r:ident, query, $t:ty) => { $r.query = Some(|g| <$t as schemars::JsonSchema>::json_schema(g)) };
    (@ $r:ident, auth, open) => { $r.auth = ClientAuth::Open };
    (@ $r:ident, auth, pair_secret) => { $r.auth = ClientAuth::PairSecret };
}

/// Every route the client listener serves. Every refusal on any of them is
/// a `ClientError`.
pub(crate) fn client_table() -> Vec<ClientRoute> {
    use crate::client_pairing::*;
    use crate::client_wire::*;
    vec![
        client_json!(post, PairAccepted, "/client/pair", pair_handler, body = PairBody, auth = open),
        client_json!(get, PairPoll, "/client/pair/:request_id", poll_handler, auth = pair_secret),
        client_json!(get, Whoami, "/client/whoami", whoami_handler),
        client_json!(get, ClientOpenApi, "/client/openapi.json", openapi_handler),
        client_json!(get, ClientProfiles, "/client/profiles", profiles_handler, query = ProfilesQuery),
        client_json!(post, WorkAccepted, "/client/work", work_handler, body = WorkBody),
        client_json!(get, WorkState, "/client/work/:work_id", work_status_handler),
        client_json!(post, StatusWord, "/client/work/:work_id/cancel", cancel_handler),
        client_json!(post, KeepReply, "/client/work/:work_id/keep", keep_handler),
        client_json!(post, DiscardReply, "/client/work/:work_id/discard", discard_handler),
        client_json!(post, UndoReply, "/client/work/:work_id/undo", undo_handler),
        client_json!(get, RecipeReply, "/client/recipe", recipe_handler, query = RecipeQuery),
        client_json!(post, WorkspaceInfo, "/client/workspaces", workspace_create_handler, body = WorkspaceBody),
        client_json!(get, WorkspaceInfo, "/client/workspaces/:name", workspace_status_handler),
        client_json!(delete, WorkspaceRemoved, "/client/workspaces/:name", workspace_remove_handler, optional_body = RemoveBody),
        client_json!(get, BranchChanges, "/client/workspaces/:name/changes", workspace_changes_handler),
        client_json!(post, EditReply, "/client/workspaces/:name/edit", workspace_edit_handler, body = EditBody),
        client_json!(post, ImageReply, IMAGE_ROUTE, workspace_image_handler, body = ImageBody),
        client_json!(post, PreviewState, "/client/workspaces/:name/preview", preview_start_handler, optional_body = PreviewStartBody),
        client_json!(get, PreviewState, "/client/workspaces/:name/preview", preview_status_handler),
        client_json!(delete, StatusWord, "/client/workspaces/:name/preview", preview_stop_handler),
        client_json!(put, RecipeReply, "/client/workspaces/:name/recipe", recipe_set_handler, body = RecipeSetBody),
    ]
}

/// (#3560) Whether a route at `path` takes no token (`auth = open` or
/// `pair_secret`: the pairing routes). Through the tailnet entry such a
/// route needs a tailnet user and device to record, in place of a token.
pub(crate) fn takes_no_token(path: &str) -> bool {
    static NO_TOKEN: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NO_TOKEN
        .get_or_init(|| client_table().into_iter().filter(|r| r.auth != ClientAuth::Bearer).map(|r| r.path).collect())
        .contains(&path)
}

/// The client listener's router: every table route (a path listed twice
/// takes both methods), JSON for an unlisted path or method, and the
/// listener's gate in front of all of it.
pub(crate) fn client_router(state: crate::client_pairing::ClientServerState) -> Router {
    use crate::client_pairing as cp;
    let mut by_path: Vec<(&'static str, MethodRouter<cp::ClientServerState>)> = Vec::new();
    for r in client_table() {
        match by_path.iter_mut().find(|(p, _)| *p == r.path) {
            Some((_, m)) => *m = std::mem::take(m).merge(r.handler),
            None => by_path.push((r.path, r.handler)),
        }
    }
    by_path
        .into_iter()
        .fold(Router::new(), |router, (path, handler)| router.route(path, handler))
        .fallback(cp::no_such_route)
        .method_not_allowed_fallback(cp::method_not_allowed)
        .layer(axum::middleware::from_fn_with_state(state.clone(), cp::only_this_machines_apps))
        // (#3468) A panic in a handler answers a coded 500 (`ClientError`).
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(cp::panic_response))
        .with_state(state)
}

/// The client listener's routes and the shape of every type they answer
/// with, read from each type's JSON schema (no TypeScript twin exists for
/// them: the viewer imports none).
#[cfg(test)]
fn render_client_table() -> String {
    let mut out = String::new();
    for r in client_table() {
        // (#3560) A route that takes a token also answers a paired device
        // through the tailnet entry; the pairing routes, a device pairing
        // through it.
        let who = match r.auth {
            ClientAuth::Bearer => "this machine's apps and paired devices through its tailnet entry",
            ClientAuth::Open | ClientAuth::PairSecret => "this machine's apps and devices pairing through its tailnet entry",
        };
        out.push_str(&format!(
            "{} {}  json {} (client listener: 127.0.0.1, {who}; not the viewer daemon)\n",
            r.method.to_uppercase(),
            r.path,
            bare(r.reply)
        ));
    }
    out.push_str("\n# client listener response types: Type.field: type, from the JSON schema (every refusal is ClientError)\n");
    let mut lines = std::collections::BTreeSet::new();
    for (name, schema) in client_schemas() {
        type_lines(&name, &schema, &mut lines);
    }
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// The JSON schema of every client response type (and of `ClientError`):
/// the generator's definitions once each route's reply schema is added.
#[cfg(test)]
fn client_schemas() -> Vec<(String, serde_json::Value)> {
    let mut generator = schemars::SchemaGenerator::default();
    generator.subschema_for::<crate::client_wire::ClientError>();
    for r in client_table() {
        (r.reply_schema)(&mut generator);
    }
    generator.definitions().clone().into_iter().collect()
}

#[cfg(test)]
fn type_lines(name: &str, schema: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
    match schema.get("properties").and_then(|p| p.as_object()) {
        Some(_) => {
            for member in object_members(schema) {
                out.insert(format!("{name}.{member}"));
            }
        }
        None => {
            out.insert(format!("{name} = {}", schema_type(schema)));
        }
    }
}

/// `name: type` (`name?: type` when not required) for each property.
#[cfg(test)]
fn object_members(schema: &serde_json::Value) -> Vec<String> {
    let required: Vec<&str> =
        schema.get("required").and_then(|r| r.as_array()).map(|r| r.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    let props = schema.get("properties").and_then(|p| p.as_object()).cloned().unwrap_or_default();
    props
        .iter()
        .map(|(k, v)| format!("{k}{}: {}", if required.contains(&k.as_str()) { "" } else { "?" }, schema_type(v)))
        .collect()
}

/// A schema as a short type: a referenced type's name, a constant, a union,
/// an array of its items, an inline object's members, or its JSON type.
#[cfg(test)]
fn schema_type(v: &serde_json::Value) -> String {
    composite_type(v).unwrap_or_else(|| json_type(v))
}

/// A reference, constant, enum, union or inline object, if `v` is one.
#[cfg(test)]
fn composite_type(v: &serde_json::Value) -> Option<String> {
    if let Some(r) = v.get("$ref").and_then(|r| r.as_str()) {
        return Some(r.rsplit('/').next().unwrap_or(r).to_string());
    }
    if let Some(c) = v.get("const") {
        return Some(c.to_string());
    }
    if let Some(e) = v.get("enum").and_then(|e| e.as_array()) {
        return Some(e.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" | "));
    }
    let alts = v.get("oneOf").or_else(|| v.get("anyOf")).and_then(|a| a.as_array());
    if let Some(alts) = alts {
        return Some(alts.iter().map(schema_type).collect::<Vec<_>>().join(" | "));
    }
    v.get("properties").map(|_| format!("{{ {} }}", object_members(v).join(", ")))
}

/// The schema's JSON `type` (one or a union of several).
#[cfg(test)]
fn json_type(v: &serde_json::Value) -> String {
    let types: Vec<&str> = match v.get("type") {
        Some(serde_json::Value::String(t)) => vec![t.as_str()],
        Some(serde_json::Value::Array(ts)) => ts.iter().filter_map(|t| t.as_str()).collect(),
        _ => return "any".to_string(),
    };
    types.iter().map(|t| one_json_type(t, v)).collect::<Vec<_>>().join(" | ")
}

/// One JSON type, with an array's items and a map's values.
#[cfg(test)]
fn one_json_type(t: &str, v: &serde_json::Value) -> String {
    match (t, v.get("additionalProperties")) {
        ("array", _) => format!("Array<{}>", v.get("items").map_or_else(|| "any".to_string(), schema_type)),
        ("object", Some(a)) if a.is_object() => format!("Record<string, {}>", schema_type(a)),
        (other, _) => other.to_string(),
    }
}

/// How a route keeps host facts from a remote reader. Every route has one, and
/// [`Route::redaction`] matches exhaustively on [`Reply`], so a new kind of
/// reply cannot be added without choosing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteRedaction {
    /// The handler sits behind [`crate::redaction::redact_reads`].
    Layer,
    /// The handler redacts for a remote reader itself (the panels' terminal
    /// output, the event stream's lines).
    Handler,
    /// A fixed document or image that names no host.
    Static,
}

impl Route {
    pub(crate) fn redaction(&self) -> RouteRedaction {
        match self.reply {
            Reply::Json(_) if self.redacts_itself => RouteRedaction::Handler,
            Reply::Json(_) => RouteRedaction::Layer,
            Reply::EventStream => RouteRedaction::Handler,
            Reply::Html | Reply::Png | Reply::Manifest => RouteRedaction::Static,
        }
    }
}

/// A route's handler, behind the redaction layer when its stance is `Layer`.
fn with_read_redaction(route: &Route) -> MethodRouter<AppState> {
    match route.redaction() {
        RouteRedaction::Layer => route.handler.clone().layer(axum::middleware::from_fn(crate::redaction::redact_reads)),
        RouteRedaction::Handler | RouteRedaction::Static => route.handler.clone(),
    }
}

/// The router: every non-streaming route gets a request timeout, bounding a slow
/// or hung request; the long-lived event stream (#925) is kept apart so that
/// timeout never applies to it.
pub(crate) fn router() -> Router<AppState> {
    let (streaming, timed): (Vec<Route>, Vec<Route>) =
        table().into_iter().partition(|r| r.reply == Reply::EventStream);
    let timed = timed
        .into_iter()
        .fold(Router::new(), |router, r| router.route(r.path, with_read_redaction(&r)))
        .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(crate::REQUEST_TIMEOUT_SECS)));
    let streaming = streaming.into_iter().fold(Router::new(), |router, r| router.route(r.path, r.handler));
    timed.merge(streaming)
}

/// The fields of a generated TypeScript type, one `name: type` string per
/// top-level member (a union's members instead, for a type that is not an
/// object). The generated twin is the repo's own record of a response type's
/// shape, so the golden reads it instead of a second hand-kept description.
/// Doc comments are dropped and whitespace is collapsed, so an edited comment
/// never reads as a changed field.
#[cfg(test)]
fn ts_members(src: &str) -> Vec<String> {
    let mut text = String::new();
    let mut rest = src;
    while let Some(i) = rest.find("/*") {
        text.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    text.push_str(rest);
    let body = text.split_once("export type ").and_then(|(_, t)| t.split_once('=')).map_or("", |(_, b)| b.trim());
    let body = body.trim_end().trim_end_matches(';').trim();
    let (inner, sep) = match body.strip_prefix('{').and_then(|b| b.strip_suffix('}')) {
        Some(inner) => (inner, ','),
        None => (body, '|'),
    };
    let (mut parts, mut depth, mut quote, mut cur) = (Vec::new(), 0i32, None::<char>, String::new());
    for c in inner.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '{' | '(' | '[' | '<') => depth += 1,
            (None, '}' | ')' | ']' | '>') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts.iter().map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|p| !p.is_empty()).collect()
}

/// The table as the golden renders it: one `GET <path>  <reply>` line per route,
/// in table order, then the fleet listener's routes, then every JSON response
/// type's fields (`Type.name: type`, from its generated TypeScript twin). A
/// response shape is a contract (D2, #3035): `scripts/contract-additive-guard.py`
/// lets a PR add lines here and refuses one that removes or changes a line.
#[cfg(test)]
pub(crate) fn render_table() -> String {
    let mut out: String = table().iter().map(|r| format!("GET {}  {}\n", r.path, r.reply.render())).collect();
    out.push_str(&format!(
        "POST {}  ndjson (darkmux_fleet submission; fleet listener, not the viewer daemon)\n",
        darkmux_fleet::SUBMISSION_PATH
    ));
    out.push_str(&format!(
        "GET {}  json ListenerCard (fleet listener, not the viewer daemon; the caller's own grant rides with the card)\n",
        darkmux_fleet::CARD_PATH
    ));
    out.push_str(&render_client_table());
    out.push_str("\n# response types: Type.field: type, from the generated TypeScript twins\n");
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/types/generated");
    let mut seen = std::collections::BTreeSet::new();
    for r in table() {
        let Reply::Json(ty) = r.reply else { continue };
        let name = bare(ty);
        if !seen.insert(name) {
            continue;
        }
        let src = std::fs::read_to_string(dir.join(format!("{name}.ts")))
            .unwrap_or_else(|e| panic!("{} answers with {ty}, whose generated {name}.ts is unreadable: {e}", r.path));
        for member in ts_members(&src) {
            out.push_str(&format!("{name}.{member}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("route-table.golden")
    }

    /// Adding, removing or renaming a route, or changing a route's response
    /// type, fails here until the golden is regenerated: a deliberate act,
    /// visible in the diff.
    #[test]
    fn route_table_matches_the_golden() {
        let rendered = render_table();
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            std::fs::write(golden_path(), &rendered).expect("writing the route-table golden");
            return;
        }
        let golden = std::fs::read_to_string(golden_path())
            .expect("route-table.golden is missing: regenerate with DARKMUX_REGENERATE_FIXTURES=1");
        assert_eq!(
            rendered, golden,
            "the route table changed. Routes and response shapes are semver contracts: if this \
             is intended, regenerate with `DARKMUX_REGENERATE_FIXTURES=1 cargo test -p \
             darkmux-serve route_table`, and add a CHANGELOG migration line."
        );
    }

    /// The member reader keeps a field's whole type (nested generics, unions,
    /// inline objects), drops doc comments, and reads a non-object type as its
    /// union members.
    #[test]
    fn ts_members_reads_fields_and_unions() {
        let object = "import type { A } from \"./A\";\n/** doc */\nexport type T = { a: Array<A>, \n/** why */\nb?: string | null, c: { x: number, y: number }, d: \"p,q\", };\n";
        assert_eq!(
            ts_members(object),
            ["a: Array<A>", "b?: string | null", "c: { x: number, y: number }", "d: \"p,q\""]
        );
        assert_eq!(ts_members("export type U = \"on\" | \"off\" | { k: number };"), ["\"on\"", "\"off\"", "{ k: number }"]);
    }

    /// No two routes share a path, and every path is unique in the router.
    #[test]
    fn route_paths_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for r in table() {
            assert!(seen.insert(r.path), "{} is registered twice", r.path);
        }
    }

    /// Every JSON response type has a generated TypeScript twin on disk, so the
    /// viewer can import it. (`types:check` in CI keeps the twins fresh.)
    #[test]
    fn every_json_response_type_has_a_generated_twin() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/types/generated");
        for r in table() {
            let ty = match r.reply {
                Reply::Json(t) => t,
                _ => continue,
            };
            let name = bare(ty);
            assert!(
                dir.join(format!("{name}.ts")).is_file(),
                "{} answers with {ty}, which has no generated {name}.ts: derive TS on it and run `bun run types:regen`",
                r.path
            );
        }
    }

    /// The fleet listener's own routes are the ones the golden names, and its
    /// router answers exactly those (the gate's refusal, not a 404, for a
    /// caller with no token).
    #[test]
    fn the_fleet_listener_serves_the_paths_the_golden_names() {
        let table = render_table();
        assert!(table.contains(&format!("POST {}", darkmux_fleet::SUBMISSION_PATH)));
        assert!(table.contains(&format!("GET {}", darkmux_fleet::CARD_PATH)));
    }

    /// The router answers every table path (a routed handler may answer 400 or
    /// 404 for a fabricated id, but with a body; an UNROUTED path is an empty
    /// 404), and a path the table does not list is a 404.
    #[tokio::test]
    async fn the_router_serves_exactly_the_table() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let flows = tempfile::tempdir().unwrap();
        let answer = |uri: String| {
            let app = crate::build_router_local(flows.path().to_path_buf());
            async move {
                let res = app
                    .oneshot(Request::builder().uri(uri).header("x-darkmux-panel", "1").body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = res.status();
                (status, axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap())
            }
        };
        for r in table().into_iter().filter(|r| r.reply != Reply::EventStream) {
            let (status, body) = answer(r.path.replace(":date", "2026-01-01").replace(":id", "x")).await;
            assert!(
                !(status == StatusCode::NOT_FOUND && body.is_empty()),
                "{} is in the table but the router does not serve it",
                r.path
            );
        }
        let (status, _) = answer("/not-in-the-table".to_string()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// No two client routes share a method and path.
    #[test]
    fn client_routes_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for r in client_table() {
            assert!(seen.insert((r.method, r.path)), "{} {} is registered twice", r.method, r.path);
        }
    }

    /// The client router answers every client-table route with that route's
    /// handler (an authenticated route refuses a stranger with a JSON 401,
    /// never the router's 404 or 405), and an unlisted path is the router's
    /// JSON 404.
    #[tokio::test]
    async fn the_client_router_serves_exactly_the_client_table() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let state = crate::client_pairing::ClientServerState::production().unwrap();
        let state = crate::client_pairing::ClientServerState {
            store: std::sync::Arc::new(crate::client_pairing::ClientStore::at(
                tempfile::tempdir().unwrap().keep().join("clients.json"),
            )),
            ..state
        };
        let answer = |method: &str, uri: String| {
            let app = client_router(state.clone());
            let mut req = Request::builder().method(method).uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 5555))));
            async move {
                let res = app.oneshot(req).await.unwrap();
                let status = res.status();
                (status, axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap())
            }
        };
        for r in client_table() {
            let uri = r.path.replace(":request_id", "r-x").replace(":work_id", "w-x").replace(":name", "nope");
            let (status, body) = answer(&r.method.to_uppercase(), uri).await;
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(
                status != StatusCode::METHOD_NOT_ALLOWED && v["error"] != "no such route",
                "{} {} is in the client table but the router does not serve it: {status} {v}",
                r.method,
                r.path
            );
        }
        let (status, body) = answer("GET", "/client/not-in-the-table".to_string()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"], "no such route");
    }

    /// Routes retired for the major release answer 404, not a redirect: a
    /// redirect would be an alias, and no alias survives.
    #[tokio::test]
    async fn retired_routes_are_unknown_not_redirects() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let flows = tempfile::tempdir().unwrap();
        for uri in [
            "/next",
            "/mission/m/graph",
            "/worktree-summary/s",
            "/flow-status",
            "/flow-session/s",
            "/fleet/sessions/live",
        ] {
            let res = crate::build_router_local(flows.path().to_path_buf())
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri} must be unknown");
        }
    }
}
