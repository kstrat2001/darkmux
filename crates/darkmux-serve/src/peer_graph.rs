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
//! 0. **Not a relay, not self** (#1466 gate MUST FIX 1) — two guards run
//!    before anything else, both zero-I/O:
//!    - **Already relayed.** The outgoing request this module makes
//!      carries [`PEER_RELAY_HEADER`]; an INCOMING request that already
//!      carries it is a request THIS module (or a peer's own copy of it)
//!      already forwarded once, and it is never forwarded again. This
//!      caps the worst case at exactly one hop even across a multi-peer
//!      attribution cycle (A attributes to B, B attributes to C, C
//!      attributes back to A), which a same-machine check alone doesn't
//!      cover.
//!    - **Self-attribution.** darkmux's own onboarding docs
//!      (`docs/guide/always-on-hub.html`, `skills/darkmux-add-machine/
//!      SKILL.md`) tell every operator to register THIS machine in its
//!      own roster at `127.0.0.1:8765` — and a machine always beats its
//!      own presence, so it is always in the live set. Without this
//!      guard, a mission attributed to this machine but absent from this
//!      machine's OWN disk (cleared, pruned, or a `DARKMUX_HOME`
//!      mismatch between the process that ran it and the daemon reading
//!      it now) would classify as a live, dialable peer — and the daemon
//!      would dial itself, miss again, dial again. Proven live (#1466
//!      gate): a counting forwarder measured ~1100 self-dial levels/sec,
//!      each holding a blocking-pool task and two sockets, from ONE
//!      request. Compares the attributed owner against
//!      `darkmux_flow::resolve_machine_id()` — the SAME resolution every
//!      flow record's own `machine_id` is stamped from — before
//!      `classify_peer` ever runs.
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
//! 4. **Reachable + authorized + answers a SHAPE-VALID graph** — only now
//!    does this module make a network call, bounded at
//!    [`PEER_GRAPH_TIMEOUT`] so a slow or wedged peer degrades the SAME
//!    way `fetch_peer_json` (`src/fleet_cli.rs`) already degrades for
//!    every other cross-machine read: unreachable, a redirect (never
//!    followed — see [`fetch_peer_graph_json`]'s own doc, #1466 gate MUST
//!    FIX 2), a 401/403 (this machine isn't sending the shared bearer
//!    token), the peer's own 404 (genuinely not on ITS disk either — an
//!    older darkmux, or a truly cleared run), a malformed body, and a
//!    well-formed-but-wrong-shaped body (any JSON that parses but isn't a
//!    `{nodes: [...], edges: [...], mission_id: "<the one we asked for>"}`
//!    object — #1466 gate MUST FIX 3, see
//!    [`graph_shape_is_valid`]) are all [`None`].
//!
//! **Every non-success outcome collapses to [`None`], on purpose.** The
//! caller's existing 404 body is unchanged, and the viewer's existing
//! `lookupOwningMachine` honesty fallback (#1656) already turns that 404
//! into "ran on `<machine>`, open darkmux there" from the SAME flow-record
//! attribution this module computes first — so every degraded case here
//! still lands on an already-correct, already-tested UI state. Only the
//! REACHABLE-AND-SHAPE-VALID case is new behavior. darkmux describes,
//! never adjudicates: this module never renders a verdict about the peer
//! or the operator's network, it either returns the peer's own graph or
//! gets out of the way — but a failure IS named to stderr per-outcome
//! (unreachable / unauthorized / peer-404 / malformed / wrong-shape),
//! mirroring `fetch_peer_json`'s own precedent of naming the OBSERVED
//! outcome rather than collapsing every cause into one silent [`None`].

use std::collections::HashSet;
use std::io::Read;
use std::path::Path as StdPath;
use std::time::Duration;

use darkmux_fleet::FleetRoster;

/// Bounded at 2s — matches `fetch_peer_json`'s (`src/fleet_cli.rs`) own
/// timeout for the identical reason: one page load per peer, even a slow
/// or wedged one, never an indefinite hang. This call runs inside a
/// `spawn_blocking` off the request future (`mission_graph_json_handler`),
/// not the CLI's one-shot process — the operator-facing contract (bounded,
/// legible, never silent) is the same either way.
///
/// **DNS caveat (#1466 gate CONSIDER 9), stated rather than silently
/// assumed away:** ureq's own docs note that slow DNS resolution can
/// exceed a configured timeout because the resolution step itself isn't
/// interruptible — and a roster address is typically a MagicDNS name
/// (Tailscale), not a bare IP. In practice this bound is "usually 2s,
/// occasionally longer on a slow/wedged resolver," not an absolute
/// ceiling; the request still terminates (DNS resolution itself fails or
/// succeeds, it doesn't hang forever), so the daemon-level hazard this
/// module exists to avoid (recursion, an unbounded fan-out) is unaffected
/// — this is a timing-precision caveat, not a correctness gap.
const PEER_GRAPH_TIMEOUT: Duration = Duration::from_millis(2000);

/// (#1466 gate MUST FIX 1, belt-and-braces) Set to `"1"` on every outgoing
/// request this module makes. An INCOMING request to
/// `mission_graph_json_handler` that already carries this header is one
/// THIS module (or a peer's own copy of it) already relayed once, and the
/// handler skips [`try_peer_graph`] entirely rather than relay it again —
/// see `mission_graph_json_handler`'s own doc. This bounds the worst case
/// at exactly one hop even across a multi-peer attribution cycle (A
/// attributes to B, B attributes to C, C attributes back to A) that the
/// same-machine check below doesn't cover on its own.
pub(crate) const PEER_RELAY_HEADER: &str = "x-darkmux-peer-relay";

/// (#1466 gate CONSIDER 7) A mission graph is small, structured JSON — a
/// handful of nodes and edges. There is no legitimate reason for a live
/// peer's answer to approach this size, so the cap is set far below
/// ureq's own built-in 10 MiB `into_string()` ceiling rather than relying
/// on that ceiling alone: a peer (hostile, compromised, or just
/// misconfigured to answer this route with something else) can otherwise
/// make this daemon buffer and parse up to that much per request.
const MAX_PEER_GRAPH_BYTES: u64 = 2 * 1024 * 1024;

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
/// say ran it. `Some` only for a genuine, shape-valid graph from a live,
/// rostered, reachable, authorized, non-self peer — every other reason is
/// `None` (see this module's own doc); the caller's existing 404 already
/// covers all of them honestly.
///
/// `already_relayed` is the belt-and-braces guard's input — the caller
/// (`mission_graph_json_handler`) reads it off the INCOMING request's own
/// [`PEER_RELAY_HEADER`] before this function ever runs, so a request
/// that already crossed one peer hop is refused a second hop here with
/// zero I/O.
pub(crate) fn try_peer_graph(
    mission_id: &str,
    flows_dir: &StdPath,
    fleet_records: &[serde_json::Value],
    already_relayed: bool,
) -> Option<serde_json::Value> {
    if already_relayed {
        return None;
    }
    // Short-circuits BEFORE any roster/Redis I/O for the common case (no
    // flow record anywhere names a machine for this mission) — the same
    // case `mission_graph_json_returns_404_for_unknown_mission` pins.
    let owner = crate::runs::mission_owner_machine(flows_dir, fleet_records, mission_id)?;
    // (#1466 gate MUST FIX 1) The SAME resolution every flow record's own
    // `machine_id` is stamped from — see `resolve_machine_id`'s own doc.
    let self_machine = darkmux_flow::resolve_machine_id();
    let roster = darkmux_fleet::load_roster().ok()?;
    let live = live_machine_names();
    let token = darkmux_flow::serve_token();
    try_peer_graph_with(
        mission_id,
        &owner,
        self_machine.as_deref(),
        &roster,
        &live,
        token.as_ref().map(|t| t.expose_for_compare()),
    )
}

/// The dependency-injected core of [`try_peer_graph`] — the attributed
/// owner, this machine's own identity, roster, live-set, and bearer token
/// all supplied by the caller rather than read from the real environment.
/// Lets every branch (self-dial / unrostered / silent / live+unreachable
/// / live+401 / live+404 / live+wrong-shape / live+success) be exercised
/// against a real loopback HTTP fixture with zero Redis and zero roster
/// file on disk — see this module's `#[cfg(test)]` block.
///
/// `already_relayed` is checked one level up in [`try_peer_graph`], not
/// here — it needs no roster/live-set/token to decide, so it never
/// reaches this DI core at all.
fn try_peer_graph_with(
    mission_id: &str,
    owner: &str,
    self_machine: Option<&str>,
    roster: &FleetRoster,
    live_machines: &HashSet<String>,
    token: Option<&str>,
) -> Option<serde_json::Value> {
    // (#1466 gate MUST FIX 1) Compared BEFORE `classify_peer` runs, per
    // this module's own doc — a mission this machine attributes to
    // itself (cleared/pruned locally, or a `DARKMUX_HOME` mismatch) must
    // never be treated as a dialable peer, no matter how it classifies.
    if self_machine == Some(owner) {
        return None;
    }
    let PeerLookup::Live { machine, address } = classify_peer(owner, roster, live_machines) else {
        return None;
    };
    let base = normalize_daemon_base(&address);
    let mut graph = fetch_peer_graph_json(&base, mission_id, &machine, token)?;
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
/// token (if configured) and the belt-and-braces relay marker
/// ([`PEER_RELAY_HEADER`]). `mission_id` is trusted un-re-validated —
/// `mission_graph_json_handler` already ran it through `is_valid_catalog_id`
/// before either code path in this module runs (a bare `..` passes that
/// charset check, same as any other path-safe token, but the `url` crate
/// normalizes it away harmlessly in path-segment position — there is no
/// traversal surface here to re-validate against).
///
/// `.redirects(0)` (#1466 gate MUST FIX 2): without it, ureq follows a
/// peer-supplied `Location` header up to 5 hops by default — including to
/// a SECOND host, since `Location` is entirely peer-controlled and this
/// call has no way to know in advance it points back at the peer that was
/// actually dialed. Proven live (#1466 gate): a rostered listener
/// answering `302 Location: http://127.0.0.1:<other-port>/v1/models` had
/// the daemon return the SECOND server's body as the mission graph,
/// provenance stamped, from a host the roster never named — an
/// arbitrary-URL read primitive against THIS host's own loopback-only
/// services (the model host, or the daemon's own loopback-exempt routes).
/// A redirect response still comes back `Ok` from ureq with
/// `redirects(0)` (only >=400 maps to `Err`), so the explicit `status()
/// != 200` check below is what actually rejects it — the target here is
/// always a known fixed route on a known rostered peer; no legitimate
/// redirect exists for this call to follow.
///
/// Every failure shape collapses to `None` (see this module's own doc for
/// why that is the right degradation, not a gap) but each is named to
/// stderr with the OBSERVED outcome — a wrong fleet token, an unrostered
/// peer, and a dead peer used to be indistinguishable and unlogged;
/// naming the outcome (never a verdict about the peer or the operator's
/// network) matches `fetch_peer_json`'s own precedent in
/// `src/fleet_cli.rs` (#1466 gate CONSIDER 6).
fn fetch_peer_graph_json(base: &str, mission_id: &str, peer_id: &str, token: Option<&str>) -> Option<serde_json::Value> {
    let url = format!("{base}/mission/{mission_id}/graph.json");
    let agent = ureq::AgentBuilder::new()
        .timeout(PEER_GRAPH_TIMEOUT)
        .redirects(0)
        .build();
    let mut req = agent.get(&url).set(PEER_RELAY_HEADER, "1");
    if let Some(tok) = token {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    let resp = match req.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            eprintln!(
                "peer_graph: peer `{peer_id}` ({url}) requires a bearer token this machine isn't sending"
            );
            return None;
        }
        Err(ureq::Error::Status(404, _)) => {
            eprintln!("peer_graph: peer `{peer_id}` ({url}) answered 404 — no mission there either");
            return None;
        }
        Err(ureq::Error::Status(status, _)) => {
            eprintln!("peer_graph: peer `{peer_id}` ({url}) answered HTTP {status}");
            return None;
        }
        Err(e) => {
            eprintln!("peer_graph: peer `{peer_id}` ({url}) unreachable: {e}");
            return None;
        }
    };
    // (#1466 gate MUST FIX 2) A 3xx lands here (ureq only turns >=400 into
    // `Err`) — reject anything but an exact 200 rather than trying to
    // special-case redirect statuses.
    if resp.status() != 200 {
        eprintln!(
            "peer_graph: peer `{peer_id}` ({url}) answered HTTP {} — not a 200, not followed",
            resp.status()
        );
        return None;
    }
    // (#1466 gate CONSIDER 7) Cheap reject before spending the read: a
    // mission graph is `application/json` (this daemon's own
    // `axum::Json` handler always sets it); anything else answering this
    // route isn't the route we asked for.
    if !resp.content_type().eq_ignore_ascii_case("application/json") {
        eprintln!(
            "peer_graph: peer `{peer_id}` ({url}) answered with content-type `{}`, not application/json",
            resp.content_type()
        );
        return None;
    }
    // (#1466 gate CONSIDER 7) A tighter cap than ureq's own 10 MiB
    // `into_string()` ceiling — a mission graph is small, structured
    // JSON; there's no legitimate reason for one to approach this size.
    let mut buf: Vec<u8> = Vec::new();
    if resp
        .into_reader()
        .take(MAX_PEER_GRAPH_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        eprintln!("peer_graph: peer `{peer_id}` ({url}) response could not be read");
        return None;
    }
    if buf.len() as u64 > MAX_PEER_GRAPH_BYTES {
        eprintln!(
            "peer_graph: peer `{peer_id}` ({url}) response exceeded the {MAX_PEER_GRAPH_BYTES}-byte cap"
        );
        return None;
    }
    let body = match String::from_utf8(buf) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("peer_graph: peer `{peer_id}` ({url}) response was not valid UTF-8");
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("peer_graph: peer `{peer_id}` ({url}) response was not valid JSON");
            return None;
        }
    };
    // (#1466 gate MUST FIX 3) A parsed 200 is only trusted as a mission
    // graph when its shape actually matches one — see
    // `graph_shape_is_valid`'s own doc for what "matches" means and why
    // the collapsed-to-404 honesty fallback depends on this.
    if !graph_shape_is_valid(&value, mission_id) {
        eprintln!(
            "peer_graph: peer `{peer_id}` ({url}) response did not shape-validate as mission \
             `{mission_id}`'s graph (wrong shape, or a graph for a different mission)"
        );
        return None;
    }
    Some(value)
}

/// Whether a parsed peer response is trustworthy enough to treat as
/// `mission_id`'s graph (#1466 gate MUST FIX 3). Before this check, ANY
/// well-formed JSON a peer's port happened to answer with became a 200:
/// a model-list payload, a bare array (not even an object, so it can't
/// carry the provenance stamp either), or a graph answering for a
/// DIFFERENT mission than the one this call requested — nothing compared
/// them. That matters operationally: the viewer's honesty fallback
/// (`lookupOwningMachine`, #1656) only fires on a 404
/// (`ui/src/lenses/mission/MissionGraphLens.tsx`), and both
/// `ui/src/lenses/mission/graph.ts` and the lens itself read `.nodes`
/// unguarded — so a wrong-shape 200 used to throw during render instead
/// of landing on the tested 404 UI state this module's own doc promises.
///
/// A real mission graph (`mission_graph::MissionGraph`, see that
/// module) is an object carrying `nodes` and `edges` arrays plus its own
/// `mission_id`; this checks exactly those three things and nothing
/// deeper (it isn't re-validating every node/edge field — a peer that
/// answers the right shape for the right mission is trusted the same way
/// it was before this fix, only a WRONG shape or a WRONG mission id is
/// now rejected).
fn graph_shape_is_valid(value: &serde_json::Value, mission_id: &str) -> bool {
    let Some(obj) = value.as_object() else { return false };
    let nodes_ok = matches!(obj.get("nodes"), Some(serde_json::Value::Array(_)));
    let edges_ok = matches!(obj.get("edges"), Some(serde_json::Value::Array(_)));
    let id_matches = obj.get("mission_id").and_then(|v| v.as_str()) == Some(mission_id);
    nodes_ok && edges_ok && id_matches
}

/// Stamp provenance into the graph's own free-text `note` field — already
/// rendered verbatim by both `MissionCanvas.tsx` and `MissionTimelineView.tsx`
/// (the port's `legacy`-graph warning slot) — rather than widening the wire
/// shape for one new field.
///
/// **Always overwrites the peer's own `note`, never appends to it** (#1466
/// gate CONSIDER 5 — changed from the original append-based behavior). The
/// `note` field is free text the PEER controls, so appending let a peer
/// return its own forged `"fetched live from peer `trusted-hub`"` text —
/// the forgery would read first and this function's honest stamp would
/// read like a trailing parenthetical, on a field the canvas and timeline
/// render verbatim. `note` isn't the peer's only channel for that
/// information: `legacy: true` (a plain bool the graph shape already
/// carries) is what actually gates the port's legacy-graph warning UI —
/// losing a peer's own free-text explanation of ITS `legacy: true` is a
/// real but small richness regression against a forgery vector on a
/// field the operator reads as this daemon's own attestation.
fn stamp_provenance(graph: &mut serde_json::Value, machine: &str) {
    let Some(obj) = graph.as_object_mut() else { return };
    obj.insert(
        "note".to_string(),
        serde_json::Value::String(format!("fetched live from peer `{machine}`")),
    );
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
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
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
        let out = try_peer_graph_with("m", "studio", None, &roster, &HashSet::new(), None);
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
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(1), "connection-refused must not wait out the 2s timeout");
    }

    #[test]
    fn wedged_peer_is_bounded_by_the_timeout_not_a_hang() {
        let addr = wedged_http();
        let roster = roster_with("studio", &addr);
        let start = std::time::Instant::now();
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        let elapsed = start.elapsed();
        assert_eq!(out, None);
        assert!(elapsed < Duration::from_millis(2500), "must return within the {PEER_GRAPH_TIMEOUT:?} bound, got {elapsed:?}");
        assert!(elapsed >= Duration::from_millis(1500), "a genuinely instant return here would mean the timeout isn't being enforced, got {elapsed:?}");
    }

    #[test]
    fn auth_required_peer_returns_none() {
        let (addr, hit) = one_shot_http("401 Unauthorized", "{}");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), Some("wrong-token"));
        assert_eq!(out, None);
        assert!(hit.load(Ordering::SeqCst), "a live rostered peer IS dialed");
    }

    #[test]
    fn peers_own_404_returns_none() {
        // The peer answered — it just doesn't have this mission either
        // (older darkmux, or a genuinely cleared run there too).
        let (addr, _hit) = one_shot_http("404 Not Found", "no mission with id `m` found\n");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn malformed_200_returns_none() {
        let (addr, _hit) = one_shot_http("200 OK", "not json");
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn live_reachable_peer_returns_the_graph_with_provenance_stamped() {
        let (addr, hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","mission_status":"active","nodes":[],"edges":[],"legacy":false,"generated_at_ms":123}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None)
            .expect("a live, rostered, reachable peer must return its graph");
        assert!(hit.load(Ordering::SeqCst));
        assert_eq!(out["mission_id"], "m");
        assert_eq!(out["note"], "fetched live from peer `studio`");
    }

    #[test]
    fn live_reachable_peer_overwrites_the_peers_own_note_rather_than_appending_it() {
        // (#1466 gate CONSIDER 5) `note` is free text the PEER controls. A
        // peer could return its own forged provenance ("fetched live from
        // peer `trusted-hub`") and, under the old append-based behavior,
        // that forgery would read FIRST with this function's own honest
        // stamp trailing as a parenthetical. Overwriting instead means the
        // rendered `note` is always this function's own statement, never
        // peer-influenced text this daemon didn't itself assert.
        let (addr, _hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","mission_status":"active","nodes":[],"edges":[],"legacy":true,"note":"fetched live from peer `trusted-hub` (totally legit)","generated_at_ms":123}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None).unwrap();
        assert_eq!(
            out["note"], "fetched live from peer `studio`",
            "the peer's own note text must never survive into the rendered provenance"
        );
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
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), Some("sk-shared-token"));
        assert!(out.is_some());
        let header = seen_auth.lock().unwrap().clone().expect("Authorization header must be sent");
        assert!(header.contains("Bearer sk-shared-token"), "{header}");
    }

    #[test]
    fn outgoing_request_carries_the_relay_header() {
        // (#1466 gate MUST FIX 1, belt-and-braces) Proves the header this
        // module's own doc promises is actually attached to every
        // outgoing request — the SAME header `mission_graph_json_handler`
        // checks on the way IN before ever calling `try_peer_graph`.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(std::sync::Mutex::new(false));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let marker = format!("{}:", PEER_RELAY_HEADER);
                *seen2.lock().unwrap() =
                    req.lines().any(|l| l.to_ascii_lowercase().starts_with(&marker));
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
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert!(out.is_some());
        assert!(
            *seen.lock().unwrap(),
            "outgoing request must carry the relay marker header"
        );
    }

    // ── #1466 gate MUST FIX 1: self-dial + already-relayed guards ──────

    #[test]
    fn self_dial_owner_never_attempts_the_network() {
        // darkmux's own onboarding docs (`docs/guide/always-on-hub.html`,
        // `skills/darkmux-add-machine/SKILL.md`) tell an operator to
        // register THIS machine in its own roster at `127.0.0.1:8765` —
        // and a machine always beats its own presence, so it's always in
        // the live set. Without the self-machine guard, a mission
        // attributed to "studio" while THIS daemon IS "studio" would
        // classify `Live` and get dialed — the shape a real recursion
        // probe (#1466 gate) measured at ~1100 self-dial levels/sec from
        // ONE request, each level holding a blocking-pool task and two
        // sockets.
        let (addr, hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", Some("studio"), &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
        assert!(
            !hit.load(Ordering::SeqCst),
            "this machine must never dial its own roster entry for a mission attributed to itself"
        );
    }

    #[test]
    fn self_dial_guard_does_not_trip_a_genuinely_different_owner() {
        // Guards the guard: a self-check that's too eager (e.g. two
        // unresolved identities both reading as the same thing) must not
        // swallow a legitimate, different peer.
        let (addr, hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", Some("laptop"), &roster, &live(&["studio"]), None)
            .expect("a genuinely different owner must still be dialed");
        assert!(hit.load(Ordering::SeqCst));
        assert_eq!(out["mission_id"], "m");
    }

    #[test]
    #[serial_test::serial]
    fn already_relayed_request_short_circuits_with_no_attribution() {
        // The cheap half of the belt-and-braces proof: with nothing
        // attributing the mission to any machine, `already_relayed`
        // returning `None` immediately is indistinguishable from the
        // normal "unattributed" `None` on outcome alone — this only
        // proves the call doesn't panic/misbehave on the cheapest
        // possible input. The DECISIVE proof — that the guard actually
        // prevents a dial that would otherwise happen — is the next test,
        // `already_relayed_request_is_never_dialed_even_when_the_owner_is_live_and_reachable`,
        // which sets up a genuinely attributed, rostered, LIVE peer and
        // shows it is never touched. `DARKMUX_HOME` is scoped to a
        // scratch tempdir purely so this test can never touch the
        // operator's real `~/.darkmux` if the short-circuit ever
        // regresses and execution proceeds further than it should.
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let out = try_peer_graph("nonexistent-mission-xyz", tmp.path(), &[], true);
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(out, None);
    }

    fn redis_server_available_for_peer_graph_tests() -> bool {
        std::process::Command::new("redis-server")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    struct RelayTestRedis {
        child: std::process::Child,
        url: String,
    }

    impl Drop for RelayTestRedis {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Minimal local twin of `lib_tests.rs`'s own `spawn_redis` — an
    /// ephemeral, throwaway `redis-server` this ONE test spawns and
    /// kills, never the operator's real Redis. Kept local rather than
    /// shared: this module's test list only needs it for this single
    /// end-to-end belt-and-braces proof.
    fn spawn_redis_for_peer_graph_tests() -> RelayTestRedis {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("redis-server")
            .args([
                "--port", &port.to_string(),
                "--save", "",
                "--appendonly", "no",
                "--bind", "127.0.0.1",
                "--protected-mode", "no",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("redis-server spawn");
        let url = format!("redis://127.0.0.1:{port}");
        let client = redis::Client::open(url.as_str()).expect("redis client");
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Ok(mut conn) = client.get_connection() {
                if let Ok(pong) = redis::cmd("PING").query::<String>(&mut conn) {
                    if pong == "PONG" {
                        return RelayTestRedis { child, url };
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("redis-server did not become ready");
    }

    #[test]
    #[serial_test::serial]
    fn already_relayed_request_is_never_dialed_even_when_the_owner_is_live_and_reachable() {
        // The decisive half: a mission genuinely ATTRIBUTED to a peer
        // that is ROSTERED and LIVE — exactly the conditions under which
        // `try_peer_graph` would otherwise dial it — is still never
        // dialed when the incoming request already carries the relay
        // marker. This is the real end-to-end shape of the belt-and-
        // braces guard (#1466 gate MUST FIX 1): it bounds a multi-peer
        // attribution cycle (A→B→C→A) to exactly one hop, a case the
        // same-machine check alone doesn't cover.
        if !redis_server_available_for_peer_graph_tests() {
            eprintln!("skipping: redis-server not on PATH");
            return;
        }
        const MISSION_ID: &str = "already-relayed-live-owner-mission";
        const PEER: &str = "relay-test-peer";

        let redis = spawn_redis_for_peer_graph_tests();
        let (addr, hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"already-relayed-live-owner-mission","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#,
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let flows_dir = tmp.path().join("flows");
        std::fs::create_dir_all(&flows_dir).unwrap();
        let today = darkmux_flow::day_utc_now();
        let day_file = flows_dir.join(format!("{today}.jsonl"));
        std::fs::write(
            &day_file,
            format!(
                r#"{{"ts":"{today}T10:00:00Z","action":"mission start","mission_id":"{MISSION_ID}","machine_id":"{PEER}"}}"#
            ) + "\n",
        )
        .unwrap();

        let fleet_file = tmp.path().join("fleet.json");
        let prev_fleet = std::env::var("DARKMUX_FLEET_FILE").ok();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", &fleet_file);
            std::env::set_var("DARKMUX_REDIS_URL", &redis.url);
        }
        darkmux_fleet::mutate_roster(|roster| {
            darkmux_fleet::add_machine(roster, PEER, &addr, None)?;
            Ok(())
        })
        .unwrap();
        let redis_client = redis::Client::open(redis.url.as_str()).unwrap();
        let beat = darkmux_flow::presence::PresenceBeat {
            machine_uid: "uid-relay-test-peer".to_string(),
            display_name: PEER.to_string(),
            schema_version: "1.0.0".to_string(),
            beat_ts_ms: darkmux_flow::presence::now_ms(),
            specs: None,
            loaded_models: Vec::new(),
            darkmux_version: None,
        };
        darkmux_flow::presence::write_beat(&redis_client, &beat, 60).unwrap();

        let out = try_peer_graph(MISSION_ID, &flows_dir, &[], true);

        unsafe {
            match &prev_fleet {
                Some(v) => std::env::set_var("DARKMUX_FLEET_FILE", v),
                None => std::env::remove_var("DARKMUX_FLEET_FILE"),
            }
            match &prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        assert_eq!(out, None);
        assert!(
            !hit.load(Ordering::SeqCst),
            "an already-relayed request must never be dialed, even when its attributed owner \
             is rostered AND live — the exact conditions under which it would otherwise be dialed"
        );
    }

    // ── #1466 gate MUST FIX 2: redirects are never followed ────────────

    #[test]
    fn redirect_is_never_followed_to_a_second_host() {
        // The `Location` header is entirely peer-controlled. Proven live
        // (#1466 gate): a rostered peer answering 302 to a SECOND
        // loopback service had ureq follow it and return that second
        // server's body as the mission graph, provenance stamped — an
        // arbitrary-URL read primitive against this host's own
        // loopback-only services (the model host, or the daemon's own
        // loopback-exempt routes).
        let (addr2, hit2) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#,
        );
        let location = format!("http://{addr2}/mission/m/graph.json");
        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr1 = listener1.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener1.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        let roster = roster_with("studio", &addr1);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None, "a redirect must never resolve to a peer's graph");
        assert!(
            !hit2.load(Ordering::SeqCst),
            "the redirect target must never be dialed — MUST FIX 2 requires redirects(0)"
        );
    }

    // ── #1466 gate MUST FIX 3: shape validation ─────────────────────────

    #[test]
    fn model_list_json_is_rejected_not_returned_as_a_graph() {
        // Before this fix ANY well-formed JSON became a 200 — including
        // a payload from a completely different route (an LMStudio-style
        // model list).
        let (addr, _hit) = one_shot_http("200 OK", r#"{"object":"list","data":[{"id":"qwen3"}]}"#);
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn bare_array_is_rejected_not_returned_as_a_graph() {
        let (addr, _hit) = one_shot_http("200 OK", r#"[1,2,3]"#);
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn missing_nodes_or_edges_arrays_are_rejected() {
        let (addr, _hit) = one_shot_http("200 OK", r#"{"mission_id":"m","legacy":false}"#);
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn mismatched_mission_id_is_rejected() {
        let (addr, _hit) = one_shot_http(
            "200 OK",
            r#"{"mission_id":"someone-elses-mission","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#,
        );
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(
            out, None,
            "a graph for a different mission id must never be accepted"
        );
    }

    // ── #1466 gate CONSIDER 7: content-type + size cap ─────────────────

    #[test]
    fn non_json_content_type_is_rejected_even_with_a_json_looking_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
    }

    #[test]
    fn oversized_response_is_rejected() {
        // A body larger than `MAX_PEER_GRAPH_BYTES` must never be parsed,
        // even though it would otherwise be valid JSON — the size cap is
        // the thing under test, not a JSON-parse failure, so the padding
        // lives inside a string value.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let padding = "a".repeat((MAX_PEER_GRAPH_BYTES + 1024) as usize);
                let body = format!(
                    r#"{{"mission_id":"m","nodes":[],"edges":[],"legacy":false,"generated_at_ms":0,"pad":"{padding}"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        let roster = roster_with("studio", &addr);
        let out = try_peer_graph_with("m", "studio", None, &roster, &live(&["studio"]), None);
        assert_eq!(out, None);
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
