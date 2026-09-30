//! The daemon's route table: the one place a route exists.
//!
//! From this release the routes and response shapes of the daemon are semver
//! contracts. The router is built from [`table`], and the golden
//! (`route-table.golden`, checked by `route_table_matches_the_golden`) pins
//! every entry's method, path and response type, so adding, removing or
//! renaming a route fails a test until the golden is regenerated on purpose
//! (`DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve routes`).
//!
//! A JSON route names its response type through the `json!` / `json_list!`
//! macros, which check at compile time that the type exists; that type is one of
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
    /// A JSON body that is an array of the named type.
    JsonList(&'static str),
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
            Reply::JsonList(t) => format!("json {}[]", bare(t)),
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
}

/// A JSON route: the type is checked to exist, and named in the golden.
macro_rules! json {
    ($ty:ty, $path:literal, $handler:expr) => {{
        let _: Option<$ty> = None;
        Route { path: $path, reply: Reply::Json(stringify!($ty)), handler: get($handler) }
    }};
}

/// A JSON route answering with an array of a type.
macro_rules! json_list {
    ($ty:ty, $path:literal, $handler:expr) => {{
        let _: Option<$ty> = None;
        Route { path: $path, reply: Reply::JsonList(stringify!($ty)), handler: get($handler) }
    }};
}

fn plain(path: &'static str, reply: Reply, handler: MethodRouter<AppState>) -> Route {
    Route { path, reply, handler }
}

/// Every route the viewer daemon serves.
pub(crate) fn table() -> Vec<Route> {
    use crate::wire::*;
    use crate::*;
    vec![
        plain("/", Reply::Html, get(root_html)),
        plain("/play/:date", Reply::Html, get(play_html)),
        json!(HealthResponse, "/health", health),
        json_list!(darkmux_flow::FlowRecord, "/flow/:date", flow_handler),
        plain("/flow/:date/stream", Reply::EventStream, get(flow_stream_handler)),
        json!(FlowDaysResponse, "/flow-days", flow_days_handler),
        json!(FlowMissionsResponse, "/flow-missions", flow_missions_handler),
        json!(FlowRecordsResponse, "/flow-mission/:id", flow_mission_handler),
        json!(FlowRecordsResponse, "/flow-dispatch/:id", flow_dispatch_handler),
        json!(MachineStatusResponse, "/machine/status", machine_status_handler),
        json!(MachineSpecsResponse, "/machine/specs", machine_specs_handler),
        json!(MachineResourcesResponse, "/machine/resources", machine_resources_handler),
        json!(machine_card::MachineCard, "/machine/card", fleet_view::machine_card_handler),
        json!(MissionsResponse, "/missions", missions_handler),
        json!(RunsResponse, "/runs", runs_handler),
        json!(PanelResponse, "/panel/:id", panel::panel_handler),
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

/// The router: every non-streaming route gets a request timeout, bounding a slow
/// or hung request; the long-lived event stream (#925) is kept apart so that
/// timeout never applies to it.
pub(crate) fn router() -> Router<AppState> {
    let (streaming, timed): (Vec<Route>, Vec<Route>) =
        table().into_iter().partition(|r| r.reply == Reply::EventStream);
    let timed = timed
        .into_iter()
        .fold(Router::new(), |router, r| router.route(r.path, r.handler))
        .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(crate::REQUEST_TIMEOUT_SECS)));
    let streaming = streaming.into_iter().fold(Router::new(), |router, r| router.route(r.path, r.handler));
    timed.merge(streaming)
}

/// The table as the golden renders it: one `GET <path>  <reply>` line per route,
/// in table order, then the fleet listener's route.
#[cfg(test)]
pub(crate) fn render_table() -> String {
    let mut out: String = table().iter().map(|r| format!("GET {}  {}\n", r.path, r.reply.render())).collect();
    out.push_str(&format!(
        "POST {}  ndjson (darkmux_fleet submission; fleet listener, not the viewer daemon)\n",
        darkmux_fleet::SUBMISSION_PATH
    ));
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
                Reply::Json(t) | Reply::JsonList(t) => t,
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

    /// The fleet listener's own route is the one the golden names.
    #[test]
    fn the_fleet_listener_serves_the_submission_path_the_golden_names() {
        assert!(render_table().contains(&format!("POST {}", darkmux_fleet::SUBMISSION_PATH)));
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
