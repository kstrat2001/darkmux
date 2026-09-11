//! (#1466) Best-effort peer-mission-graph fetch.
//!
//! `GET /mission/:id/graph.json` (`crate::mission_graph_json_handler`)
//! reads local disk only: `mission_graph::build_mission_graph` walks
//! `~/.darkmux/missions/<id>/`, which holds nothing but THIS machine's own
//! Phase/Task/Step JSON. A mission a peer ran is invisible there no matter
//! how much of its work crossed the shared flow stream — the same
//! structural fact `runs::build_flow_mission_index`'s own doc names for the
//! Runs lens ("the task graph, per-step config genuinely is not available
//! off-machine").
//!
//! What changed since that doc was written: THIS module. The daemon's
//! `/mission/:id/graph.json` route is the identical route on every darkmux
//! instance in the fleet — a peer that ran the mission can answer the exact
//! same request this one just failed to. So when the local lookup comes up
//! empty, [`try_peer_graph`] asks the machine the flow stream says ran it,
//! IF that machine is something this reader can actually reach:
//!
//! 1. **Attributed** — some flow record (fleet stream or local day-file)
//!    names a `machine_id` for this mission (`runs::mission_owner_machine`).
//!    No record at all → [`None`] immediately, no roster/Redis touched —
//!    the pre-#1466-continuation "may have been ephemeral or cleared" case,
//!    unchanged.
//! 2. **Rostered** — that machine is in `darkmux machine add`'s roster.
//!    Attributed to a machine nobody registered here → nothing to dial →
//!    [`None`].
//! 3. **Live** — "presence is fleet-membership truth" (CLAUDE.md): a
//!    rostered machine with no current presence beat is a DIFFERENT state
//!    from a live one, and dialing a peer already known to be down would
//!    just be a slow way to reach the same [`None`] — so a silent peer is
//!    never attempted at all.
//! 4. **Reachable + authorized + answers** — only now does this module make
//!    a network call, bounded at [`PEER_GRAPH_TIMEOUT`] so a slow or wedged
//!    peer degrades the SAME way `fetch_peer_json` (`src/fleet_cli.rs`)
//!    already degrades for every other cross-machine read: unreachable, a
//!    401/403 (this machine isn't sending the shared bearer token), the
//!    peer's own 404 (genuinely not on ITS disk either — an older darkmux,
//!    or a truly cleared run), and a malformed body are all [`None`].
//!
//! **Every non-success outcome collapses to [`None`], on purpose.** The
//! caller's existing 404 body is unchanged, and the viewer's existing
//! `lookupOwningMachine` honesty fallback (#1656) already turns that 404
//! into "ran on `<machine>`, open darkmux there" from the SAME flow-record
//! attribution this module computes first — so every degraded case here
//! still lands on an already-correct, already-tested UI state. Only the
//! REACHABLE case is new behavior. darkmux describes, never adjudicates:
//! this module never renders a verdict about the peer or the operator's
//! network, it either returns the peer's own graph or gets out of the way.

use std::collections::HashSet;
use std::path::Path as StdPath;
use std::time::Duration;

use darkmux_fleet::FleetRoster;

/// Bounded at 2s — matches `fetch_peer_json`'s (`src/fleet_cli.rs`) own
/// timeout for the identical reason: one page load per peer, even a slow
/// or wedged one, never an indefinite hang. This call runs inside a
/// `spawn_blocking` off the request future (`mission_graph_json_handler`),
/// not the CLI's one-shot process — the operator-facing contract (bounded,
/// legible, never silent) is the same either way.
const PEER_GRAPH_TIMEOUT: Duration = Duration::from_millis(2000);

/// The decision `try_peer_graph` makes, once it already has an attributed
/// owner, before ever touching the network — pure, so it is testable
/// without a roster file, Redis, or a socket. `Unattributed` (no flow
/// record names ANY machine) is handled a level up, in `try_peer_graph`
/// itself, via an early `?` — it never reaches this classifier at all,
/// which is what keeps that common case from touching the roster or Redis.
/// See this module's own doc for what each variant means operationally.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PeerLookup {
    Unrostered { machine: String },
    Silent { machine: String },
    Live { machine: String, address: String },
}

fn classify_peer(owner: &str, roster: &FleetRoster, live_machines: &HashSet<String>) -> PeerLookup {
    let Some(entry) = roster.machines.get(owner) else {
        return PeerLookup::Unrostered { machine: owner.to_string() };
    };
    if !live_machines.contains(owner) {
        return PeerLookup::Silent { machine: owner.to_string() };
    }
    PeerLookup::Live { machine: owner.to_string(), address: entry.address.clone() }
}

/// Best-effort: serve `mission_id`'s graph from the peer that flow records
/// say ran it. `Some` only for a genuine, parsed graph from a live,
/// rostered, reachable, authorized peer — every other reason is `None`
/// (see this module's own doc); the caller's existing 404 already covers
/// all of them honestly.
pub(crate) fn try_peer_graph(
    mission_id: &str,
    flows_dir: &StdPath,
    fleet_records: &[serde_json::Value],
) -> Option<serde_json::Value> {
    // Short-circuits BEFORE any roster/Redis I/O for the common case (no
    // flow record anywhere names a machine for this mission) — the same
    // case `mission_graph_json_returns_404_for_unknown_mission` pins.
    let owner = crate::runs::mission_owner_machine(flows_dir, fleet_records, mission_id)?;
    let roster = darkmux_fleet::load_roster().ok()?;
    let live = live_machine_names();
    let token = darkmux_flow::serve_token();
    try_peer_graph_with(mission_id, &owner, &roster, &live, token.as_ref().map(|t| t.expose_for_compare()))
}

/// The dependency-injected core of [`try_peer_graph`] — roster, live-set,
/// and bearer token all supplied by the caller rather than read from the
/// real environment. Lets every branch (unrostered / silent / live+
/// unreachable / live+401 / live+404 / live+success) be exercised against
/// a real loopback HTTP fixture with zero Redis and zero roster file on
/// disk — see this module's `#[cfg(test)]` block.
fn try_peer_graph_with(
    mission_id: &str,
    owner: &str,
    roster: &FleetRoster,
    live_machines: &HashSet<String>,
    token: Option<&str>,
) -> Option<serde_json::Value> {
    let PeerLookup::Live { machine, address } = classify_peer(owner, roster, live_machines) else {
        return None;
    };
    let base = normalize_daemon_base(&address);
    let mut graph = fetch_peer_graph_json(&base, mission_id, token)?;
    stamp_provenance(&mut graph, &machine);
    Some(graph)
}

/// Which machines currently hold a live presence beat, by the SAME
/// `display_name` flow records stamp as `machine_id` (`resolve_machine_id`
/// backs both — see `darkmux_flow::presence::PresenceBeat::display_name`'s
/// own doc). `false`/empty on ANY inability to check (no Redis configured,
/// Redis unreachable, no matching beat) — presence is opt-in evidence,
/// never assumed, and every caller here already falls back to `None` safely
/// when the set is empty.
fn live_machine_names() -> HashSet<String> {
    let Some(url) = darkmux_flow::redis_url() else {
        return HashSet::new();
    };
    let Ok(client) = redis::Client::open(url.expose_for_probe()) else {
        return HashSet::new();
    };
    darkmux_flow::presence::read_live(&client)
        .map(|beats| beats.into_iter().map(|b| b.display_name).collect())
        .unwrap_or_default()
}

/// Mirrors `normalize_daemon_base` in `src/fleet_cli.rs` (a private helper
/// in the binary crate, unreachable from here) — same three roster address
/// shapes: a full URL, `host:port`, or a bare host that gets the default
/// daemon port appended.
fn normalize_daemon_base(address: &str) -> String {
    let trimmed = address.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.contains(':') {
        format!("http://{trimmed}")
    } else {
        format!("http://{trimmed}:{}", darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT)
    }
}

/// GET `<base>/mission/<mission_id>/graph.json` with the shared bearer
/// token, if one is configured. `mission_id` is trusted un-re-validated —
/// `mission_graph_json_handler` already ran it through `is_valid_catalog_id`
/// before either code path in this module runs. Every failure shape
/// (network error/timeout, 401/403, 404, a non-JSON or unparseable 200)
/// collapses to `None` — see this module's own doc for why that is the
/// right degradation, not a gap.
fn fetch_peer_graph_json(base: &str, mission_id: &str, token: Option<&str>) -> Option<serde_json::Value> {
    let url = format!("{base}/mission/{mission_id}/graph.json");
    let agent = ureq::AgentBuilder::new().timeout(PEER_GRAPH_TIMEOUT).build();
    let mut req = agent.get(&url);
    if let Some(tok) = token {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => resp.into_string().ok().and_then(|body| serde_json::from_str(&body).ok()),
        Err(_) => None,
    }
}

/// Stamp provenance into the graph's own free-text `note` field — already
/// rendered verbatim by both `MissionCanvas.tsx` and `MissionTimelineView.tsx`
/// (the port's `legacy`-graph warning slot) — rather than widening the wire
/// shape for one new field. Preserves the peer's own note (if any, e.g. its
/// `legacy: true` explanation) by appending rather than clobbering.
fn stamp_provenance(graph: &mut serde_json::Value, machine: &str) {
    let Some(obj) = graph.as_object_mut() else { return };
    let provenance = format!("fetched live from peer `{machine}`");
    let combined = match obj.get("note").and_then(|n| n.as_str()) {
        Some(existing) if !existing.is_empty() => format!("{existing} ({provenance})"),
        _ => provenance,
    };
    obj.insert("note".to_string(), serde_json::Value::String(combined));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn roster_with(id: &str, address: &str) -> FleetRoster {
        let mut roster = FleetRoster::default();
        roster.machines.insert(
            id.to_string(),
            darkmux_fleet::MachineEntry {
                id: id.to_string(),
                address: address.to_string(),
                description: None,
                added_unix_ms: 0,
            },
        );
        roster
    }

    fn live(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// One-shot HTTP responder on an ephemeral loopback port — same shape
    /// as `fetch_peer_json`'s own `one_shot_http` test helper
    /// (`src/fleet_cli.rs`). Returns the bound address and a flag the
    /// caller can check to prove whether the connection was ever made
    /// (the `Silent` case needs to prove a NEGATIVE: no network attempt at
    /// all, not merely one that failed).
    fn one_shot_http(status_line: &'static str, body: &'static str) -> (String, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let hit = Arc::new(AtomicBool::new(false));
        let hit2 = hit.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                hit2.store(true, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (addr, hit)
    }

    /// A listener that ACCEPTS but never writes a byte — the "wedged peer"
    /// shape `fetch_peer_graph_json`'s timeout has to survive, distinct
    /// from `127.0.0.1:1`'s instant connection-refused.
    fn wedged_http() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                // Hold the connection open, write nothing, drop only when
                // the process exits (test binary teardown) or the peer
                // gives up — proving the CALLER's timeout is what ends it.
                std::mem::forget(stream);
            }
        });
        addr
    }

    // ── classify_peer (pure) ─────────────────────────────────────────

    #[test]
    fn classify_peer_unrostered_when_the_id_is_absent_from_the_roster() {
        let roster = FleetRoster::default();
        assert_eq!(
            classify_peer("studio", &roster, &live(&["studio"])),
            PeerLookup::Unrostered { machine: "studio".into() }
        );
    }

    #[test]
    fn classify_peer_silent_when_rostered_but_not_in_the_live_set() {
        let roster = roster_with("studio", "127.0.0.1:9");
        assert_eq!(
            classify_peer("studio", &roster, &HashSet::new()),
            PeerLookup::Silent { machine: "studio".into() }
        );
    }

    #[test]
    fn classify_peer_live_when_rostered_and_present() {
        let roster = roster_with("studio", "127.0.0.1:9000");
        assert_eq!(
            classify_peer("studio", &roster, &live(&["studio"])),
            PeerLookup::Live { machine: "studio".into(), address: "127.0.0.1:9000".into() }
        );
    }

    // ── try_peer_graph_with — the full decision + fetch, no real network
    // beyond loopback, no Redis, no roster file on disk ─────────────────

    #[test]
    fn unrostered_owner_never_attempts_the_network() {
        let (addr, hit) = one_shot_http("200 OK", r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#);
        // The roster names a DIFFERENT machine at this address — "studio"
        // (the mission's actual owner) is absent.
        let roster = roster_with("not-studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
        assert!(!hit.load(Ordering::SeqCst), "unrostered peer must never be dialed");
    }

    #[test]
    fn silent_peer_never_attempts_the_network() {
        let (addr, hit) = one_shot_http("200 OK", r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#);
        let roster = roster_with("studio", &addr);
        // "studio" is rostered but the live set is empty — rostered but
        // silent, the #1466-continuation degraded case (CLAUDE.md:
        // "presence is fleet-membership truth").
        let out = try_peer_graph_with("m", "studio", &roster, &HashSet::new(), None);
        assert_eq!(out, None);
        assert!(!hit.load(Ordering::SeqCst), "a silent peer must never be dialed — presence already answered");
    }

    #[test]
    fn unreachable_peer_returns_none_fast() {
        // Port 1 on loopback: nothing listens, connection refused near-
        // instantly (`fetch_peer_json`'s own precedent for this exact
        // fixture, `src/fleet_cli.rs`).
        let roster = roster_with("studio", "127.0.0.1:1");
        let start = std::time::Instant::now();
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(1), "connection-refused must not wait out the 2s timeout");
    }

    #[test]
    fn wedged_peer_is_bounded_by_the_timeout_not_a_hang() {
        let addr = wedged_http();
        let roster = roster_with("studio", &addr);
        let start = std::time::Instant::now();
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None);
        let elapsed = start.elapsed();
        assert_eq!(out, None);
        assert!(elapsed < Duration::from_millis(2500), "must return within the {PEER_GRAPH_TIMEOUT:?} bound, got {elapsed:?}");
        assert!(elapsed >= Duration::from_millis(1500), "a genuinely instant return here would mean the timeout isn't being enforced, got {elapsed:?}");
    }

    #[test]
    fn auth_required_peer_returns_none() {
        let (addr, hit) = one_shot_http("401 Unauthorized", "{}");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), Some("wrong-token"));
        assert_eq!(out, None);
        assert!(hit.load(Ordering::SeqCst), "a live rostered peer IS dialed");
    }

    #[test]
    fn peers_own_404_returns_none() {
        // The peer answered — it just doesn't have this mission either
        // (older darkmux, or a genuinely cleared run there too).
        let (addr, _hit) = one_shot_http("404 Not Found", "no mission with id `m` found\n");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn malformed_200_returns_none() {
        let (addr, _hit) = one_shot_http("200 OK", "not json");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn live_reachable_peer_returns_the_graph_with_provenance_stamped() {
        let (addr, hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","mission_status":"active","nodes":[],"edges":[],"legacy":false,"generated_at_ms":123}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None)
            .expect("a live, rostered, reachable peer must return its graph");
        assert!(hit.load(Ordering::SeqCst));
        assert_eq!(out["mission_id"], "m");
        assert_eq!(out["note"], "fetched live from peer `studio`");
    }

    #[test]
    fn live_reachable_peer_appends_to_an_existing_note_rather_than_clobbering_it() {
        let (addr, _hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","mission_status":"active","nodes":[],"edges":[],"legacy":true,"note":"phases only, no task graph","generated_at_ms":123}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), None).unwrap();
        assert_eq!(out["note"], "phases only, no task graph (fetched live from peer `studio`)");
    }

    #[test]
    fn bearer_token_is_sent_when_configured() {
        // A listener that inspects the request line/headers it received
        // rather than a canned one_shot_http reply — proves the token
        // actually rides the Authorization header, not just that SOME
        // request landed.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen_auth = Arc::new(std::sync::Mutex::new(None::<String>));
        let seen_auth2 = seen_auth.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let auth = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .map(|l| l.to_string());
                *seen_auth2.lock().unwrap() = auth;
                let body = r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", &roster, &live(&["studio"]), Some("sk-shared-token"));
        assert!(out.is_some());
        let header = seen_auth.lock().unwrap().clone().expect("Authorization header must be sent");
        assert!(header.contains("Bearer sk-shared-token"), "{header}");
    }

    // ── normalize_daemon_base (mirrors src/fleet_cli.rs's own tests) ────

    #[test]
    fn normalize_daemon_base_shapes() {
        assert_eq!(normalize_daemon_base("http://studio.tailnet:9000/"), "http://studio.tailnet:9000");
        assert_eq!(normalize_daemon_base("100.64.0.2:8765"), "http://100.64.0.2:8765");
        assert_eq!(
            normalize_daemon_base("100.64.0.2"),
            format!("http://100.64.0.2:{}", darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT)
        );
    }
}
