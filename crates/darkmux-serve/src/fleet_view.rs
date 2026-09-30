//! The fleet view: one row per roster machine, each carrying that machine's
//! own card. Every daemon gathers its own view (`GET /fleet/view`), and
//! `darkmux machine list` prints the same gather, so any machine asking gets
//! the same answer with no hub in the way.
//!
//! **Where a card comes from.** From the peer itself, over the verified peer
//! path: `darkmux_fleet::peer_target` checks the roster address is the
//! pinned overlay node before `darkmux_fleet::fleet_get` sends the fleet
//! token. The FIRST ask goes to the peer's fleet listener (`GET /fleet/card`),
//! the one place that verifies the caller's node from the socket and can say
//! what the peer accepts from THIS machine; when the listener is off or
//! refuses, the card comes from the peer's daemon (`GET /machine/card`),
//! which states no grant. Each row records which source answered and what
//! is known about `accepts` ([`AcceptsState`]). Cards never come from Redis.
//! Presence is a liveness hint only, because anything that can write the
//! shared Redis could forge a key, and a card drives routing and
//! data-boundary decisions.
//!
//! **How.** Peers are asked in parallel; the whole view is single-flight and
//! cached for [`FLEET_VIEW_CACHE_TTL`] (recorded in the view as
//! `cache_ttl_ms`, never adaptive). A peer that presence says is gone is not
//! dialed. This machine's own card is built locally, with no HTTP.
//!
//! **What a row can say.** Exactly one of: the peer's card; "card
//! unavailable" (an older darkmux with no `/machine/card` route, or a card on
//! another schema major), which is not an error; "mismatch" (the answering
//! machine is not the one the roster entry names, so its card is not
//! attributed); or "unreachable" with a typed reason. The view never fills
//! in a fact a card did not state.

use crate::machine_card::{gather_local_card, CardAccepts, ListenerCard, MachineCard, CARD_SCHEMA_VERSION};
use crate::source_state::SourceState;
use crate::wire::RosterMachineEntry;
use darkmux_fleet::{FleetRoster, IdentityProvider, MachineEntry, TargetError};
use darkmux_flow::presence::PresenceBeat;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// How long a gathered view is served before the next request gathers again.
/// Recorded in every view as `cache_ttl_ms`.
pub(crate) const FLEET_VIEW_CACHE_TTL: Duration = Duration::from_secs(5);

/// The longest ONE card request to a peer may take, overall: connecting,
/// sending and reading the answer. A peer can be asked twice (its listener,
/// then its daemon) and its `/health` once more; the identity check and DNS
/// before them are bounded by their own limits, not by this one.
const PEER_CARD_TIMEOUT: Duration = Duration::from_millis(2000);

/// The longest a peer's `/health` may take, asked only to name the version
/// of a peer that has no card route.
const PEER_HEALTH_TIMEOUT: Duration = Duration::from_millis(1000);

/// The longest a refusal sentence from a peer's listener is kept.
const PEER_REASON_MAX_CHARS: usize = 400;

/// The most a peer's card may weigh: a card is a few KB, and a peer that
/// answers with more is not sending one.
const MAX_PEER_CARD_BYTES: u64 = 1024 * 1024;

/// The longest single string a peer's card may carry into this view.
const PEER_FIELD_MAX_CHARS: usize = 200;

/// Whether a roster machine has a live presence beat. Presence is a hint:
/// `unknown` when the shared Redis is off or could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Live,
    Gone,
    Unknown,
}

/// Why a card could not be read from a machine that may be up. Each reason
/// has its own remedy, so none stands for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum UnreachableReason {
    /// Presence reports no live beat, so the peer was not dialed.
    PresenceGone,
    /// The roster address names no host or has a bad port: nothing was sent.
    BadAddress,
    /// The roster address did not resolve: nothing was sent.
    DnsFailed,
    /// The network identity tool could not answer (it is not running, or not
    /// signed in), so the address could not be verified: nothing was sent.
    IdentityUnavailable,
    /// The roster address is not a node on the identity network: nothing, and
    /// no token, was sent.
    NotOnOverlay,
    /// The node at the roster address is not the one the entry pinned:
    /// nothing, and no token, was sent.
    PinMismatch,
    /// The peer answered 401 or 403: this machine's fleet token is missing or
    /// not the peer's.
    AuthRequired,
    /// The connection failed or timed out.
    ConnectFailed,
    /// The peer answered, with something that is not a card.
    BadAnswer,
}

/// Which endpoint a card came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum CardSource {
    /// This machine's own card, built in this process.
    Local,
    /// The peer's fleet listener, behind its gate: the card comes with what
    /// the peer accepts from this machine.
    Listener,
    /// The peer's daemon (`/machine/card`): a card that states no grant.
    Daemon,
}

/// Why what a peer accepts from this machine is not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum AcceptsUnknown {
    /// The row is this machine: it does not send work to itself.
    ThisMachine,
    /// Nothing answered on the peer's fleet listener port: the listener is
    /// off there, or not reachable from this machine.
    ListenerOff,
    /// The listener answered without a card and without refusing this
    /// machine: it could not identify the caller, is at capacity, has no
    /// fleet token, or is an older darkmux with no card route. `detail`
    /// carries its word.
    ListenerUnavailable,
    /// The listener was not dialed: the roster address is a loopback one, and
    /// no node is verified behind it, so no token goes anywhere.
    NotDialed,
    /// A reader that is not on this machine is not shown what this machine's
    /// peers let it do.
    Withheld,
}

/// What a peer lets THIS machine do, as far as this machine knows. Three
/// answers a reader must tell apart: yes (with the entry), no (with the
/// peer's reason), and not known (with why).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AcceptsState {
    /// The peer's listener verified this machine and lists it: `accepts` is
    /// that entry, and no other.
    Granted { accepts: CardAccepts },
    /// The peer's listener refused this machine (its token, its node, or the
    /// allow-list): `reason` is the peer's own sentence.
    Refused { reason: String },
    /// No answer either way.
    Unknown {
        why: AcceptsUnknown,
        /// The peer's word or the transport's, when there was one.
        detail: Option<String>,
    },
}

/// What asking one machine for its card produced.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CardOutcome {
    /// The machine's own card.
    Available {
        card: Box<MachineCard>,
        /// Which endpoint the card came from.
        source: CardSource,
        /// What the machine lets THIS machine do.
        accepts: AcceptsState,
    },
    /// The machine answered, but has no card this darkmux reads: it is on a
    /// darkmux without the route, or its card is on another schema major.
    /// Not an error.
    Unavailable {
        /// The darkmux version the peer reports, when it did.
        peer_version: Option<String>,
    },
    /// A card came back from an endpoint that is not this roster entry's
    /// machine (another machine holds its address, or the entry names the
    /// wrong one). The card is NOT attributed to the entry: nothing in it is
    /// shown or routed on.
    Mismatch {
        /// The `machine_id` the answering card claims, when it named one.
        answered_as: Option<String>,
    },
    /// No card could be had.
    Unreachable {
        reason: UnreachableReason,
        /// The transport's word for a connection failure, or the HTTP status.
        detail: Option<String>,
    },
}

/// One roster machine and what it said about itself.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

/// Which process gathered a view, and so what this machine's own row can
/// say: only a running daemon has seats, a host sampler's thermal state and a
/// battery reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum GatheredBy {
    /// A running daemon: this machine's row carries its seats and governor
    /// readings.
    Daemon,
    /// A CLI process, because no daemon answered: this machine's row has no
    /// seats and no thermal or battery reading. They are not observed, not
    /// absent.
    CliProcess,
}

/// `GET /fleet/view`: every roster machine and its card, as this daemon
/// gathered them.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetView {
    /// Which process gathered this view.
    pub gathered_by: GatheredBy,
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
    /// The view without what any peer lets THIS machine do: what a reader
    /// other than this machine may see, since those entries state what each
    /// peer lets this machine run.
    pub fn without_accepts(mut self) -> Self {
        for m in &mut self.machines {
            if let CardOutcome::Available { accepts, source, .. } = &mut m.card {
                if *source != CardSource::Local {
                    *accepts = AcceptsState::Unknown { why: AcceptsUnknown::Withheld, detail: None };
                }
            }
        }
        self
    }
}

/// Everything a gather reads, so the gather itself is the same code in the
/// daemon, in the CLI and under a test that scripts the peers.
pub(crate) trait FleetSources: Send + Sync {
    /// Which process this gather runs in.
    fn gathered_by(&self) -> GatheredBy;
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
                    Plan::Local => CardOutcome::Available {
                        card: Box::new(src.local_card()),
                        source: CardSource::Local,
                        accepts: AcceptsState::Unknown { why: AcceptsUnknown::ThisMachine, detail: None },
                    },
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
        gathered_by: src.gathered_by(),
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
    gathered_by: GatheredBy,
}

impl ProcessSources {
    pub(crate) fn new(provider: Arc<dyn IdentityProvider>, gathered_by: GatheredBy) -> Self {
        Self { provider, gathered_by }
    }
}

impl FleetSources for ProcessSources {
    fn gathered_by(&self) -> GatheredBy {
        self.gathered_by
    }

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
        gather_local_card()
    }

    fn peer_card(&self, entry: &MachineEntry, known_version: Option<&str>) -> CardOutcome {
        let listener_port = darkmux_types::config_access::fleet_listener_port();
        fetch_peer_card(self.provider.as_ref(), entry, known_version, listener_port)
    }
}

/// This machine's view, gathered in THIS process: what `darkmux machine list`
/// prints when no daemon answers. Not cached (`cache_ttl_ms` is `0`); the
/// seat block and the governor readings of this machine's own card belong to
/// a running daemon, so a card built here has none (the view says so in
/// `gathered_by`).
pub fn gather_fleet_view_now() -> FleetView {
    let provider: Arc<dyn IdentityProvider> = Arc::from(darkmux_fleet::configured_provider_or_unavailable());
    gather_view(&ProcessSources::new(provider, GatheredBy::CliProcess), Duration::ZERO)
}

/// The view this machine's own daemon gathered, when one answers on
/// `daemon_addr`: the view any reader of this daemon would get, with this
/// machine's seats and governor readings. `None` when no daemon answers, or
/// it answers with something that is not a view (an older darkmux).
pub fn fetch_local_daemon_view(daemon_addr: &str) -> Option<FleetView> {
    let provider = darkmux_fleet::configured_provider_or_unavailable();
    let target = darkmux_fleet::local_daemon_target(
        daemon_addr,
        darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT,
        provider.as_ref(),
    )
    .ok()?;
    let resp = darkmux_fleet::fleet_get(&target, "/fleet/view", LOCAL_VIEW_TIMEOUT, &[]).ok()?;
    let mut v = read_json(resp)?;
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    serde_json::from_value(v).ok()
}

/// The longest a CLI waits for its own daemon's view: the daemon gathers
/// every peer in parallel behind its own per-request bounds.
const LOCAL_VIEW_TIMEOUT: Duration = Duration::from_secs(12);

// ─── asking one peer ───────────────────────────────────────────────────────

/// Where a peer's card can be asked for.
struct CardTargets {
    /// The peer's daemon, on the address the roster names.
    daemon: darkmux_fleet::PeerTarget,
    /// The same verified node's fleet listener; `None` when no node was
    /// verified (a loopback roster entry), since the token goes nowhere
    /// unverified.
    listener: Option<darkmux_fleet::PeerTarget>,
}

fn unreachable_for(e: &TargetError) -> UnreachableReason {
    match e {
        TargetError::BadAddress { .. } => UnreachableReason::BadAddress,
        TargetError::DoesNotResolve { .. } => UnreachableReason::DnsFailed,
        TargetError::IdentityUnavailable { .. } => UnreachableReason::IdentityUnavailable,
        TargetError::NotOnOverlay { .. } => UnreachableReason::NotOnOverlay,
        TargetError::PinMismatch { .. } => UnreachableReason::PinMismatch,
        // Only a caller that passes this machine's own daemon address can
        // hit it; a roster entry never does.
        TargetError::OwnAddress { .. } => UnreachableReason::BadAddress,
    }
}

/// The verified node behind a roster entry, aimed at its daemon and its
/// listener. The daemon never writes a first-contact pin (the roster is
/// operator state); `peer_target` still refuses an address that is not the
/// pinned node, before any token is attached. A loopback entry no node
/// stands behind, or that the identity tool cannot answer for (a same-host
/// test fleet), is dialed as written, daemon only and with no token. A
/// loopback address that is a node other than the pinned one is refused.
fn card_targets(
    provider: &dyn IdentityProvider,
    entry: &MachineEntry,
    listener_port: u16,
) -> Result<CardTargets, UnreachableReason> {
    let daemon_port = darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT;
    match darkmux_fleet::peer_target(&entry.id, entry, None, daemon_port, false, provider) {
        Ok(daemon) => {
            let listener = daemon.at_listener(listener_port);
            Ok(CardTargets { daemon, listener: Some(listener) })
        }
        Err(e @ (TargetError::NotOnOverlay { .. } | TargetError::IdentityUnavailable { .. }))
            if darkmux_fleet::address_host_is_loopback(&entry.address) =>
        {
            darkmux_fleet::peer_target(&entry.id, entry, None, daemon_port, true, provider)
                .map(|daemon| CardTargets { daemon, listener: None })
                .map_err(|_| unreachable_for(&e))
        }
        Err(e) => Err(unreachable_for(&e)),
    }
}

/// Ask one roster peer for its card: its fleet listener first (on
/// `listener_port`), then its daemon.
fn fetch_peer_card(
    provider: &dyn IdentityProvider,
    entry: &MachineEntry,
    known_version: Option<&str>,
    listener_port: u16,
) -> CardOutcome {
    let targets = match card_targets(provider, entry, listener_port) {
        Ok(t) => t,
        Err(reason) => return CardOutcome::Unreachable { reason, detail: None },
    };
    let accepts = match targets.listener.as_ref().map(ask_listener) {
        Some(ListenerAnswer::Card(listener_card)) => {
            let ListenerCard { card, accepts } = *listener_card;
            return attribute(entry, card, CardSource::Listener, AcceptsState::Granted { accepts });
        }
        Some(ListenerAnswer::NoCard(state)) => state,
        None => AcceptsState::Unknown { why: AcceptsUnknown::NotDialed, detail: None },
    };
    fetch_card_from(&targets.daemon, entry, known_version, accepts)
}

/// What asking a peer's fleet listener for its card produced.
enum ListenerAnswer {
    /// The card, with what the listener accepts from this machine.
    Card(Box<ListenerCard>),
    /// No card: what that says about `accepts`. The card is then asked of the
    /// peer's daemon.
    NoCard(AcceptsState),
}

fn unknown(why: AcceptsUnknown, detail: impl Into<Option<String>>) -> AcceptsState {
    AcceptsState::Unknown { why, detail: detail.into() }
}

/// Ask a verified peer's listener for its card.
fn ask_listener(target: &darkmux_fleet::PeerTarget) -> ListenerAnswer {
    match darkmux_fleet::fleet_get(target, darkmux_fleet::CARD_PATH, PEER_CARD_TIMEOUT, &[]) {
        Ok(resp) => parse_listener_card(resp),
        Err(ureq::Error::Status(code, resp)) => ListenerAnswer::NoCard(state_for_refusal(code, resp)),
        Err(ureq::Error::Transport(t)) => {
            ListenerAnswer::NoCard(unknown(AcceptsUnknown::ListenerOff, format!("{:?}", t.kind())))
        }
    }
}

/// What a listener's non-200 answer says about `accepts`: 401 and 403 are its
/// refusal of this machine (its own sentence rides the body); anything else
/// is not an answer about this machine at all.
fn state_for_refusal(code: u16, resp: ureq::Response) -> AcceptsState {
    let sentence = read_json(resp).and_then(|mut v| {
        darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_REASON_MAX_CHARS);
        v.get("reason").and_then(serde_json::Value::as_str).map(str::to_string)
    });
    match code {
        401 | 403 => AcceptsState::Refused {
            reason: sentence.unwrap_or_else(|| format!("the fleet listener answered HTTP {code}")),
        },
        404 => unknown(AcceptsUnknown::ListenerUnavailable, "the listener has no card route (an older darkmux)".to_string()),
        _ => unknown(AcceptsUnknown::ListenerUnavailable, sentence.unwrap_or_else(|| format!("HTTP {code}"))),
    }
}

/// A listener's answer, sanitized and typed. A body that is not a card of this
/// schema major is no answer: the daemon is asked instead.
fn parse_listener_card(resp: ureq::Response) -> ListenerAnswer {
    let not_a_card = || {
        ListenerAnswer::NoCard(unknown(
            AcceptsUnknown::ListenerUnavailable,
            "the listener's answer is not a card this darkmux reads".to_string(),
        ))
    };
    let Some(mut v) = read_json(resp) else { return not_a_card() };
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    if !same_schema_major(v.pointer("/card/card_schema_version").and_then(serde_json::Value::as_str)) {
        return not_a_card();
    }
    match serde_json::from_value::<ListenerCard>(v) {
        Ok(card) => ListenerAnswer::Card(Box::new(card)),
        Err(_) => not_a_card(),
    }
}

/// Whether a card's own identity is the roster entry's. The hardware id
/// decides when both sides declare one; else the machine name does (the
/// roster id is the name flow records and allow-lists carry). A card that
/// names neither is not confirmed to be the entry's.
fn card_is_of(entry: &MachineEntry, card: &MachineCard) -> bool {
    match (entry.machine_uid.as_deref(), card.specs.machine_uid.as_deref()) {
        (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
        _ => card.specs.machine_id.as_deref().is_some_and(|id| darkmux_fleet::same_machine(id, &entry.id)),
    }
}

/// A card as the row for `entry` shows it: attributed, or flagged as another
/// machine's and dropped.
fn attribute(entry: &MachineEntry, card: MachineCard, source: CardSource, accepts: AcceptsState) -> CardOutcome {
    if card_is_of(entry, &card) {
        CardOutcome::Available { card: Box::new(card), source, accepts }
    } else {
        CardOutcome::Mismatch { answered_as: card.specs.machine_id.filter(|id| !id.is_empty()) }
    }
}

/// Ask a verified peer's daemon for its card. `accepts` is what the listener
/// attempt already established.
fn fetch_card_from(
    target: &darkmux_fleet::PeerTarget,
    entry: &MachineEntry,
    known_version: Option<&str>,
    accepts: AcceptsState,
) -> CardOutcome {
    match darkmux_fleet::fleet_get(target, "/machine/card", PEER_CARD_TIMEOUT, &[]) {
        Ok(resp) => parse_card_response(resp, entry, accepts),
        Err(ureq::Error::Status(401 | 403, _)) => {
            CardOutcome::Unreachable { reason: UnreachableReason::AuthRequired, detail: None }
        }
        Err(ureq::Error::Status(404, _)) => CardOutcome::Unavailable {
            peer_version: known_version.map(str::to_string).or_else(|| peer_health_version(target)),
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
    let resp = darkmux_fleet::fleet_get(target, "/health", PEER_HEALTH_TIMEOUT, &[]).ok()?;
    let mut v = read_json(resp)?;
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    v.get("darkmux_version").and_then(serde_json::Value::as_str).map(str::to_string)
}

fn read_json(resp: ureq::Response) -> Option<serde_json::Value> {
    let mut body = String::new();
    resp.into_reader().take(MAX_PEER_CARD_BYTES).read_to_string(&mut body).ok()?;
    serde_json::from_str(&body).ok()
}

/// A peer's daemon card, sanitized and typed. A body that is not a card of
/// this schema major is "unavailable", naming the version it reports.
fn parse_card_response(resp: ureq::Response, entry: &MachineEntry, accepts: AcceptsState) -> CardOutcome {
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
        Ok(card) => attribute(entry, card, CardSource::Daemon, accepts),
        Err(_) => CardOutcome::Unavailable { peer_version },
    }
}

/// Whether a peer's card schema version shares this build's major.
fn same_schema_major(theirs: Option<&str>) -> bool {
    let major = |v: &str| v.split('.').next().map(str::to_string);
    theirs.and_then(major).is_some_and(|m| Some(m) == major(CARD_SCHEMA_VERSION))
}

// ─── the daemon's wiring ───────────────────────────────────────────────────

/// What the daemon's fleet routes read: the gather's sources and the cache.
#[derive(Clone)]
pub(crate) struct FleetContext {
    pub sources: Arc<dyn FleetSources>,
    pub cache: Arc<ViewCache>,
}

impl FleetContext {
    pub(crate) fn production() -> Self {
        let provider: Arc<dyn IdentityProvider> = Arc::from(darkmux_fleet::configured_provider_or_unavailable());
        Self { sources: Arc::new(ProcessSources::new(provider, GatheredBy::Daemon)), cache: Arc::new(ViewCache::default()) }
    }

    /// A context that reads nothing outside the process: no roster and no
    /// presence. For tests that are not about the fleet, so they never dial
    /// the developer's real peers.
    #[cfg(test)]
    pub(crate) fn hermetic() -> Self {
        Self::with_sources(Arc::new(tests::Scripted::default()))
    }

    #[cfg(test)]
    pub(crate) fn with_sources(sources: Arc<dyn FleetSources>) -> Self {
        Self { sources, cache: Arc::new(ViewCache::default()) }
    }
}

/// `GET /machine/card`: this machine's card. It states no grant: what this
/// machine accepts from a caller is the fleet listener's to say
/// (`GET /fleet/card`), where the caller is verified from the socket.
pub(crate) async fn machine_card_handler(
) -> Result<axum::Json<MachineCard>, (axum::http::StatusCode, &'static str)> {
    tokio::task::spawn_blocking(gather_local_card)
        .await
        .map(axum::Json)
        .map_err(|_| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "darkmux serve: machine card gather panicked\n"))
}

/// `GET /fleet/view`. What peers let THIS machine do goes only to a reader on
/// this machine.
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
        fn gathered_by(&self) -> GatheredBy {
            GatheredBy::Daemon
        }
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
            gather_local_card()
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

    /// A card that says it is machine `id`, with no hardware id.
    fn card_of(id: &str) -> MachineCard {
        let mut card = gather_local_card();
        card.specs.machine_id = Some(id.to_string());
        card.specs.machine_uid = None;
        card.gather_ms = 1;
        card
    }

    fn grant() -> CardAccepts {
        CardAccepts {
            peer_name: "laptop".into(),
            profiles: vec!["deep".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        }
    }

    fn available(id: &str, source: CardSource, accepts: AcceptsState) -> CardOutcome {
        CardOutcome::Available { card: Box::new(card_of(id)), source, accepts }
    }

    #[test]
    fn a_view_for_another_reader_withholds_what_peers_let_this_machine_do() {
        let s = Scripted {
            local_id: Some("laptop".into()),
            roster: vec![entry("laptop"), entry("studio")],
            presence_off: true,
            ..Default::default()
        };
        peer_says(&s, "studio", 0, available("studio", CardSource::Listener, AcceptsState::Granted { accepts: grant() }));
        let full = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let accepts_of = |v: &FleetView, id: &str| match &v.machines.iter().find(|m| m.entry.id == id).unwrap().card {
            CardOutcome::Available { accepts, .. } => accepts.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(accepts_of(&full, "studio"), AcceptsState::Granted { accepts: grant() }, "this machine's own reader sees it");
        let other = full.without_accepts();
        assert_eq!(
            accepts_of(&other, "studio"),
            AcceptsState::Unknown { why: AcceptsUnknown::Withheld, detail: None }
        );
        assert_eq!(
            accepts_of(&other, "laptop"),
            AcceptsState::Unknown { why: AcceptsUnknown::ThisMachine, detail: None },
            "this machine's own row states no grant to hide"
        );
    }

    // ── which machine a card is of ──────────────────────────────────────

    fn attributed(entry: &MachineEntry, card: MachineCard) -> CardOutcome {
        attribute(entry, card, CardSource::Daemon, AcceptsState::Unknown { why: AcceptsUnknown::ListenerOff, detail: None })
    }

    /// The promise: a card is shown on a row only when it is that machine's.
    /// Another machine answering at the address (a re-used address, a wrong
    /// roster entry) is a mismatch, and its card is dropped, not attributed.
    #[test]
    fn a_card_from_another_machine_is_a_mismatch_and_is_not_attributed() {
        match attributed(&entry("studio"), card_of("mini")) {
            CardOutcome::Mismatch { answered_as } => assert_eq!(answered_as.as_deref(), Some("mini")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(attributed(&entry("studio"), card_of("Studio")), CardOutcome::Available { .. }), "names compare like flow names");
    }

    #[test]
    fn a_card_that_names_no_machine_is_not_confirmed_to_be_the_entrys() {
        let mut card = card_of("studio");
        card.specs.machine_id = None;
        assert!(matches!(attributed(&entry("studio"), card), CardOutcome::Mismatch { answered_as: None }));
    }

    /// Hardware identity decides when both sides declare one: a renamed
    /// machine is still itself, and a different machine under the same name
    /// is not.
    #[test]
    fn a_declared_hardware_id_decides_over_the_name() {
        let mut studio = entry("studio");
        studio.machine_uid = Some("abc-123".into());
        let mut renamed = card_of("studio-renamed");
        renamed.specs.machine_uid = Some("ABC-123".into());
        assert!(matches!(attributed(&studio, renamed), CardOutcome::Available { .. }), "same hardware, another name");
        let mut impostor = card_of("studio");
        impostor.specs.machine_uid = Some("OTHER".into());
        assert!(matches!(attributed(&studio, impostor), CardOutcome::Mismatch { .. }), "same name, other hardware");
    }

    // ── one peer over real HTTP ─────────────────────────────────────────

    use std::io::Write;

    /// A loopback HTTP server with canned answers by path. `/health` answers a
    /// version unless it is canned; any other path is a 404. Records every
    /// request's path and `Authorization` header. Serves a few requests, then
    /// stops.
    struct FakePeer {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    /// One request a fake peer got: its path and `Authorization` header.
    type Seen = (String, Option<String>);

    impl FakePeer {
        fn serve(routes: Vec<(&'static str, &'static str, String)>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            std::thread::spawn(move || {
                for _ in 0..6 {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let auth = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .map(|l| l.split_once(':').unwrap().1.trim().to_string());
                    log.lock().unwrap().push((path.clone(), auth));
                    let (status, body) = match routes.iter().find(|(p, _, _)| *p == path) {
                        Some((_, status, body)) => (*status, body.clone()),
                        None if path == "/health" => ("200 OK", r#"{"darkmux_version":"4.9.1"}"#.to_string()),
                        None => ("404 Not Found", "{}".to_string()),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self { port, seen }
        }

        fn paths(&self) -> Vec<String> {
            self.seen.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
        }

        fn authorization_of(&self, path: &str) -> Option<String> {
            self.seen.lock().unwrap().iter().find(|(p, _)| p == path).and_then(|(_, a)| a.clone())
        }
    }

    /// A port nothing listens on.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    const FLEET_TOKEN: &str = "fleet-view-test-token";

    /// Run `f` with the fleet token in this process's environment.
    fn with_fleet_token<T>(f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("DARKMUX_SERVE_TOKEN").ok();
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", FLEET_TOKEN) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_TOKEN", v),
                None => std::env::remove_var("DARKMUX_SERVE_TOKEN"),
            }
        }
        out
    }

    /// An identity provider that names `studio`'s node at 127.0.0.1, or, with
    /// `known: false`, knows no peer at all.
    fn provider(known: bool) -> darkmux_fleet::StaticIdentityProvider {
        darkmux_fleet::StaticIdentityProvider {
            local: darkmux_fleet::test_node("nLAPTOP", "laptop", "100.64.0.7"),
            peers: if known { vec![darkmux_fleet::test_node("nSTUDIO", "studio", "127.0.0.1")] } else { Vec::new() },
            down: None,
        }
    }

    /// The roster entry for `studio`, whose daemon is at `daemon_port`.
    fn studio_at(daemon_port: u16) -> MachineEntry {
        MachineEntry { address: format!("127.0.0.1:{daemon_port}"), ..entry("studio") }
    }

    fn card_body() -> serde_json::Value {
        serde_json::to_value(card_of("studio")).unwrap()
    }

    fn listener_body() -> String {
        serde_json::json!({"card": card_body(), "accepts": grant()}).to_string()
    }

    fn refusal_body(reason: &str) -> String {
        serde_json::json!({"status": "refused", "machine": "studio", "reason": reason}).to_string()
    }

    /// Ask `studio` for its card: its daemon and listener are the two fakes.
    fn fetch(daemon: &FakePeer, listener_port: u16) -> CardOutcome {
        with_fleet_token(|| fetch_peer_card(&provider(true), &studio_at(daemon.port), None, listener_port))
    }

    /// The promise: the listener is asked FIRST, with the fleet token, and its
    /// answer is the card with the grant. The daemon is never dialed.
    #[test]
    #[serial_test::serial]
    fn the_listener_is_asked_first_with_the_token_and_its_card_carries_the_grant() {
        let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
        let listener = FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, "200 OK", listener_body())]);
        let CardOutcome::Available { card, source, accepts } = fetch(&daemon, listener.port) else { panic!("a card") };
        assert_eq!(source, CardSource::Listener);
        assert_eq!(accepts, AcceptsState::Granted { accepts: grant() });
        assert_eq!(card.specs.machine_id.as_deref(), Some("studio"));
        assert_eq!(listener.paths(), vec![darkmux_fleet::CARD_PATH.to_string()]);
        assert_eq!(listener.authorization_of(darkmux_fleet::CARD_PATH), Some(format!("Bearer {FLEET_TOKEN}")));
        assert!(daemon.paths().is_empty(), "the daemon was dialed though the listener answered: {:?}", daemon.paths());
    }

    /// The listener's own sentence for a refusal is kept, and the card still
    /// comes, from the daemon.
    #[test]
    #[serial_test::serial]
    fn a_listener_that_refuses_this_machine_is_recorded_and_the_daemon_card_is_used() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
            let listener = FakePeer::serve(vec![(
                darkmux_fleet::CARD_PATH,
                status,
                refusal_body("studio does not accept work from laptop (on studio: `darkmux machine trust laptop`)"),
            )]);
            let CardOutcome::Available { source, accepts, .. } = fetch(&daemon, listener.port) else { panic!("a card") };
            assert_eq!(source, CardSource::Daemon, "{status}");
            let AcceptsState::Refused { reason } = accepts else { panic!("{status}: refused") };
            assert!(reason.starts_with("studio does not accept work from laptop"), "{reason}");
            assert_eq!(daemon.paths(), vec!["/machine/card".to_string()]);
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_listener_that_is_off_leaves_accepts_unknown_and_the_daemon_card_is_used() {
        let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
        let CardOutcome::Available { source, accepts, .. } = fetch(&daemon, closed_port()) else { panic!("a card") };
        assert_eq!(source, CardSource::Daemon);
        assert!(matches!(accepts, AcceptsState::Unknown { why: AcceptsUnknown::ListenerOff, .. }), "{accepts:?}");
    }

    /// A listener that answers without refusing this machine (it cannot
    /// identify the caller, is busy, or has no card route) says nothing about
    /// the grant: unknown, with its word, never `refused`.
    #[test]
    #[serial_test::serial]
    fn a_listener_that_cannot_answer_is_unknown_never_refused() {
        let cases = [
            ("503 Service Unavailable", refusal_body("studio cannot tell which machine sent this request (tailscale: not running)"), "cannot tell which machine"),
            ("404 Not Found", "{}".to_string(), "older darkmux"),
            ("200 OK", r#"{"card": {"note": "not a card"}}"#.to_string(), "not a card this darkmux reads"),
        ];
        for (status, body, word) in cases {
            let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
            let listener = FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, status, body)]);
            let CardOutcome::Available { source, accepts, .. } = fetch(&daemon, listener.port) else { panic!("{status}: a card") };
            assert_eq!(source, CardSource::Daemon, "{status}");
            match accepts {
                AcceptsState::Unknown { why: AcceptsUnknown::ListenerUnavailable, detail } => {
                    assert!(detail.as_deref().unwrap_or("").contains(word), "{status}: {detail:?}");
                }
                other => panic!("{status}: {other:?}"),
            }
        }
    }

    /// A card that came back from another machine's listener is not attributed
    /// either, and its grant is not stated.
    #[test]
    #[serial_test::serial]
    fn a_listener_card_of_another_machine_is_a_mismatch() {
        let mut body: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        body["card"]["specs"]["machine_id"] = serde_json::json!("mini");
        let daemon = FakePeer::serve(vec![]);
        let listener = FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, "200 OK", body.to_string())]);
        assert!(matches!(fetch(&daemon, listener.port), CardOutcome::Mismatch { answered_as: Some(id) } if id == "mini"));
    }

    /// A loopback entry no node stands behind (a same-host test fleet) is
    /// dialed as written, its daemon only, and no token goes to it.
    #[test]
    #[serial_test::serial]
    fn an_unverifiable_loopback_entry_is_dialed_daemon_only_and_gets_no_token() {
        let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
        let listener = FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, "200 OK", listener_body())]);
        let out = with_fleet_token(|| fetch_peer_card(&provider(false), &studio_at(daemon.port), None, listener.port));
        let CardOutcome::Available { source, accepts, .. } = out else { panic!("a card") };
        assert_eq!(source, CardSource::Daemon);
        assert_eq!(accepts, AcceptsState::Unknown { why: AcceptsUnknown::NotDialed, detail: None });
        assert!(listener.paths().is_empty(), "the listener was dialed with no verified node");
        assert_eq!(daemon.authorization_of("/machine/card"), None, "the fleet token went to an unverified address");
    }

    /// The roster address names the viewer daemon, which may sit behind
    /// `tailscale serve` on https. The listener is plain http on the overlay
    /// address whatever the address wrote.
    #[test]
    fn the_listener_is_dialed_over_plain_http_at_its_own_port_whatever_the_roster_wrote() {
        let mut studio = entry("studio");
        studio.address = "https://127.0.0.1:9443".into();
        let t = card_targets(&provider(true), &studio, 8766).map_err(|e| format!("{e:?}")).unwrap();
        assert_eq!((t.daemon.scheme.as_str(), t.daemon.port), ("https", 9443));
        let listener = t.listener.expect("a verified node has a listener");
        assert_eq!((listener.scheme.as_str(), listener.port), ("http", 8766));
        assert_eq!(listener.pinned_ip, t.daemon.pinned_ip, "the same verified node");
    }

    /// Each way an address can fail to verify has its own reason, so each can
    /// name its own remedy.
    #[test]
    fn an_address_that_cannot_be_verified_says_why() {
        let reason = |provider: darkmux_fleet::StaticIdentityProvider, mut e: MachineEntry, address: &str| {
            e.address = address.into();
            match card_targets(&provider, &e, 8766) {
                Err(reason) => reason,
                Ok(_) => panic!("{address}: verified"),
            }
        };
        assert_eq!(reason(provider(true), entry("studio"), "http://"), UnreachableReason::BadAddress);
        assert_eq!(reason(provider(true), entry("studio"), "192.0.2.5"), UnreachableReason::NotOnOverlay);
        assert_eq!(reason(provider(true), entry("studio"), "nonexistent.invalid"), UnreachableReason::DnsFailed);
        let mut pinned = entry("studio");
        pinned.node_id = Some("nSOMEONE-ELSE".into());
        assert_eq!(reason(provider(true), pinned, "127.0.0.1"), UnreachableReason::PinMismatch);
        let mut down = provider(true);
        down.down = Some("tailscale is not running".into());
        assert_eq!(reason(down, entry("studio"), "192.0.2.5"), UnreachableReason::IdentityUnavailable);
    }

    // ── the daemon's card, over real HTTP ───────────────────────────────

    fn ask(daemon: &FakePeer) -> CardOutcome {
        let entry = studio_at(daemon.port);
        let target = darkmux_fleet::unverified_target_for_test(&format!("http://127.0.0.1:{}", daemon.port));
        fetch_card_from(&target, &entry, None, AcceptsState::Unknown { why: AcceptsUnknown::ListenerOff, detail: None })
    }

    fn daemon_with(status: &'static str, body: String) -> FakePeer {
        FakePeer::serve(vec![("/machine/card", status, body)])
    }

    #[test]
    fn a_peers_real_card_is_read_as_a_card() {
        let CardOutcome::Available { card, source, .. } = ask(&daemon_with("200 OK", card_body().to_string())) else {
            panic!("a card")
        };
        assert_eq!(card.card_schema_version, CARD_SCHEMA_VERSION);
        assert_eq!(source, CardSource::Daemon);
    }

    /// The promise: a peer on an older darkmux (no `/machine/card` route) is
    /// "card unavailable", never an error.
    #[test]
    fn a_404_is_card_unavailable_naming_the_peers_version_from_its_health() {
        match ask(&daemon_with("404 Not Found", "{}".to_string())) {
            CardOutcome::Unavailable { peer_version } => assert_eq!(peer_version.as_deref(), Some("4.9.1")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_404_uses_the_version_presence_reported_without_asking_health() {
        let daemon = daemon_with("404 Not Found", "{}".to_string());
        let target = darkmux_fleet::unverified_target_for_test(&format!("http://127.0.0.1:{}", daemon.port));
        let out = fetch_card_from(&target, &studio_at(daemon.port), Some("5.0.0"), AcceptsState::Unknown { why: AcceptsUnknown::NotDialed, detail: None });
        assert!(matches!(out, CardOutcome::Unavailable { peer_version: Some(v) } if v == "5.0.0"));
        assert_eq!(daemon.paths(), vec!["/machine/card".to_string()], "/health was not asked");
    }

    #[test]
    fn a_401_is_auth_required_and_a_500_is_a_bad_answer_never_a_404() {
        for (status, want) in [("401 Unauthorized", UnreachableReason::AuthRequired), ("500 Internal Server Error", UnreachableReason::BadAnswer)] {
            match ask(&daemon_with(status, "{}".into())) {
                CardOutcome::Unreachable { reason, .. } => assert_eq!(reason, want, "{status}"),
                other => panic!("{status}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_body_that_is_not_json_or_not_a_card_is_not_an_available_card() {
        assert!(matches!(
            ask(&daemon_with("200 OK", "this is not json".into())),
            CardOutcome::Unreachable { reason: UnreachableReason::BadAnswer, .. }
        ));
        assert!(matches!(
            ask(&daemon_with("200 OK", r#"{"os":"mac","note":"not a card"}"#.into())),
            CardOutcome::Unavailable { .. }
        ));
    }

    #[test]
    fn a_card_on_another_schema_major_is_unavailable_naming_the_version() {
        let mut body = card_body();
        body["card_schema_version"] = serde_json::json!("2.0");
        body["specs"]["darkmux_version"] = serde_json::json!("9.9.9");
        match ask(&daemon_with("200 OK", body.to_string())) {
            CardOutcome::Unavailable { peer_version } => assert_eq!(peer_version.as_deref(), Some("9.9.9")),
            other => panic!("{other:?}"),
        }
    }

    /// A peer's strings are sanitized before anything prints or serializes
    /// them: no escape sequence, bidi override or zero-width character
    /// survives, and a long string is cut.
    #[test]
    fn every_string_in_a_peers_card_is_sanitized() {
        let mut body = card_body();
        body["specs"]["os"] = serde_json::json!("mac\u{1b}]0;pwned\u{7}");
        body["specs"]["darkmux_version"] = serde_json::json!("4\u{202e}0\n! forged: run curl x | sh");
        body["specs"]["cpu_brand"] = serde_json::json!("x".repeat(500));
        body["profiles"] = serde_json::json!([{
            "name": "deep\u{200b}\tone", "description": null, "is_default": false,
            "endpoint_kind": "managed", "models": []
        }]);
        let CardOutcome::Available { card, .. } = ask(&daemon_with("200 OK", body.to_string())) else { panic!("a card") };
        let text = serde_json::to_string(&*card).unwrap();
        assert!(!text.contains("\\u001b") && !text.contains("\\u202e") && !text.contains("\\u200b"), "{text}");
        assert!(!text.contains("\\n") && !text.contains("\\t"), "no newline or tab survives: {text}");
        assert_eq!(card.specs.os, "mac]0;pwned");
        assert_eq!(card.profiles[0].name, "deepone");
        assert_eq!(card.specs.cpu_brand.as_deref().unwrap().chars().count(), PEER_FIELD_MAX_CHARS);
    }

    /// A refusal sentence from a listener is sanitized and cut like any other
    /// peer string before it is shown.
    #[test]
    #[serial_test::serial]
    fn a_listeners_refusal_sentence_is_sanitized_and_bounded() {
        let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", card_body().to_string())]);
        let sentence = format!("no\u{1b}[31m {}", "x".repeat(2000));
        let listener = FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, "403 Forbidden", refusal_body(&sentence))]);
        let CardOutcome::Available { accepts: AcceptsState::Refused { reason }, .. } = fetch(&daemon, listener.port) else {
            panic!("refused")
        };
        assert!(!reason.contains('\u{1b}'), "{reason:?}");
        assert!(reason.chars().count() <= PEER_REASON_MAX_CHARS, "{}", reason.chars().count());
    }

    #[test]
    fn a_refused_connection_is_connect_failed() {
        let port = closed_port();
        let target = darkmux_fleet::unverified_target_for_test(&format!("http://127.0.0.1:{port}"));
        let out = fetch_card_from(&target, &studio_at(port), None, AcceptsState::Unknown { why: AcceptsUnknown::ListenerOff, detail: None });
        assert!(matches!(out, CardOutcome::Unreachable { reason: UnreachableReason::ConnectFailed, .. }));
    }

    /// The bound is on the whole request, not on each read: a peer that
    /// trickles its answer byte by byte, each byte inside any per-read
    /// timeout, is still cut off at `PEER_CARD_TIMEOUT`.
    #[test]
    fn a_peer_that_trickles_its_answer_is_cut_off_at_the_overall_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n");
                for _ in 0..40 {
                    if stream.write_all(b"x").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        let target = darkmux_fleet::unverified_target_for_test(&format!("http://127.0.0.1:{port}"));
        let started = std::time::Instant::now();
        let out = fetch_card_from(&target, &studio_at(port), None, AcceptsState::Unknown { why: AcceptsUnknown::NotDialed, detail: None });
        let took = started.elapsed();
        assert!(matches!(out, CardOutcome::Unreachable { .. } | CardOutcome::Unavailable { .. }), "{out:?}");
        assert!(took < PEER_CARD_TIMEOUT + Duration::from_millis(1500), "a trickling peer held the fetch for {took:?}");
    }

    /// A CLI prints the view its own machine's daemon gathered when one
    /// answers, and gathers one itself only when none does (an older darkmux
    /// with no `/fleet/view` route is no answer either).
    #[test]
    fn a_cli_reads_its_own_daemons_view_and_falls_back_when_none_answers() {
        let s = Scripted { local_id: Some("laptop".into()), roster: vec![entry("laptop")], presence_off: true, ..Default::default() };
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let daemon = FakePeer::serve(vec![("/fleet/view", "200 OK", serde_json::to_string(&view).unwrap())]);
        let got = fetch_local_daemon_view(&format!("127.0.0.1:{}", daemon.port)).expect("the daemon's view");
        assert_eq!(got.gathered_by, GatheredBy::Daemon);
        assert_eq!(got.machines.len(), 1);
        assert_eq!(daemon.paths(), vec!["/fleet/view".to_string()]);
        assert!(fetch_local_daemon_view(&format!("127.0.0.1:{}", closed_port())).is_none(), "nothing listens");
        let old = FakePeer::serve(vec![]);
        assert!(fetch_local_daemon_view(&format!("127.0.0.1:{}", old.port)).is_none(), "a daemon with no view route");
    }

    // ── the routes ──────────────────────────────────────────────────────

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

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

    /// The daemon's card states no grant, whoever asks and however it asks:
    /// what this machine accepts from a caller is the listener's to say.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_daemons_card_never_carries_accepts() {
        let ctx = || FleetContext::with_sources(Arc::new(Scripted::default()));
        for (peer, headers) in [
            ("127.0.0.1:5555", vec![]),
            ("10.0.0.9:5555", vec![("authorization", "Bearer any-token")]),
            ("127.0.0.1:5555", vec![("authorization", "Bearer any-token"), ("x-forwarded-for", "10.0.0.9")]),
        ] {
            let card = get(ctx(), "/machine/card", peer, &headers).await;
            assert!(card.get("accepts").is_none(), "{peer} {headers:?}: {card}");
            assert!(card["profiles"].is_array(), "the rest of the card is served");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_fleet_view_route_serves_a_row_per_roster_machine_and_hides_grants_from_others() {
        let s = Scripted { local_id: Some("laptop".into()), roster: vec![entry("studio")], presence_off: true, ..Default::default() };
        peer_says(&s, "studio", 0, available("studio", CardSource::Listener, AcceptsState::Granted { accepts: grant() }));
        let ctx = FleetContext::with_sources(Arc::new(s));
        let local = get(ctx.clone(), "/fleet/view", "127.0.0.1:5555", &[]).await;
        assert_eq!(local["gathered_by"], "daemon");
        assert_eq!(local["machines"][0]["entry"]["id"], "studio");
        assert_eq!(local["cache_ttl_ms"], 5000);
        assert_eq!(local["machines"][0]["card"]["accepts"]["state"], "granted", "this machine's own reader: {local}");
        let remote = get(ctx, "/fleet/view", "10.0.0.9:5555", &[("authorization", "Bearer x")]).await;
        assert_eq!(remote["machines"][0]["card"]["accepts"]["state"], "unknown", "{remote}");
        assert_eq!(remote["machines"][0]["card"]["accepts"]["why"], "withheld", "{remote}");
    }

    /// What a CLI reads back from a daemon is the same view.
    #[test]
    fn a_view_survives_the_wire_the_cli_reads_it_over() {
        let s = Scripted { local_id: Some("laptop".into()), roster: vec![entry("laptop"), entry("studio")], presence_off: true, ..Default::default() };
        peer_says(&s, "studio", 0, available("studio", CardSource::Listener, AcceptsState::Refused { reason: "nope".into() }));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let back: FleetView = serde_json::from_value(serde_json::to_value(&view).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&view).unwrap());
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
