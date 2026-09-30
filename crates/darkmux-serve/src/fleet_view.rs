//! The fleet view: one row per roster machine, each carrying that machine's
//! own card. Every daemon gathers its own view (`GET /fleet/view`), and
//! `darkmux machine list` prints the same gather, so any machine asking gets
//! the same answer with no hub in the way.
//!
//! **Where a card comes from.** From the peer itself, over the verified peer
//! path: `darkmux_fleet::peer_target` checks the roster address is the
//! pinned overlay node before `darkmux_fleet::fleet_get` sends the fleet
//! token. Cards never come from Redis. Presence is a liveness hint only,
//! because anything that can write the shared Redis could forge a key, and a
//! card drives routing and data-boundary decisions.
//!
//! **How.** Peers are asked in parallel, each with a short timeout; the
//! whole view is single-flight and cached for [`FLEET_VIEW_CACHE_TTL`]
//! (recorded in the view as `cache_ttl_ms`, never adaptive). A peer that
//! presence says is gone is not dialed. This machine's own card is built
//! locally, with no HTTP.
//!
//! **What a row can say.** Exactly one of: the peer's card; "card
//! unavailable" (an older darkmux with no `/machine/card` route, or a card on
//! another schema major), which is not an error; or "unreachable" with a
//! typed reason. The view never fills in a fact a card did not state.

use crate::machine_card::{gather_local_card, MachineCard, CARD_SCHEMA_VERSION};
use crate::source_state::SourceState;
use crate::wire::RosterMachineEntry;
use darkmux_fleet::{FleetRoster, IdentityProvider, MachineEntry};
use darkmux_flow::presence::PresenceBeat;
use serde::Serialize;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// How long a gathered view is served before the next request gathers again.
/// Recorded in every view as `cache_ttl_ms`.
pub(crate) const FLEET_VIEW_CACHE_TTL: Duration = Duration::from_secs(5);

/// The longest one peer may take to answer its card request.
const PEER_CARD_TIMEOUT: Duration = Duration::from_millis(2000);

/// The most a peer's card may weigh: a card is a few KB, and a peer that
/// answers with more is not sending one.
const MAX_PEER_CARD_BYTES: u64 = 1024 * 1024;

/// The longest single string a peer's card may carry into this view.
const PEER_FIELD_MAX_CHARS: usize = 200;

/// Whether a roster machine has a live presence beat. Presence is a hint:
/// `unknown` when the shared Redis is off or could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Live,
    Gone,
    Unknown,
}

/// Why a card could not be read from a machine that may be up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum UnreachableReason {
    /// Presence reports no live beat, so the peer was not dialed.
    PresenceGone,
    /// The roster address is not the peer's pinned overlay node: nothing, and
    /// no token, was sent.
    Unverified,
    /// The peer answered 401 or 403: this machine's fleet token is missing or
    /// not the peer's.
    AuthRequired,
    /// The connection failed or timed out.
    ConnectFailed,
    /// The peer answered, with something that is not a card.
    BadAnswer,
}

/// What asking one machine for its card produced.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CardOutcome {
    /// The machine's own card.
    Available { card: Box<MachineCard> },
    /// The machine answered, but has no card this darkmux reads: it is on a
    /// darkmux without the route, or its card is on another schema major.
    /// Not an error.
    Unavailable {
        /// The darkmux version the peer reports, when it did.
        peer_version: Option<String>,
    },
    /// No card could be had.
    Unreachable {
        reason: UnreachableReason,
        /// The transport's word for a connection failure, or the HTTP status.
        detail: Option<String>,
    },
}

/// One roster machine and what it said about itself.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetMachine {
    pub entry: RosterMachineEntry,
    pub is_this_machine: bool,
    pub liveness: Liveness,
    /// The presence beat's own timestamp; `null` without a live beat.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub last_beat_ms: Option<u64>,
    pub card: CardOutcome,
}

/// `GET /fleet/view`: every roster machine and its card, as this daemon
/// gathered them.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetView {
    pub local_machine_id: Option<String>,
    /// How completely presence answered; a row's `liveness` is `unknown` when
    /// it did not.
    pub presence: SourceState,
    /// Why `machines` is empty when the roster file exists and could not be
    /// parsed; `null` otherwise. A fixed sentence.
    pub roster_error: Option<String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub fetched_at_ms: u64,
    /// The cache TTL this view was gathered under; `0` for a gather that is
    /// not cached (the CLI's).
    #[cfg_attr(test, ts(type = "number"))]
    pub cache_ttl_ms: u64,
    /// What gathering the view cost, in milliseconds.
    #[cfg_attr(test, ts(type = "number"))]
    pub gather_ms: u64,
    pub machines: Vec<FleetMachine>,
}

impl FleetView {
    /// The view without any peer's caller-scoped `accepts` block: what a
    /// reader other than this machine may see, since those blocks state what
    /// each peer lets THIS machine do.
    pub fn without_accepts(mut self) -> Self {
        for m in &mut self.machines {
            if let CardOutcome::Available { card } = &mut m.card {
                card.accepts = None;
            }
        }
        self
    }
}

/// Everything a gather reads, so the gather itself is the same code in the
/// daemon, in the CLI and under a test that scripts the peers.
pub(crate) trait FleetSources: Send + Sync {
    fn local_machine_id(&self) -> Option<String>;
    /// The roster, or the fixed sentence for a roster file that exists and
    /// does not parse.
    fn roster(&self) -> Result<FleetRoster, &'static str>;
    fn presence(&self) -> (Vec<PresenceBeat>, SourceState);
    fn local_card(&self) -> MachineCard;
    /// Ask one peer for its card. Blocking. `known_version` is the darkmux
    /// version its presence beat reported, for a peer with no card route.
    fn peer_card(&self, entry: &MachineEntry, known_version: Option<&str>) -> CardOutcome;
}

/// What a gather does about one roster machine.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// This machine: build the card locally.
    Local,
    /// Presence says it is gone: do not dial.
    Skip,
    /// Ask the peer.
    Ask,
}

fn plan_for(is_self: bool, liveness: Liveness) -> Plan {
    match (is_self, liveness) {
        (true, _) => Plan::Local,
        (false, Liveness::Gone) => Plan::Skip,
        (false, Liveness::Live | Liveness::Unknown) => Plan::Ask,
    }
}

/// The beat that belongs to a roster entry: by hardware identity when the
/// entry declares one, else by the name flow records carry.
fn beat_for<'a>(entry: &MachineEntry, beats: &'a [PresenceBeat]) -> Option<&'a PresenceBeat> {
    beats.iter().find(|b| match entry.machine_uid.as_deref() {
        Some(uid) => b.machine_uid.eq_ignore_ascii_case(uid),
        None => darkmux_fleet::same_machine(&b.display_name, &entry.id),
    })
}

fn liveness_of(presence: &SourceState, beat: Option<&PresenceBeat>) -> Liveness {
    match (presence, beat) {
        (SourceState::Ok, Some(_)) => Liveness::Live,
        (SourceState::Ok, None) => Liveness::Gone,
        (SourceState::Off | SourceState::Stale { .. } | SourceState::Unavailable { .. }, _) => Liveness::Unknown,
    }
}

fn gone_outcome() -> CardOutcome {
    CardOutcome::Unreachable { reason: UnreachableReason::PresenceGone, detail: None }
}

fn unfinished_outcome() -> CardOutcome {
    CardOutcome::Unreachable {
        reason: UnreachableReason::BadAnswer,
        detail: Some("the card fetch did not finish".to_string()),
    }
}

/// Gather the view: roster, presence, then every card at once. `ttl` is the
/// cache TTL this gather is served under (recorded, not enforced here).
pub(crate) fn gather_view(src: &dyn FleetSources, ttl: Duration) -> FleetView {
    let started = std::time::Instant::now();
    let local_id = src.local_machine_id();
    let (roster, roster_error) = match src.roster() {
        Ok(r) => (r, None),
        Err(sentence) => (FleetRoster::default(), Some(sentence.to_string())),
    };
    let (beats, presence) = src.presence();
    let rows: Vec<(&MachineEntry, bool, Liveness, Option<&PresenceBeat>)> = roster
        .machines
        .values()
        .map(|entry| {
            let is_self = local_id.as_deref().is_some_and(|l| darkmux_fleet::same_machine(l, &entry.id));
            let beat = beat_for(entry, &beats);
            let liveness = if is_self { Liveness::Live } else { liveness_of(&presence, beat) };
            (entry, is_self, liveness, beat)
        })
        .collect();
    let outcomes: Vec<CardOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = rows
            .iter()
            .map(|(entry, is_self, liveness, beat)| {
                let plan = plan_for(*is_self, *liveness);
                let known_version = beat.and_then(|b| b.darkmux_version.as_deref());
                scope.spawn(move || match plan {
                    Plan::Local => CardOutcome::Available { card: Box::new(src.local_card()) },
                    Plan::Skip => gone_outcome(),
                    Plan::Ask => src.peer_card(entry, known_version),
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| unfinished_outcome())).collect()
    });
    let machines = rows
        .iter()
        .zip(outcomes)
        .map(|((entry, is_self, liveness, beat), card)| FleetMachine {
            entry: RosterMachineEntry::from(*entry),
            is_this_machine: *is_self,
            liveness: *liveness,
            last_beat_ms: beat.map(|b| b.beat_ts_ms),
            card,
        })
        .collect();
    FleetView {
        local_machine_id: local_id,
        presence,
        roster_error,
        fetched_at_ms: crate::current_millis(),
        cache_ttl_ms: ttl.as_millis() as u64,
        gather_ms: started.elapsed().as_millis() as u64,
        machines,
    }
}

// ─── the cache ─────────────────────────────────────────────────────────────

/// The gathered view, kept for [`FLEET_VIEW_CACHE_TTL`]. Single-flight: a
/// burst of requests on a cold cache costs ONE gather (the same pattern as
/// `/machine/resources`). Wall-clock time, not `Instant`, so a daemon that
/// slept does not serve a pre-sleep view as fresh.
#[derive(Default)]
pub(crate) struct ViewCache {
    gate: tokio::sync::Mutex<()>,
    slot: std::sync::Mutex<Option<(std::time::SystemTime, FleetView)>>,
}

impl ViewCache {
    fn fresh(&self) -> Option<FleetView> {
        let guard = self.slot.lock().ok()?;
        let (at, view) = guard.as_ref()?;
        crate::wall_clock_cache_is_fresh(*at, std::time::SystemTime::now(), FLEET_VIEW_CACHE_TTL).then(|| view.clone())
    }

    /// The cached view, or the result of `gather` (run once for every
    /// request that arrived while it ran).
    pub(crate) async fn get(
        &self,
        gather: impl FnOnce() -> FleetView + Send + 'static,
    ) -> Result<FleetView, tokio::task::JoinError> {
        if let Some(view) = self.fresh() {
            return Ok(view);
        }
        let _permit = self.gate.lock().await;
        if let Some(view) = self.fresh() {
            return Ok(view);
        }
        let view = tokio::task::spawn_blocking(gather).await?;
        if let Ok(mut guard) = self.slot.lock() {
            *guard = Some((std::time::SystemTime::now(), view.clone()));
        }
        Ok(view)
    }
}

// ─── what the daemon and the CLI read ──────────────────────────────────────

/// The real inputs: this machine's roster, the shared Redis's presence, this
/// machine's card, and each peer over the verified peer path.
pub(crate) struct ProcessSources {
    provider: Arc<dyn IdentityProvider>,
}

impl ProcessSources {
    pub(crate) fn new(provider: Arc<dyn IdentityProvider>) -> Self {
        Self { provider }
    }
}

impl FleetSources for ProcessSources {
    fn local_machine_id(&self) -> Option<String> {
        darkmux_flow::resolve_machine_id()
    }

    fn roster(&self) -> Result<FleetRoster, &'static str> {
        darkmux_fleet::load_roster().map_err(|e| {
            eprintln!("darkmux serve: fleet view: reading the roster failed ({e:#})");
            crate::ROSTER_READ_FAILED
        })
    }

    fn presence(&self) -> (Vec<PresenceBeat>, SourceState) {
        match darkmux_flow::redis_url() {
            Some(url) => crate::read_presence_beats(&url, "machines", darkmux_flow::presence::read_live),
            None => (Vec::new(), SourceState::Off),
        }
    }

    fn local_card(&self) -> MachineCard {
        gather_local_card(None)
    }

    fn peer_card(&self, entry: &MachineEntry, known_version: Option<&str>) -> CardOutcome {
        fetch_peer_card(self.provider.as_ref(), entry, known_version)
    }
}

/// This machine's view, gathered now: what `darkmux machine list` prints.
/// Not cached (`cache_ttl_ms` is `0`); the seat block of this machine's own
/// card is the running daemon's, so a card built here carries none.
pub fn gather_fleet_view_now() -> FleetView {
    let provider: Arc<dyn IdentityProvider> = Arc::from(darkmux_fleet::configured_provider_or_unavailable());
    gather_view(&ProcessSources::new(provider), Duration::ZERO)
}

// ─── asking one peer ───────────────────────────────────────────────────────

fn fetch_peer_card(provider: &dyn IdentityProvider, entry: &MachineEntry, known_version: Option<&str>) -> CardOutcome {
    // The daemon never writes a first-contact pin (the roster is operator
    // state); `peer_target` still refuses an address that is not the pinned
    // node, before any token is attached.
    let target = match darkmux_fleet::peer_target(
        &entry.id,
        entry,
        None,
        None,
        darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT,
        true,
        provider,
    ) {
        Ok(t) => t,
        Err(_) => return CardOutcome::Unreachable { reason: UnreachableReason::Unverified, detail: None },
    };
    match darkmux_fleet::fleet_get(&target, "/machine/card", PEER_CARD_TIMEOUT, &[]) {
        Ok(resp) => parse_card_response(resp),
        Err(ureq::Error::Status(401 | 403, _)) => {
            CardOutcome::Unreachable { reason: UnreachableReason::AuthRequired, detail: None }
        }
        Err(ureq::Error::Status(404, _)) => CardOutcome::Unavailable {
            peer_version: known_version.map(str::to_string).or_else(|| peer_health_version(&target)),
        },
        Err(ureq::Error::Status(code, _)) => CardOutcome::Unreachable {
            reason: UnreachableReason::BadAnswer,
            detail: Some(format!("HTTP {code}")),
        },
        Err(ureq::Error::Transport(t)) => CardOutcome::Unreachable {
            reason: UnreachableReason::ConnectFailed,
            detail: Some(format!("{:?}", t.kind())),
        },
    }
}

/// The darkmux version a peer's `/health` reports (the route is
/// auth-exempt), for a peer that has no card route.
fn peer_health_version(target: &darkmux_fleet::PeerTarget) -> Option<String> {
    let resp = darkmux_fleet::fleet_get(target, "/health", Duration::from_millis(1000), &[]).ok()?;
    let mut v = read_json(resp)?;
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    v.get("darkmux_version").and_then(serde_json::Value::as_str).map(str::to_string)
}

fn read_json(resp: ureq::Response) -> Option<serde_json::Value> {
    let mut body = String::new();
    resp.into_reader().take(MAX_PEER_CARD_BYTES).read_to_string(&mut body).ok()?;
    serde_json::from_str(&body).ok()
}

/// A peer's card, sanitized and typed. A body that is not a card of this
/// schema major is "unavailable", naming the version it reports.
fn parse_card_response(resp: ureq::Response) -> CardOutcome {
    let Some(mut v) = read_json(resp) else {
        return CardOutcome::Unreachable {
            reason: UnreachableReason::BadAnswer,
            detail: Some("not a JSON document".to_string()),
        };
    };
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    let peer_version = v
        .pointer("/specs/darkmux_version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if !same_schema_major(v.get("card_schema_version").and_then(serde_json::Value::as_str)) {
        return CardOutcome::Unavailable { peer_version };
    }
    match serde_json::from_value::<MachineCard>(v) {
        Ok(card) => CardOutcome::Available { card: Box::new(card) },
        Err(_) => CardOutcome::Unavailable { peer_version },
    }
}

/// Whether a peer's card schema version shares this build's major.
fn same_schema_major(theirs: Option<&str>) -> bool {
    let major = |v: &str| v.split('.').next().map(str::to_string);
    theirs.and_then(major).is_some_and(|m| Some(m) == major(CARD_SCHEMA_VERSION))
}

// ─── the daemon's wiring ───────────────────────────────────────────────────

/// What the daemon's fleet routes read: the admission path the listener uses,
/// the gather's sources, and the cache.
#[derive(Clone)]
pub(crate) struct FleetContext {
    pub admission: crate::fleet_listener::Admission,
    pub sources: Arc<dyn FleetSources>,
    pub cache: Arc<ViewCache>,
}

impl FleetContext {
    pub(crate) fn production() -> Self {
        let admission = crate::fleet_listener::Admission::production();
        let sources = Arc::new(ProcessSources::new(admission.provider.clone()));
        Self { admission, sources, cache: Arc::new(ViewCache::default()) }
    }

    /// A context that reads nothing outside the process: no roster, no
    /// presence, and an admission that refuses everyone. For tests that are
    /// not about the fleet, so they never dial the developer's real peers.
    #[cfg(test)]
    pub(crate) fn hermetic() -> Self {
        Self::with_sources(Arc::new(tests::Scripted::default()), crate::fleet_listener::Admission::refusing())
    }

    #[cfg(test)]
    pub(crate) fn with_sources(sources: Arc<dyn FleetSources>, admission: crate::fleet_listener::Admission) -> Self {
        Self { admission, sources, cache: Arc::new(ViewCache::default()) }
    }
}

/// `GET /machine/card`. The card for a caller that presents the fleet token
/// from a verified node carries that node's `accepts`; any other reader gets
/// the card without it.
pub(crate) async fn machine_card_handler(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<MachineCard>, (axum::http::StatusCode, &'static str)> {
    let accepts =
        crate::machine_card::accepts_for(&state.fleet.admission, peer.map(|c| c.0.ip()), &headers).await;
    tokio::task::spawn_blocking(move || gather_local_card(accepts))
        .await
        .map(axum::Json)
        .map_err(|_| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "darkmux serve: machine card gather panicked\n"))
}

/// `GET /fleet/view`. Peers' `accepts` blocks state what each lets THIS
/// machine do, so they go only to a reader on this machine.
pub(crate) async fn fleet_view_handler(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<FleetView>, (axum::http::StatusCode, &'static str)> {
    let sources = state.fleet.sources.clone();
    let view = state
        .fleet
        .cache
        .get(move || gather_view(sources.as_ref(), FLEET_VIEW_CACHE_TTL))
        .await
        .map_err(|_| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "darkmux serve: fleet view gather panicked\n"))?;
    let local = crate::is_local_request(peer.map(|c| c.0), &headers);
    Ok(axum::Json(if local { view } else { view.without_accepts() }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Scripted inputs: a roster, presence, and one outcome per peer id, each
    /// after an optional delay; counts the dials.
    #[derive(Default)]
    pub(crate) struct Scripted {
        pub local_id: Option<String>,
        pub roster: Vec<MachineEntry>,
        pub beats: Vec<PresenceBeat>,
        pub presence_off: bool,
        pub roster_error: Option<&'static str>,
        pub outcomes: Mutex<std::collections::BTreeMap<String, (u64, CardOutcome)>>,
        pub dials: Mutex<Vec<(String, Option<String>)>>,
        pub local_cards: AtomicUsize,
    }

    pub(crate) fn entry(id: &str) -> MachineEntry {
        MachineEntry {
            id: id.to_string(),
            address: format!("{id}.example.invalid"),
            description: None,
            added_unix_ms: 1,
            machine_uid: None,
            loopback_intended: false,
            node_id: None,
            extras: Default::default(),
        }
    }

    pub(crate) fn beat(name: &str, uid: &str, version: Option<&str>) -> PresenceBeat {
        PresenceBeat {
            machine_uid: uid.to_string(),
            display_name: name.to_string(),
            schema_version: "2.0.0".into(),
            beat_ts_ms: 1234,
            specs: None,
            darkmux_version: version.map(str::to_string),
        }
    }

    impl FleetSources for Scripted {
        fn local_machine_id(&self) -> Option<String> {
            self.local_id.clone()
        }
        fn roster(&self) -> Result<FleetRoster, &'static str> {
            if let Some(e) = self.roster_error {
                return Err(e);
            }
            let mut r = FleetRoster::default();
            for e in &self.roster {
                r.machines.insert(e.id.clone(), e.clone());
            }
            Ok(r)
        }
        fn presence(&self) -> (Vec<PresenceBeat>, SourceState) {
            if self.presence_off {
                (Vec::new(), SourceState::Off)
            } else {
                (self.beats.clone(), SourceState::Ok)
            }
        }
        fn local_card(&self) -> MachineCard {
            self.local_cards.fetch_add(1, Ordering::SeqCst);
            gather_local_card(None)
        }
        fn peer_card(&self, entry: &MachineEntry, known_version: Option<&str>) -> CardOutcome {
            self.dials.lock().unwrap().push((entry.id.clone(), known_version.map(str::to_string)));
            let (delay, outcome) = self.outcomes.lock().unwrap().get(&entry.id).cloned().expect("a scripted outcome");
            std::thread::sleep(Duration::from_millis(delay));
            outcome
        }
    }

    fn dialed(s: &Scripted) -> Vec<String> {
        let mut v: Vec<String> = s.dials.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();
        v.sort();
        v
    }

    fn peer_says(s: &Scripted, id: &str, delay_ms: u64, o: CardOutcome) {
        s.outcomes.lock().unwrap().insert(id.to_string(), (delay_ms, o));
    }

    fn unavailable(v: &str) -> CardOutcome {
        CardOutcome::Unavailable { peer_version: Some(v.to_string()) }
    }

    #[test]
    fn a_peer_presence_says_is_gone_is_not_dialed_and_the_rest_are() {
        let s = Scripted {
            local_id: Some("laptop".into()),
            roster: vec![entry("laptop"), entry("studio"), entry("mini")],
            beats: vec![beat("laptop", "U1", None), beat("mini", "U3", Some("5.0.0"))],
            ..Default::default()
        };
        peer_says(&s, "mini", 0, unavailable("5.0.0"));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(dialed(&s), vec!["mini".to_string()], "studio has no live beat: never dialed");
        let studio = view.machines.iter().find(|m| m.entry.id == "studio").unwrap();
        assert_eq!(studio.liveness, Liveness::Gone);
        assert!(matches!(
            studio.card,
            CardOutcome::Unreachable { reason: UnreachableReason::PresenceGone, .. }
        ));
        let mini = view.machines.iter().find(|m| m.entry.id == "mini").unwrap();
        assert_eq!((mini.liveness, mini.last_beat_ms), (Liveness::Live, Some(1234)));
    }

    #[test]
    fn this_machines_card_is_built_locally_and_never_dialed() {
        let s = Scripted { local_id: Some("Laptop".into()), roster: vec![entry("laptop")], ..Default::default() };
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(s.local_cards.load(Ordering::SeqCst), 1);
        assert!(dialed(&s).is_empty(), "no HTTP for this machine's own row");
        assert!(view.machines[0].is_this_machine);
        assert!(matches!(view.machines[0].card, CardOutcome::Available { .. }));
        assert_eq!(view.machines[0].liveness, Liveness::Live, "the machine answering is alive");
    }

    /// Presence that is off or unreadable says nothing about a peer, so it is
    /// asked; only a readable presence with no beat is "gone".
    #[test]
    fn with_no_presence_to_read_every_peer_is_asked_and_liveness_is_unknown() {
        let s = Scripted {
            local_id: Some("laptop".into()),
            roster: vec![entry("studio")],
            presence_off: true,
            ..Default::default()
        };
        peer_says(&s, "studio", 0, unavailable("4.9.0"));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(dialed(&s), vec!["studio".to_string()]);
        assert_eq!(view.machines[0].liveness, Liveness::Unknown);
        assert!(matches!(view.presence, SourceState::Off));
    }

    #[test]
    fn a_beat_is_matched_by_hardware_identity_when_the_entry_declares_one() {
        let mut studio = entry("studio");
        studio.machine_uid = Some("abc-123".into());
        let s = Scripted {
            local_id: Some("laptop".into()),
            roster: vec![studio],
            // Same NAME, different hardware: not this machine.
            beats: vec![beat("studio", "OTHER", None)],
            ..Default::default()
        };
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.machines[0].liveness, Liveness::Gone);
        let mut s2 = Scripted { local_id: Some("laptop".into()), roster: vec![{
            let mut e = entry("studio");
            e.machine_uid = Some("abc-123".into());
            e
        }], beats: vec![beat("renamed", "ABC-123", None)], ..Default::default() };
        peer_says(&s2, "studio", 0, unavailable("5.0.0"));
        s2.presence_off = false;
        assert_eq!(gather_view(&s2, FLEET_VIEW_CACHE_TTL).machines[0].liveness, Liveness::Live, "uid match, case-insensitive");
    }

    /// The promise: a slow peer does not hold the others up.
    #[test]
    fn peers_are_asked_in_parallel_not_one_after_another() {
        let s = Scripted {
            local_id: Some("laptop".into()),
            roster: vec![entry("a"), entry("b"), entry("c")],
            presence_off: true,
            ..Default::default()
        };
        for id in ["a", "b", "c"] {
            peer_says(&s, id, 400, unavailable("5.0.0"));
        }
        let started = std::time::Instant::now();
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let took = started.elapsed();
        assert_eq!(view.machines.len(), 3);
        assert!(took < Duration::from_millis(900), "three 400 ms peers took {took:?}: they were asked in series");
    }

    #[test]
    fn an_unreadable_roster_is_named_and_the_view_is_empty() {
        let s = Scripted { roster_error: Some("the roster is broken"), ..Default::default() };
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.roster_error.as_deref(), Some("the roster is broken"));
        assert!(view.machines.is_empty());
    }

    #[test]
    fn the_view_records_the_ttl_it_was_gathered_under() {
        let s = Scripted::default();
        assert_eq!(gather_view(&s, FLEET_VIEW_CACHE_TTL).cache_ttl_ms, 5000);
        assert_eq!(gather_view(&s, Duration::ZERO).cache_ttl_ms, 0);
    }

    #[test]
    fn the_plan_never_dials_this_machine_or_a_gone_peer() {
        assert_eq!(plan_for(true, Liveness::Gone), Plan::Local);
        assert_eq!(plan_for(false, Liveness::Gone), Plan::Skip);
        assert_eq!(plan_for(false, Liveness::Live), Plan::Ask);
        assert_eq!(plan_for(false, Liveness::Unknown), Plan::Ask);
    }

    #[test]
    fn a_peer_card_on_another_schema_major_is_not_ours_to_read() {
        assert!(same_schema_major(Some("1.0")));
        assert!(same_schema_major(Some("1.7")));
        assert!(!same_schema_major(Some("2.0")));
        assert!(!same_schema_major(Some("")));
        assert!(!same_schema_major(None));
    }

    fn card_with_accepts() -> MachineCard {
        let mut card = gather_local_card(Some(crate::machine_card::CardAccepts {
            peer_name: "laptop".into(),
            profiles: vec!["deep".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        }));
        card.accepts.as_ref().expect("built with accepts");
        card.gather_ms = 1;
        card
    }

    #[test]
    fn a_view_for_another_reader_drops_every_peers_accepts_block() {
        let s = Scripted { local_id: Some("laptop".into()), roster: vec![entry("studio")], presence_off: true, ..Default::default() };
        peer_says(&s, "studio", 0, CardOutcome::Available { card: Box::new(card_with_accepts()) });
        let full = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let CardOutcome::Available { card } = &full.machines[0].card else { panic!("a card") };
        assert!(card.accepts.is_some(), "this machine's own reader sees it");
        let CardOutcome::Available { card } = &full.without_accepts().machines[0].card else { panic!("a card") };
        assert!(card.accepts.is_none());
    }

    // ── the routes ──────────────────────────────────────────────────────

    use crate::fleet_listener::Admission;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use darkmux_fleet::{test_node, StaticIdentityProvider};
    use darkmux_types::config::AcceptWorkEntry;
    use tower::ServiceExt;

    const TOKEN: &str = "fleet-card-test-token";

    /// An admission whose network knows the laptop at 10.0.0.9 and the mini
    /// at 10.0.0.10, and whose allow-list grants each something different.
    fn admission() -> Admission {
        let mut allow = std::collections::BTreeMap::new();
        for (name, node, profiles) in [("macbook-pro", "nLAPTOP", vec!["deep"]), ("mini-1", "nMINI", vec!["secret-profile"])] {
            allow.insert(
                name.to_string(),
                AcceptWorkEntry {
                    node_id: Some(node.into()),
                    profiles: Some(profiles.into_iter().map(String::from).collect()),
                    roles: Some(vec!["radio-host".into()]),
                    images: None,
                    workspace: Some(false),
                    extras: Default::default(),
                },
            );
        }
        let provider = StaticIdentityProvider {
            local: test_node("nSELF", "self", "10.0.0.1"),
            peers: vec![test_node("nLAPTOP", "laptop", "10.0.0.9"), test_node("nMINI", "mini", "10.0.0.10")],
            down: None,
        };
        Admission {
            provider: Arc::new(provider),
            local_node_id: Arc::new(|| Some("nSELF".to_string())),
            token: Arc::new(|| Some(TOKEN.to_string())),
            allow_list: Arc::new(move || Ok(allow.clone())),
        }
    }

    async fn get(ctx: FleetContext, uri: &str, peer: &str, headers: &[(&str, &str)]) -> serde_json::Value {
        let app = crate::build_router_full(std::path::PathBuf::new(), None, None, ctx);
        let mut req = Request::builder().uri(uri).header("host", "localhost");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200, "GET {uri}");
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap()).unwrap()
    }

    async fn card_for(peer: &str, headers: &[(&str, &str)]) -> serde_json::Value {
        get(FleetContext::with_sources(Arc::new(Scripted::default()), admission()), "/machine/card", peer, headers).await
    }

    fn bearer() -> String {
        format!("Bearer {TOKEN}")
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn accepts_is_absent_without_the_fleet_token() {
        let card = card_for("10.0.0.9:5555", &[]).await;
        assert!(card.get("accepts").is_none(), "a verified node with no token: {card}");
        assert!(card["profiles"].is_array(), "the rest of the card is still served");
        let wrong = card_for("10.0.0.9:5555", &[("authorization", "Bearer not-the-token")]).await;
        assert!(wrong.get("accepts").is_none(), "a wrong token: {wrong}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn accepts_is_absent_for_a_node_that_is_not_verified_and_allow_listed() {
        let auth = bearer();
        let stranger = card_for("10.0.0.77:5555", &[("authorization", &auth)]).await;
        assert!(stranger.get("accepts").is_none(), "an address no node holds: {stranger}");
        let mut no_entry = admission();
        no_entry.allow_list = Arc::new(|| Ok(Default::default()));
        let card = get(
            FleetContext::with_sources(Arc::new(Scripted::default()), no_entry),
            "/machine/card",
            "10.0.0.9:5555",
            &[("authorization", &auth)],
        )
        .await;
        assert!(card.get("accepts").is_none(), "a node with no allow-list entry: {card}");
    }

    /// A proxy in front of the daemon makes every peer arrive on loopback with
    /// the real address in a header. A header is not evidence of a node.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_forwarded_header_does_not_make_a_loopback_caller_a_node() {
        let auth = bearer();
        let card = card_for("127.0.0.1:5555", &[("authorization", &auth), ("x-forwarded-for", "10.0.0.9")]).await;
        assert!(card.get("accepts").is_none(), "{card}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn accepts_is_the_verified_callers_entry_and_no_one_elses() {
        let auth = bearer();
        let card = card_for("10.0.0.9:5555", &[("authorization", &auth)]).await;
        let accepts = &card["accepts"];
        assert_eq!(accepts["peer_name"], "macbook-pro", "{card}");
        assert_eq!(accepts["profiles"], serde_json::json!(["deep"]));
        assert_eq!(accepts["roles"], serde_json::json!(["radio-host"]));
        assert_eq!(accepts["workspace"], false);
        let text = card.to_string();
        assert!(!text.contains("mini-1") && !text.contains("secret-profile"), "another sender's grant leaked: {text}");
        let mini = card_for("10.0.0.10:5555", &[("authorization", &auth)]).await;
        assert_eq!(mini["accepts"]["peer_name"], "mini-1");
        assert!(!mini.to_string().contains("macbook-pro"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_fleet_view_route_serves_a_row_per_roster_machine_and_hides_grants_from_others() {
        let s = Scripted { local_id: Some("laptop".into()), roster: vec![entry("studio")], presence_off: true, ..Default::default() };
        peer_says(&s, "studio", 0, CardOutcome::Available { card: Box::new(card_with_accepts()) });
        let ctx = FleetContext::with_sources(Arc::new(s), admission());
        let local = get(ctx.clone(), "/fleet/view", "127.0.0.1:5555", &[]).await;
        assert_eq!(local["machines"][0]["entry"]["id"], "studio");
        assert_eq!(local["cache_ttl_ms"], 5000);
        assert!(local["machines"][0]["card"]["card"]["accepts"].is_object(), "this machine's own reader: {local}");
        let remote = get(ctx, "/fleet/view", "10.0.0.9:5555", &[("authorization", &bearer())]).await;
        assert!(remote["machines"][0]["card"]["card"].get("accepts").is_none(), "{remote}");
    }

    #[tokio::test]
    async fn a_burst_on_a_cold_cache_costs_one_gather() {
        let cache = Arc::new(ViewCache::default());
        let gathers = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (cache, gathers) = (cache.clone(), gathers.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get(move || {
                        gathers.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(100));
                        gather_view(&Scripted::default(), FLEET_VIEW_CACHE_TTL)
                    })
                    .await
                    .unwrap()
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(gathers.load(Ordering::SeqCst), 1);
    }
}
