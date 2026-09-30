//! The fleet view: one row per machine, each carrying that machine's own
//! card. Every daemon gathers its own view (`GET /fleet/view`), and
//! `darkmux machine list` prints the same gather, so any machine asking gets
//! the same kind of answer with no hub in the way.
//!
//! **The channel.** A card comes from the peer's FLEET LISTENER
//! (`GET /fleet/card`), the one surface that can say who is calling: it
//! binds the overlay address, so it verifies the caller's node from the
//! socket, which the daemon behind `tailscale serve` never can. There is no
//! second channel and no fallback: `darkmux_fleet::peer_target` checks the
//! roster address is the pinned overlay node before `darkmux_fleet::fleet_get`
//! sends the fleet token, and a peer whose listener is off is shown as
//! unreachable with a typed reason ([`UnreachableReason::ListenerOff`]).
//! Cards never come from Redis: anything that can write the shared Redis
//! could forge a key, and a card drives routing and data-boundary decisions.
//!
//! **Symmetry.** A machine sees a peer's card whether or not that peer lets
//! it run work: the listener authenticates the caller and reports the grant
//! as data, never as a gate ([`AcceptsState`]).
//!
//! **Every peer is dialed.** Presence is a display column ([`Liveness`]),
//! never a gate: an unreadable or forged Redis cannot hide a machine that is
//! up. The whole view is single-flight and cached for [`FLEET_VIEW_CACHE_TTL`]
//! (recorded in the view as `cache_ttl_ms`, never adaptive).
//!
//! **This machine always has a row.** It is recognized by the verified node
//! behind a roster address, else by hardware uid, else by name, and its card
//! is built locally with no HTTP. A roster entry that is this machine under
//! another name folds into that row (`is_this_machine`); when the roster has
//! no entry for it the row has `entry: null`.
//!
//! **What a row can say.** Exactly one of: the peer's card; "card
//! unavailable" (a darkmux with no card route, or a card this darkmux cannot
//! read, each with its own [`UnavailableWhy`]); "mismatch" (the answering
//! machine is not the one the roster entry names, so its card is not
//! attributed); or "unreachable" with a typed reason. The view never fills in
//! a fact a card did not state.
//!
//! **Forward compatibility.** Every enum here has an `unknown` arm a reader
//! falls to for a value a newer darkmux invented. `Unknown` is never read as
//! any known value.

use crate::machine_card::{gather_local_card, CardAccepts, CardGrant, ListenerCard, MachineCard, CARD_SCHEMA_VERSION};
use crate::source_state::SourceState;
use crate::wire::RosterMachineEntry;
use darkmux_fleet::{IdentityProvider, MachineEntry, PeerTarget, TargetError};
use darkmux_flow::presence::PresenceBeat;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// How long a gathered view is served before the next request gathers again.
/// Recorded in every view as `cache_ttl_ms`.
pub(crate) const FLEET_VIEW_CACHE_TTL: Duration = Duration::from_secs(5);

/// The longest ONE card request to a peer may take, overall: connecting,
/// sending and reading the answer. The identity check and DNS before it are
/// bounded by their own limits, not by this one.
const PEER_CARD_TIMEOUT: Duration = Duration::from_millis(2000);

/// The longest a refusal sentence from a peer's listener is kept.
const PEER_REASON_MAX_CHARS: usize = 400;

/// The most a peer's card may weigh: a card is a few KB, and a peer that
/// answers with more is not sending one.
const MAX_PEER_CARD_BYTES: u64 = 1024 * 1024;

/// The longest single string a peer's card may carry into this view.
const PEER_FIELD_MAX_CHARS: usize = 200;

/// Whether a machine has a live presence beat. Presence is a hint and a
/// display column: `unknown` when the shared Redis is off or could not be
/// read, and `no_beat` says only that no beat was found, never that the
/// machine is down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Live,
    NoBeat,
    /// Presence could not say, or a value a newer darkmux states.
    #[serde(other)]
    Unknown,
}

/// Why a card could not be read from a machine that may be up. Each reason
/// has its own remedy, so none stands for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum UnreachableReason {
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
    /// Nothing answered on the peer's fleet listener port: its listener is
    /// off, or not reachable from this machine. `detail` carries the
    /// transport's word.
    ListenerOff,
    /// The peer's listener answered 401: this machine's fleet token is
    /// missing or not the peer's. `detail` carries its sentence.
    AuthRequired,
    /// The peer's listener answered 403: it could not place this machine's
    /// address on its network, or the request came from its own node.
    /// `detail` carries its sentence.
    RefusedByPeer,
    /// The peer's listener answered that it cannot serve now (503 or 429): it
    /// has no fleet token, cannot identify its callers, or is at capacity.
    /// `detail` carries its sentence.
    ListenerUnavailable,
    /// The peer answered, with something that is not a card or a status this
    /// darkmux knows what to do with.
    BadAnswer,
    /// A reason a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// Which endpoint a card came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum CardSource {
    /// This machine's own card, built in this process.
    Local,
    /// The peer's fleet listener.
    Listener,
    /// A source a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// What a machine lets THIS machine do, as far as this machine knows: yes
/// (with the entry), no entry, or not known. It is a fact about the row's
/// machine, beside its card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AcceptsState {
    /// The peer's allow-list has an entry for this machine: `accepts` is that
    /// entry, and no other.
    Granted { accepts: CardAccepts },
    /// The peer authenticated this machine and has no entry for it: it takes
    /// no work from this machine, and its card is still shown.
    NotListed,
    /// The row is this machine: it does not send work to itself.
    ThisMachine,
    /// A reader that is neither on this machine nor holding the fleet token
    /// is not shown what this machine's peers let it do.
    Withheld,
    /// No answer: no card came back, the peer could not say, or a state a
    /// newer darkmux states. Never read as granted or as not listed.
    #[serde(other)]
    Unknown,
}

/// Where a row's `machine_uid` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum UidSource {
    /// The machine's own card, read over the verified channel.
    Card,
    /// The roster entry declares it.
    Declared,
    /// Derived from this machine's flow history under the entry's name.
    FlowHistory,
    /// A source a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// Why a machine that answered has no card this darkmux reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum UnavailableWhy {
    /// Its listener has no card route: a darkmux older than the card.
    NoCardRoute,
    /// It sent a card on another schema major than this darkmux reads.
    OtherSchemaMajor,
    /// It sent a card of this major that does not parse: a writer bug on the
    /// peer, not an older darkmux.
    Unparseable,
    /// A reason a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// Whose word a `peer_version` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum VersionSource {
    /// The peer's own answer.
    Peer,
    /// The peer's presence beat, which anything that can write the shared
    /// Redis could have forged.
    Presence,
    /// A source a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
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
    },
    /// The machine answered, but has no card this darkmux reads. Not an
    /// error: `why` says which of the three it is.
    Unavailable {
        why: UnavailableWhy,
        /// The darkmux version the peer reports, when one is known.
        peer_version: Option<String>,
        /// Whose word `peer_version` is; `null` with no version.
        peer_version_source: Option<VersionSource>,
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
        /// The peer's sentence or the transport's word, when there was one.
        detail: Option<String>,
    },
    /// An outcome a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// One machine and what it said about itself.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetMachine {
    /// The roster entry this row is for; `null` only for this machine's own
    /// row when its roster has no entry that is this machine.
    pub entry: Option<RosterMachineEntry>,
    /// This machine was recognized by the verified node behind the entry's
    /// address, else by hardware uid, else by name.
    pub is_this_machine: bool,
    /// The row's resolved hardware identity: the verified card's, else the
    /// entry's declared one, else the flow-history derivation. `null` when
    /// none is known.
    pub machine_uid: Option<String>,
    /// Where `machine_uid` came from; `null` with no uid.
    pub uid_source: Option<UidSource>,
    pub liveness: Liveness,
    /// The presence beat's own timestamp, on the PEER's clock: display only.
    /// `null` without a beat.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub last_beat_ms: Option<u64>,
    /// When THIS machine got the peer's answer, on this machine's clock: an
    /// age is computed from it, never from the card's own timestamp. `null`
    /// when nothing answered.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub received_at_ms: Option<u64>,
    /// What producing this row cost, in milliseconds.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub fetch_ms: Option<u64>,
    pub card: CardOutcome,
    /// What the machine lets THIS machine do.
    pub accepts: AcceptsState,
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
    /// A gatherer a newer darkmux states that this one does not know.
    #[serde(other)]
    Unknown,
}

/// `GET /fleet/view`: every machine and its card, as this daemon gathered
/// them.
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
    /// Why the roster contributed no entries when its file exists and could
    /// not be parsed; `null` otherwise. A fixed sentence.
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
    /// Every roster machine, and this machine's own row (always present).
    pub machines: Vec<FleetMachine>,
}

impl FleetView {
    /// The view as a reader outside this machine's operator side may see it:
    /// without what each peer lets this machine run (those entries state what
    /// each peer trusts this machine with) and without any card's seat block
    /// (`/health` withholds the same fact from the same readers).
    pub fn for_outside_reader(mut self) -> Self {
        for m in &mut self.machines {
            if !m.is_this_machine {
                m.accepts = AcceptsState::Withheld;
            }
            if let CardOutcome::Available { card, .. } = &mut m.card {
                card.seats = None;
            }
        }
        self
    }
}

/// This machine as it knows itself, for recognizing its own row.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalIdentity {
    pub machine_id: Option<String>,
    pub machine_uid: Option<String>,
    /// The node the identity provider names as this machine.
    pub node_id: Option<String>,
}

/// What asking a verified peer for its card produced.
pub(crate) struct Fetched {
    pub outcome: CardOutcome,
    pub accepts: AcceptsState,
}

/// Everything a gather reads, so the gather itself is the same code in the
/// daemon, in the CLI and under a test that scripts the peers.
pub(crate) trait FleetSources: Send + Sync {
    /// Which process this gather runs in.
    fn gathered_by(&self) -> GatheredBy;
    fn local_identity(&self) -> LocalIdentity;
    /// The roster (the one reader `/fleet/roster` shares), or the fixed
    /// sentence for a roster file that exists and does not parse.
    fn roster(&self) -> Result<crate::ResolvedRoster, &'static str>;
    fn presence(&self) -> (Vec<PresenceBeat>, SourceState);
    fn local_card(&self) -> MachineCard;
    /// Verify the node behind a roster entry's address and aim at its fleet
    /// listener. Blocking (the identity lookup).
    fn verify_peer(&self, entry: &MachineEntry) -> Result<PeerTarget, UnreachableReason>;
    /// Ask a verified peer's listener for its card. Blocking.
    /// `known_version` is the darkmux version its presence beat reported.
    fn fetch_card(&self, target: &PeerTarget, entry: &MachineEntry, known_version: Option<&str>) -> Fetched;
}

/// Whether a roster entry is THIS machine. Decided by the strongest evidence
/// there is: the verified node behind the entry's address against this
/// machine's own node; when no node was verified, the hardware uid when both
/// sides have one; else the name (the roster id is the name flow records and
/// allow-lists carry). Evidence that exists and disagrees decides "no": a
/// different node is not this machine whatever the name says.
fn is_this_machine(local: &LocalIdentity, verified_node: Option<&str>, entry: &MachineEntry) -> bool {
    if let (Some(mine), Some(theirs)) = (local.node_id.as_deref(), verified_node) {
        return mine == theirs;
    }
    if let (Some(mine), Some(theirs)) = (local.machine_uid.as_deref(), entry.machine_uid.as_deref()) {
        return mine.eq_ignore_ascii_case(theirs);
    }
    local.machine_id.as_deref().is_some_and(|id| darkmux_fleet::same_machine(id, &entry.id))
}

/// The beat that belongs to a machine: by hardware identity when it has one,
/// else by the name flow records carry.
fn beat_for<'a>(uid: Option<&str>, name: &str, beats: &'a [PresenceBeat]) -> Option<&'a PresenceBeat> {
    beats.iter().find(|b| match uid {
        Some(uid) => b.machine_uid.eq_ignore_ascii_case(uid),
        None => darkmux_fleet::same_machine(&b.display_name, name),
    })
}

fn liveness_of(presence: &SourceState, beat: Option<&PresenceBeat>) -> Liveness {
    match (presence, beat) {
        (SourceState::Ok, Some(_)) => Liveness::Live,
        (SourceState::Ok, None) => Liveness::NoBeat,
        (SourceState::Off | SourceState::Stale { .. } | SourceState::Unavailable { .. }, _) => Liveness::Unknown,
    }
}

fn unfinished_row(entry: &MachineEntry) -> FleetMachine {
    FleetMachine {
        entry: Some(RosterMachineEntry::from(entry)),
        is_this_machine: false,
        machine_uid: entry.machine_uid.clone(),
        uid_source: None,
        liveness: Liveness::Unknown,
        last_beat_ms: None,
        received_at_ms: None,
        fetch_ms: None,
        card: CardOutcome::Unreachable {
            reason: UnreachableReason::BadAnswer,
            detail: Some("the card fetch did not finish".to_string()),
        },
        accepts: AcceptsState::Unknown,
    }
}

/// What a gather knows while it builds rows.
struct GatherCtx<'a> {
    src: &'a dyn FleetSources,
    local: LocalIdentity,
    beats: Vec<PresenceBeat>,
    presence: SourceState,
    /// This machine's card, gathered once per view however many rows are it.
    local_card: std::sync::OnceLock<MachineCard>,
}

impl GatherCtx<'_> {
    fn local_card(&self) -> MachineCard {
        self.local_card.get_or_init(|| self.src.local_card()).clone()
    }

    /// A resolved uid: the card's own when it read, else the entry's.
    fn uid_of(outcome: &CardOutcome, entry: Option<&MachineEntry>, from_history: bool) -> (Option<String>, Option<UidSource>) {
        if let CardOutcome::Available { card, .. } = outcome {
            if let Some(uid) = card.specs.machine_uid.clone() {
                return (Some(uid), Some(UidSource::Card));
            }
        }
        match entry.and_then(|e| e.machine_uid.clone()) {
            Some(uid) => (Some(uid), Some(if from_history { UidSource::FlowHistory } else { UidSource::Declared })),
            None => (None, None),
        }
    }

    /// The liveness and beat timestamp of a machine, from presence.
    fn liveness(&self, uid: Option<&str>, name: &str, is_self: bool) -> (Liveness, Option<u64>) {
        let beat = beat_for(uid, name, &self.beats);
        let liveness = if is_self { Liveness::Live } else { liveness_of(&self.presence, beat) };
        (liveness, beat.map(|b| b.beat_ts_ms))
    }

    /// This machine's row: its card, built here, with no HTTP.
    fn self_row(&self, entry: Option<&MachineEntry>, from_history: bool, started: std::time::Instant) -> FleetMachine {
        let outcome = CardOutcome::Available { card: Box::new(self.local_card()), source: CardSource::Local };
        let (machine_uid, uid_source) = Self::uid_of(&outcome, entry, from_history);
        let name = entry.map(|e| e.id.as_str()).or(self.local.machine_id.as_deref()).unwrap_or_default();
        let (liveness, last_beat_ms) = self.liveness(machine_uid.as_deref(), name, true);
        FleetMachine {
            entry: entry.map(RosterMachineEntry::from),
            is_this_machine: true,
            machine_uid,
            uid_source,
            liveness,
            last_beat_ms,
            received_at_ms: Some(crate::current_millis()),
            fetch_ms: Some(started.elapsed().as_millis() as u64),
            card: outcome,
            accepts: AcceptsState::ThisMachine,
        }
    }

    /// One roster peer's row: verify the node behind its address, recognize
    /// this machine if that is who it is, else ask its listener.
    fn entry_row(&self, entry: &MachineEntry, from_history: bool) -> FleetMachine {
        let started = std::time::Instant::now();
        let route = self.src.verify_peer(entry);
        let verified_node = route.as_ref().ok().and_then(|t| t.node_id.as_deref());
        if is_this_machine(&self.local, verified_node, entry) {
            return self.self_row(Some(entry), from_history, started);
        }
        let known = beat_for(entry.machine_uid.as_deref(), &entry.id, &self.beats).and_then(|b| b.darkmux_version.as_deref());
        let (card, accepts) = match route {
            Ok(target) => {
                let f = self.src.fetch_card(&target, entry, known);
                (f.outcome, f.accepts)
            }
            Err(reason) => (CardOutcome::Unreachable { reason, detail: None }, AcceptsState::Unknown),
        };
        let (machine_uid, uid_source) = Self::uid_of(&card, Some(entry), from_history);
        let (liveness, last_beat_ms) = self.liveness(machine_uid.as_deref(), &entry.id, false);
        let answered = matches!(
            card,
            CardOutcome::Available { .. } | CardOutcome::Unavailable { .. } | CardOutcome::Mismatch { .. }
        );
        FleetMachine {
            entry: Some(RosterMachineEntry::from(entry)),
            is_this_machine: false,
            machine_uid,
            uid_source,
            liveness,
            last_beat_ms,
            received_at_ms: answered.then(crate::current_millis),
            fetch_ms: Some(started.elapsed().as_millis() as u64),
            card,
            accepts,
        }
    }
}

/// Gather the view: roster, presence, then every peer at once. `ttl` is the
/// cache TTL this gather is served under (recorded, not enforced here).
pub(crate) fn gather_view(src: &dyn FleetSources, ttl: Duration) -> FleetView {
    let started = std::time::Instant::now();
    let (roster, roster_error) = match src.roster() {
        Ok(r) => (r, None),
        Err(sentence) => (crate::ResolvedRoster { machines: Vec::new(), uid_from_history: Default::default() }, Some(sentence.to_string())),
    };
    let (beats, presence) = src.presence();
    let ctx = GatherCtx { src, local: src.local_identity(), beats, presence, local_card: Default::default() };
    let mut machines: Vec<FleetMachine> = std::thread::scope(|scope| {
        let handles: Vec<_> = roster
            .machines
            .iter()
            .map(|entry| {
                let from_history = roster.uid_from_history.contains(&entry.id);
                let ctx = &ctx;
                (entry, scope.spawn(move || ctx.entry_row(entry, from_history)))
            })
            .collect();
        handles.into_iter().map(|(entry, h)| h.join().unwrap_or_else(|_| unfinished_row(entry))).collect()
    });
    if !machines.iter().any(|m| m.is_this_machine) {
        machines.insert(0, ctx.self_row(None, false, std::time::Instant::now()));
    }
    FleetView {
        gathered_by: src.gathered_by(),
        local_machine_id: ctx.local.machine_id.clone(),
        presence: ctx.presence,
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
    /// The daemon's card cache, so this machine's own row costs no gather of
    /// its own while the listener's card is warm; `None` in a CLI process,
    /// whose card is built for one reader.
    card_cache: Option<Arc<crate::machine_card::CardCache>>,
}

impl ProcessSources {
    pub(crate) fn new(
        provider: Arc<dyn IdentityProvider>,
        gathered_by: GatheredBy,
        card_cache: Option<Arc<crate::machine_card::CardCache>>,
    ) -> Self {
        Self { provider, gathered_by, card_cache }
    }
}

impl FleetSources for ProcessSources {
    fn gathered_by(&self) -> GatheredBy {
        self.gathered_by
    }

    fn local_identity(&self) -> LocalIdentity {
        LocalIdentity {
            machine_id: darkmux_flow::resolve_machine_id(),
            machine_uid: darkmux_hardware::machine_uid().map(str::to_string),
            node_id: self.provider.local_node().ok().map(|n| n.node_id),
        }
    }

    fn roster(&self) -> Result<crate::ResolvedRoster, &'static str> {
        crate::resolved_roster(&darkmux_types::config_access::flows_dir())
    }

    fn presence(&self) -> (Vec<PresenceBeat>, SourceState) {
        match darkmux_flow::redis_url() {
            Some(url) => crate::read_presence_beats(&url, "machines", darkmux_flow::presence::read_live),
            None => (Vec::new(), SourceState::Off),
        }
    }

    fn local_card(&self) -> MachineCard {
        match &self.card_cache {
            Some(cache) => cache.get(gather_local_card),
            None => gather_local_card(),
        }
    }

    fn verify_peer(&self, entry: &MachineEntry) -> Result<PeerTarget, UnreachableReason> {
        let listener_port = darkmux_types::config_access::fleet_listener_port();
        listener_target(self.provider.as_ref(), entry, listener_port)
    }

    fn fetch_card(&self, target: &PeerTarget, entry: &MachineEntry, known_version: Option<&str>) -> Fetched {
        fetch_listener_card(target, entry, known_version)
    }
}

/// This machine's view, gathered in THIS process: what `darkmux machine list`
/// prints when no daemon answers. Not cached (`cache_ttl_ms` is `0`); the
/// seat block and the governor readings of this machine's own card belong to
/// a running daemon, so a card built here has none (the view says so in
/// `gathered_by`).
pub fn gather_fleet_view_now() -> FleetView {
    let provider: Arc<dyn IdentityProvider> = Arc::from(darkmux_fleet::configured_provider_or_unavailable());
    gather_view(&ProcessSources::new(provider, GatheredBy::CliProcess, None), Duration::ZERO)
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

/// The verified node behind a roster entry, aimed at its fleet listener: plain
/// `http` at the listener's port on the overlay address, whatever scheme the
/// roster wrote (the address names the viewer daemon, which may sit behind
/// `tailscale serve` on https). The daemon never writes a first-contact pin
/// (the roster is operator state); `peer_target` still refuses an address that
/// is not the pinned node, before any token is attached. An address no node
/// stands behind, loopback included, is refused: the token goes nowhere
/// unverified.
fn listener_target(
    provider: &dyn IdentityProvider,
    entry: &MachineEntry,
    listener_port: u16,
) -> Result<PeerTarget, UnreachableReason> {
    let daemon_port = darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT;
    darkmux_fleet::peer_target(&entry.id, entry, None, daemon_port, false, provider)
        .map(|t| t.at_listener(listener_port))
        .map_err(|e| unreachable_for(&e))
}

fn unreachable(reason: UnreachableReason, detail: Option<String>) -> Fetched {
    Fetched { outcome: CardOutcome::Unreachable { reason, detail }, accepts: AcceptsState::Unknown }
}

/// Ask a verified peer's fleet listener for its card.
fn fetch_listener_card(target: &PeerTarget, entry: &MachineEntry, known_version: Option<&str>) -> Fetched {
    match darkmux_fleet::fleet_get(target, darkmux_fleet::CARD_PATH, PEER_CARD_TIMEOUT, &[]) {
        Ok(resp) => parse_listener_card(resp, entry, known_version),
        Err(ureq::Error::Status(code, resp)) => refusal_outcome(code, resp, known_version),
        Err(ureq::Error::Transport(t)) => unreachable(UnreachableReason::ListenerOff, Some(format!("{:?}", t.kind()))),
    }
}

/// The peer's own sentence in a non-200 body, sanitized and bounded.
fn peer_sentence(resp: ureq::Response) -> Option<String> {
    let mut v = read_json(resp)?;
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_REASON_MAX_CHARS);
    v.get("reason").and_then(serde_json::Value::as_str).map(str::to_string)
}

/// What a listener's non-200 answer says. 401 and 403 are the authentication
/// gate (its own sentence rides the body); 404 is a listener with no card
/// route; 429 and 503 are a listener that cannot serve now. None of these
/// says anything about the allow-list: that is the grant of a 200.
fn refusal_outcome(code: u16, resp: ureq::Response, known_version: Option<&str>) -> Fetched {
    if code == 404 {
        let (peer_version, peer_version_source) = pick_version(None, known_version);
        return Fetched {
            outcome: CardOutcome::Unavailable { why: UnavailableWhy::NoCardRoute, peer_version, peer_version_source },
            accepts: AcceptsState::Unknown,
        };
    }
    let reason = match code {
        401 => UnreachableReason::AuthRequired,
        403 => UnreachableReason::RefusedByPeer,
        429 | 503 => UnreachableReason::ListenerUnavailable,
        _ => UnreachableReason::BadAnswer,
    };
    let detail = peer_sentence(resp).or_else(|| Some(format!("HTTP {code}")));
    unreachable(reason, detail)
}

/// The peer's own reported version when it gave one, else the presence
/// beat's, and which it was. Both are sanitized and bounded: a beat can carry
/// any string a writer of the shared Redis put there.
fn pick_version(peer: Option<&str>, presence: Option<&str>) -> (Option<String>, Option<VersionSource>) {
    let clean = |s: &str| darkmux_fleet::truncate_chars(&darkmux_fleet::sanitize_remote_line(s), PEER_FIELD_MAX_CHARS);
    match (peer, presence) {
        (Some(v), _) => (Some(clean(v)), Some(VersionSource::Peer)),
        (None, Some(v)) => (Some(clean(v)), Some(VersionSource::Presence)),
        (None, None) => (None, None),
    }
}

fn unavailable(why: UnavailableWhy, peer: Option<&str>, presence: Option<&str>) -> Fetched {
    let (peer_version, peer_version_source) = pick_version(peer, presence);
    Fetched { outcome: CardOutcome::Unavailable { why, peer_version, peer_version_source }, accepts: AcceptsState::Unknown }
}

/// A listener's 200 answer, sanitized and typed. A body that is not JSON is a
/// bad answer; a card of another schema major, or of this major and not
/// parseable, is "unavailable" with its own reason.
fn parse_listener_card(resp: ureq::Response, entry: &MachineEntry, known_version: Option<&str>) -> Fetched {
    let Some(mut v) = read_json(resp) else {
        return unreachable(UnreachableReason::BadAnswer, Some("not a JSON document".to_string()));
    };
    darkmux_fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
    let peer_version = v.pointer("/card/specs/darkmux_version").and_then(serde_json::Value::as_str).map(str::to_string);
    if !same_schema_major(v.pointer("/card/card_schema_version").and_then(serde_json::Value::as_str)) {
        return unavailable(UnavailableWhy::OtherSchemaMajor, peer_version.as_deref(), known_version);
    }
    match serde_json::from_value::<ListenerCard>(v) {
        Ok(ListenerCard { card, grant }) => attribute(entry, card, CardSource::Listener, grant),
        Err(_) => unavailable(UnavailableWhy::Unparseable, peer_version.as_deref(), known_version),
    }
}

fn read_json(resp: ureq::Response) -> Option<serde_json::Value> {
    let mut body = String::new();
    resp.into_reader().take(MAX_PEER_CARD_BYTES).read_to_string(&mut body).ok()?;
    serde_json::from_str(&body).ok()
}

/// Whether a peer's card schema version shares this build's major.
fn same_schema_major(theirs: Option<&str>) -> bool {
    let major = |v: &str| v.split('.').next().map(str::to_string);
    theirs.and_then(major).is_some_and(|m| Some(m) == major(CARD_SCHEMA_VERSION))
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

/// A card as the row for `entry` shows it: attributed with the grant beside
/// it, or flagged as another machine's and dropped (its grant is dropped too:
/// it was said about a machine that is not the entry's).
fn attribute(entry: &MachineEntry, card: MachineCard, source: CardSource, grant: CardGrant) -> Fetched {
    if !card_is_of(entry, &card) {
        let answered_as = card.specs.machine_id.filter(|id| !id.is_empty());
        return Fetched { outcome: CardOutcome::Mismatch { answered_as }, accepts: AcceptsState::Unknown };
    }
    let accepts = match grant {
        CardGrant::Listed { accepts } => AcceptsState::Granted { accepts },
        CardGrant::NotListed => AcceptsState::NotListed,
        CardGrant::Unknown => AcceptsState::Unknown,
    };
    Fetched { outcome: CardOutcome::Available { card: Box::new(card), source }, accepts }
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
        let sources = ProcessSources::new(provider, GatheredBy::Daemon, Some(crate::machine_card::process_card_cache()));
        Self { sources: Arc::new(sources), cache: Arc::new(ViewCache::default()) }
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

/// `GET /fleet/view`. What peers let THIS machine do, and every card's seats,
/// go to a reader on this machine or one holding the fleet token: the
/// audience of the doctor panel and of every other read of the execution
/// surface ([`crate::caller_is_local_or_holds_token`]).
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
    let insider = crate::caller_is_local_or_holds_token(peer.map(|c| c.0), &headers);
    Ok(axum::Json(if insider { view } else { view.for_outside_reader() }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::machine_card::tests::sample_card;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Scripted inputs: a roster, presence, what verifying each peer says, and
    /// what asking it returns (after an optional delay); counts the dials.
    #[derive(Default)]
    pub(crate) struct Scripted {
        pub local: LocalIdentity,
        pub roster: Vec<MachineEntry>,
        pub from_history: Vec<String>,
        pub beats: Vec<PresenceBeat>,
        pub presence_off: bool,
        pub roster_error: Option<&'static str>,
        /// Per peer id: the verified node, or why verification failed. A peer
        /// with no script verifies as node `n-<id>`.
        pub verify: Mutex<std::collections::BTreeMap<String, Result<String, UnreachableReason>>>,
        pub fetches: Mutex<std::collections::BTreeMap<String, (u64, Fetched)>>,
        pub dials: Mutex<Vec<(String, Option<String>)>>,
        pub local_cards: AtomicUsize,
    }

    impl Clone for Fetched {
        fn clone(&self) -> Self {
            Self { outcome: self.outcome.clone(), accepts: self.accepts.clone() }
        }
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

    fn identity(id: &str, uid: Option<&str>, node: Option<&str>) -> LocalIdentity {
        LocalIdentity { machine_id: Some(id.into()), machine_uid: uid.map(str::to_string), node_id: node.map(str::to_string) }
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
        fn local_identity(&self) -> LocalIdentity {
            self.local.clone()
        }
        fn roster(&self) -> Result<crate::ResolvedRoster, &'static str> {
            if let Some(e) = self.roster_error {
                return Err(e);
            }
            Ok(crate::ResolvedRoster { machines: self.roster.clone(), uid_from_history: self.from_history.iter().cloned().collect() })
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
            let mut card = sample_card();
            card.specs.machine_id = self.local.machine_id.clone();
            card.specs.machine_uid = self.local.machine_uid.clone();
            card
        }
        fn verify_peer(&self, entry: &MachineEntry) -> Result<PeerTarget, UnreachableReason> {
            let verdict = self.verify.lock().unwrap().get(&entry.id).cloned().unwrap_or_else(|| Ok(format!("n-{}", entry.id)));
            verdict.map(|node| {
                let mut t = darkmux_fleet::unverified_target_for_test("http://127.0.0.1:1");
                t.node_id = Some(node);
                t
            })
        }
        fn fetch_card(&self, _target: &PeerTarget, entry: &MachineEntry, known_version: Option<&str>) -> Fetched {
            self.dials.lock().unwrap().push((entry.id.clone(), known_version.map(str::to_string)));
            let (delay, fetched) = self.fetches.lock().unwrap().get(&entry.id).cloned().expect("a scripted fetch");
            std::thread::sleep(Duration::from_millis(delay));
            fetched
        }
    }

    fn dialed(s: &Scripted) -> Vec<String> {
        let mut v: Vec<String> = s.dials.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();
        v.sort();
        v
    }

    fn peer_says(s: &Scripted, id: &str, delay_ms: u64, outcome: CardOutcome, accepts: AcceptsState) {
        s.fetches.lock().unwrap().insert(id.to_string(), (delay_ms, Fetched { outcome, accepts }));
    }

    fn old(v: &str) -> CardOutcome {
        CardOutcome::Unavailable {
            why: UnavailableWhy::NoCardRoute,
            peer_version: Some(v.to_string()),
            peer_version_source: Some(VersionSource::Presence),
        }
    }

    fn row<'a>(view: &'a FleetView, id: &str) -> &'a FleetMachine {
        view.machines.iter().find(|m| m.entry.as_ref().is_some_and(|e| e.id == id)).unwrap_or_else(|| panic!("no {id} row"))
    }

    fn scripted(local: LocalIdentity, roster: Vec<MachineEntry>) -> Scripted {
        Scripted { local, roster, presence_off: true, ..Default::default() }
    }

    // ── every peer is dialed; presence is display ───────────────────────

    /// The promise: presence never gates a dial. A peer with no beat is asked,
    /// and its liveness reads `no_beat`, a fact about Redis and not a verdict on
    /// the machine.
    #[test]
    fn every_peer_is_dialed_whatever_presence_says() {
        let s = Scripted {
            local: identity("laptop", None, Some("nLAPTOP")),
            roster: vec![entry("laptop"), entry("studio"), entry("mini")],
            beats: vec![beat("laptop", "U1", None), beat("mini", "U3", Some("5.0.0"))],
            ..Default::default()
        };
        s.verify.lock().unwrap().insert("laptop".into(), Ok("nLAPTOP".into()));
        peer_says(&s, "studio", 0, old("5.0.0"), AcceptsState::Unknown);
        peer_says(&s, "mini", 0, old("5.0.0"), AcceptsState::Unknown);
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(dialed(&s), vec!["mini".to_string(), "studio".to_string()], "studio has no beat and is still asked");
        assert_eq!(row(&view, "studio").liveness, Liveness::NoBeat);
        assert!(!matches!(row(&view, "studio").card, CardOutcome::Unreachable { .. }), "no beat is not unreachable");
        let mini = row(&view, "mini");
        assert_eq!((mini.liveness, mini.last_beat_ms), (Liveness::Live, Some(1234)));
    }

    /// Presence that is off or unreadable says nothing about a peer: it is
    /// asked and its liveness is `unknown`.
    #[test]
    fn with_no_presence_to_read_every_peer_is_asked_and_liveness_is_unknown() {
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, old("4.9.0"), AcceptsState::Unknown);
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(dialed(&s), vec!["studio".to_string()]);
        assert_eq!(row(&view, "studio").liveness, Liveness::Unknown);
        assert!(matches!(view.presence, SourceState::Off));
    }

    #[test]
    fn a_beat_is_matched_by_hardware_identity_when_the_entry_has_one() {
        let mut studio = entry("studio");
        studio.machine_uid = Some("abc-123".into());
        let s = Scripted {
            local: identity("laptop", None, None),
            roster: vec![studio.clone()],
            // Same NAME, different hardware: not this machine.
            beats: vec![beat("studio", "OTHER", None)],
            ..Default::default()
        };
        peer_says(&s, "studio", 0, old("5.0.0"), AcceptsState::Unknown);
        assert_eq!(gather_view(&s, FLEET_VIEW_CACHE_TTL).machines[1].liveness, Liveness::NoBeat);
        let s2 = Scripted {
            local: identity("laptop", None, None),
            roster: vec![studio],
            beats: vec![beat("renamed", "ABC-123", None)],
            ..Default::default()
        };
        peer_says(&s2, "studio", 0, old("5.0.0"), AcceptsState::Unknown);
        assert_eq!(gather_view(&s2, FLEET_VIEW_CACHE_TTL).machines[1].liveness, Liveness::Live, "uid match, case-insensitive");
    }

    /// The promise: a slow peer does not hold the others up.
    #[test]
    fn peers_are_asked_in_parallel_not_one_after_another() {
        let s = scripted(identity("laptop", None, None), vec![entry("a"), entry("b"), entry("c")]);
        for id in ["a", "b", "c"] {
            peer_says(&s, id, 400, old("5.0.0"), AcceptsState::Unknown);
        }
        let started = std::time::Instant::now();
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let took = started.elapsed();
        assert_eq!(view.machines.len(), 4, "three peers and this machine");
        assert!(took < Duration::from_millis(900), "three 400 ms peers took {took:?}: they were asked in series");
    }

    // ── this machine always has a row ───────────────────────────────────

    /// Who is this machine: the verified node first, then uid, then name, and
    /// evidence that exists and disagrees decides "no".
    #[test]
    fn this_machine_is_recognized_by_node_then_uid_then_name() {
        let with_uid = |uid: &str| MachineEntry { machine_uid: Some(uid.into()), ..entry("alias") };
        let me = identity("MacBook-Pro", Some("UID-1"), Some("nME"));
        // The verified node decides, whatever the name or uid say.
        assert!(is_this_machine(&me, Some("nME"), &with_uid("OTHER")), "same node under another name and uid");
        assert!(!is_this_machine(&me, Some("nSTUDIO"), &MachineEntry { machine_uid: Some("UID-1".into()), ..entry("MacBook-Pro") }), "another node is not this machine, even under this machine's name and uid");
        // No verified node: the uid decides when both have one, case-insensitive.
        assert!(is_this_machine(&me, None, &with_uid("uid-1")));
        assert!(!is_this_machine(&me, None, &MachineEntry { machine_uid: Some("OTHER".into()), ..entry("MacBook-Pro") }), "another uid under this machine's name is not this machine");
        // Nothing but names: the name decides, like flow names do.
        assert!(is_this_machine(&me, None, &entry("macbook-pro")));
        assert!(!is_this_machine(&me, None, &entry("laptop")));
        // A machine that does not know its own node does not guess from the entry's.
        let no_node = identity("MacBook-Pro", None, None);
        assert!(!is_this_machine(&no_node, Some("nME"), &entry("laptop")));
        assert!(is_this_machine(&no_node, Some("nME"), &entry("macbook-pro")), "falls to the name");
    }

    /// The operator's real shape: the roster has an entry named `laptop` and
    /// this machine is `MacBook-Pro`. The entry that verifies to this machine's
    /// node is this machine's row; nothing reads `mismatch`, and the machine is
    /// not dialed.
    #[test]
    fn a_roster_alias_that_verifies_to_this_node_is_this_machines_row_and_is_not_dialed() {
        let s = scripted(identity("MacBook-Pro", Some("UID-1"), Some("nME")), vec![entry("laptop")]);
        s.verify.lock().unwrap().insert("laptop".into(), Ok("nME".into()));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.machines.len(), 1, "one row: the alias folded into this machine's");
        let m = &view.machines[0];
        assert!(m.is_this_machine);
        assert_eq!(m.entry.as_ref().unwrap().id, "laptop");
        assert!(matches!(m.card, CardOutcome::Available { source: CardSource::Local, .. }));
        assert_eq!(m.accepts, AcceptsState::ThisMachine);
        assert!(dialed(&s).is_empty(), "no HTTP for this machine's own row");
        assert_eq!(s.local_cards.load(Ordering::SeqCst), 1);
        assert_eq!(m.liveness, Liveness::Live, "the machine answering is alive");
    }

    /// The roster lacks this machine altogether, or names only an entry that is
    /// not it: the view has this machine's row (entry `null`) AND the unmatched
    /// entry, honestly.
    #[test]
    fn this_machine_has_a_row_even_when_the_roster_lacks_it_and_the_unmatched_entry_stays() {
        let s = scripted(identity("MacBook-Pro", Some("UID-1"), Some("nME")), vec![entry("laptop")]);
        s.verify.lock().unwrap().insert("laptop".into(), Err(UnreachableReason::NotOnOverlay));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.machines.len(), 2, "{:#?}", view.machines.iter().map(|m| (&m.entry, m.is_this_machine)).collect::<Vec<_>>());
        let me = view.machines.iter().find(|m| m.is_this_machine).expect("this machine's row");
        assert!(me.entry.is_none(), "the roster has no entry that is this machine");
        assert_eq!(me.machine_uid.as_deref(), Some("UID-1"));
        assert_eq!(me.uid_source, Some(UidSource::Card));
        let laptop = row(&view, "laptop");
        assert!(!laptop.is_this_machine);
        assert!(matches!(laptop.card, CardOutcome::Unreachable { reason: UnreachableReason::NotOnOverlay, .. }));
    }

    /// An empty roster, and an unreadable one, still have this machine's row.
    #[test]
    fn an_empty_or_unreadable_roster_still_shows_this_machine() {
        let empty = gather_view(&scripted(identity("laptop", None, None), vec![]), FLEET_VIEW_CACHE_TTL);
        assert_eq!(empty.machines.len(), 1);
        assert!(empty.machines[0].is_this_machine && empty.machines[0].entry.is_none());
        let s = Scripted { roster_error: Some("the roster is broken"), local: identity("laptop", None, None), ..Default::default() };
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.roster_error.as_deref(), Some("the roster is broken"));
        assert_eq!(view.machines.len(), 1);
        assert!(view.machines[0].is_this_machine);
    }

    /// With no verified node (the alias is loopback and no node stands behind
    /// it), the uid recognizes this machine: the entry's uid derived from flow
    /// history names this machine's own.
    #[test]
    fn an_alias_no_node_stands_behind_is_this_machine_by_uid_and_says_where_the_uid_came_from() {
        let mut laptop = entry("laptop");
        laptop.machine_uid = Some("uid-1".into());
        let mut s = scripted(identity("MacBook-Pro", Some("UID-1"), Some("nME")), vec![laptop]);
        s.from_history = vec!["laptop".into()];
        s.verify.lock().unwrap().insert("laptop".into(), Err(UnreachableReason::NotOnOverlay));
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(view.machines.len(), 1);
        assert!(view.machines[0].is_this_machine);
        // The local card's own uid wins over the derived one, and says so.
        assert_eq!(view.machines[0].uid_source, Some(UidSource::Card));
    }

    // ── a row says where its uid came from, and when it arrived ─────────

    #[test]
    fn a_rows_uid_is_the_cards_else_the_declared_else_the_flow_history() {
        let mut declared = entry("declared");
        declared.machine_uid = Some("D-UID".into());
        let mut derived = entry("derived");
        derived.machine_uid = Some("H-UID".into());
        let mut s = scripted(identity("laptop", None, None), vec![declared, derived, entry("bare"), entry("carded")]);
        s.from_history = vec!["derived".into()];
        for id in ["declared", "derived", "bare"] {
            peer_says(&s, id, 0, old("5.0.0"), AcceptsState::Unknown);
        }
        let mut card = sample_card();
        card.specs.machine_id = Some("carded".into());
        card.specs.machine_uid = Some("C-UID".into());
        peer_says(&s, "carded", 0, CardOutcome::Available { card: Box::new(card), source: CardSource::Listener }, AcceptsState::NotListed);
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let uid = |id: &str| (row(&view, id).machine_uid.clone(), row(&view, id).uid_source);
        assert_eq!(uid("declared"), (Some("D-UID".into()), Some(UidSource::Declared)));
        assert_eq!(uid("derived"), (Some("H-UID".into()), Some(UidSource::FlowHistory)));
        assert_eq!(uid("bare"), (None, None));
        assert_eq!(uid("carded"), (Some("C-UID".into()), Some(UidSource::Card)));
    }

    /// `received_at_ms` is this machine's clock at the answer, and only when
    /// something answered; every row states what producing it cost.
    #[test]
    fn a_row_says_when_this_machine_got_its_answer_and_what_it_cost() {
        let s = scripted(identity("laptop", None, None), vec![entry("up"), entry("down")]);
        peer_says(&s, "up", 20, old("5.0.0"), AcceptsState::Unknown);
        peer_says(&s, "down", 0, CardOutcome::Unreachable { reason: UnreachableReason::ListenerOff, detail: None }, AcceptsState::Unknown);
        let before = crate::current_millis();
        let view = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        let up = row(&view, "up");
        assert!(up.received_at_ms.is_some_and(|t| t >= before));
        assert!(up.fetch_ms.is_some_and(|ms| ms >= 20), "{:?}", up.fetch_ms);
        assert_eq!(row(&view, "down").received_at_ms, None, "nothing answered");
        assert!(row(&view, "down").fetch_ms.is_some());
    }

    /// The promise: `/fleet/roster` and the fleet view read ONE roster, with the
    /// same flow-history uid for an entry that declares none, and the view's
    /// source says which uids were derived.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_roster_route_and_the_view_share_one_roster_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let roster = tmp.path().join("fleet.json");
        std::fs::write(&roster, r#"{"version":"2","machines":{"studio":{"id":"studio","address":"studio.example.invalid","added_unix_ms":1}}}"#).unwrap();
        let flows = tmp.path().join("flows");
        std::fs::create_dir_all(&flows).unwrap();
        std::fs::write(flows.join("2026-09-30.jsonl"), "{\"machine_id\":\"studio\",\"machine_uid\":\"UID-STUDIO-HISTORY\"}\n").unwrap();
        let set = |k: &str, v: Option<&std::ffi::OsStr>| unsafe {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        };
        let prev = (std::env::var_os("DARKMUX_FLEET_FILE"), std::env::var_os("DARKMUX_FLOWS_DIR"));
        set("DARKMUX_FLEET_FILE", Some(roster.as_os_str()));
        set("DARKMUX_FLOWS_DIR", Some(flows.as_os_str()));
        let provider: Arc<dyn IdentityProvider> = Arc::new(provider(false));
        let from_view = ProcessSources::new(provider, GatheredBy::Daemon, None).roster().map_err(|e| e.to_string());
        let route = get(FleetContext::hermetic(), "/fleet/roster", "127.0.0.1:5555", &[]).await;
        set("DARKMUX_FLEET_FILE", prev.0.as_deref());
        set("DARKMUX_FLOWS_DIR", prev.1.as_deref());
        let from_view = from_view.expect("the roster reads");
        assert_eq!(from_view.machines[0].machine_uid.as_deref(), Some("UID-STUDIO-HISTORY"));
        assert!(from_view.uid_from_history.contains("studio"), "the view's source says the uid was derived");
        assert_eq!(route["machines"][0]["machine_uid"], "UID-STUDIO-HISTORY", "the route shows the same uid: {route}");
    }

    #[test]
    fn the_view_records_the_ttl_it_was_gathered_under() {
        let s = Scripted::default();
        assert_eq!(gather_view(&s, FLEET_VIEW_CACHE_TTL).cache_ttl_ms, 5000);
        assert_eq!(gather_view(&s, Duration::ZERO).cache_ttl_ms, 0);
    }

    // ── who sees what ───────────────────────────────────────────────────

    fn grant() -> CardAccepts {
        CardAccepts {
            peer_name: "laptop".into(),
            profiles: vec!["deep".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        }
    }

    fn card_of(id: &str) -> MachineCard {
        let mut card = sample_card();
        card.specs.machine_id = Some(id.to_string());
        card.specs.machine_uid = None;
        card
    }

    fn available(id: &str) -> CardOutcome {
        CardOutcome::Available { card: Box::new(card_of(id)), source: CardSource::Listener }
    }

    /// A reader outside this machine's operator side is shown neither what
    /// peers let this machine do nor any seat block.
    #[test]
    fn a_view_for_an_outside_reader_withholds_grants_and_seats() {
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, available("studio"), AcceptsState::Granted { accepts: grant() });
        let full = gather_view(&s, FLEET_VIEW_CACHE_TTL);
        assert_eq!(row(&full, "studio").accepts, AcceptsState::Granted { accepts: grant() }, "the operator side sees it");
        let seats = |v: &FleetView, id: &str| match &row(v, id).card {
            CardOutcome::Available { card, .. } => card.seats.is_some(),
            other => panic!("{other:?}"),
        };
        assert!(seats(&full, "studio"));
        let outside = full.for_outside_reader();
        assert_eq!(row(&outside, "studio").accepts, AcceptsState::Withheld);
        assert!(!seats(&outside, "studio"), "a peer's seats are not for an outside reader");
        let me = outside.machines.iter().find(|m| m.is_this_machine).unwrap();
        assert_eq!(me.accepts, AcceptsState::ThisMachine, "this machine's own row states no grant to hide");
        assert!(matches!(&me.card, CardOutcome::Available { card, .. } if card.seats.is_none()));
    }

    // ── which machine a card is of ──────────────────────────────────────

    fn attributed(entry: &MachineEntry, card: MachineCard) -> Fetched {
        attribute(entry, card, CardSource::Listener, CardGrant::Listed { accepts: grant() })
    }

    /// The promise: a card is shown on a row only when it is that machine's.
    /// Another machine answering at the address (a re-used address, a wrong
    /// roster entry) is a mismatch, and its card and its grant are dropped.
    #[test]
    fn a_card_from_another_machine_is_a_mismatch_and_is_not_attributed() {
        let f = attributed(&entry("studio"), card_of("mini"));
        match f.outcome {
            CardOutcome::Mismatch { answered_as } => assert_eq!(answered_as.as_deref(), Some("mini")),
            other => panic!("{other:?}"),
        }
        assert_eq!(f.accepts, AcceptsState::Unknown, "a grant said by another machine is not this entry's");
        assert!(matches!(attributed(&entry("studio"), card_of("Studio")).outcome, CardOutcome::Available { .. }), "names compare like flow names");
    }

    #[test]
    fn a_card_that_names_no_machine_is_not_confirmed_to_be_the_entrys() {
        let mut card = card_of("studio");
        card.specs.machine_id = None;
        assert!(matches!(attributed(&entry("studio"), card).outcome, CardOutcome::Mismatch { answered_as: None }));
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
        assert!(matches!(attributed(&studio, renamed).outcome, CardOutcome::Available { .. }), "same hardware, another name");
        let mut impostor = card_of("studio");
        impostor.specs.machine_uid = Some("OTHER".into());
        assert!(matches!(attributed(&studio, impostor).outcome, CardOutcome::Mismatch { .. }), "same name, other hardware");
    }

    #[test]
    fn a_grant_is_stated_as_a_row_fact_beside_the_card() {
        let of = |grant| attribute(&entry("studio"), card_of("studio"), CardSource::Listener, grant).accepts;
        assert_eq!(of(CardGrant::Listed { accepts: grant() }), AcceptsState::Granted { accepts: grant() });
        assert_eq!(of(CardGrant::NotListed), AcceptsState::NotListed);
        assert_eq!(of(CardGrant::Unknown), AcceptsState::Unknown);
    }

    // ── one peer over real HTTP ─────────────────────────────────────────

    use std::io::Write;

    /// A loopback HTTP server with canned answers by path; any other path is a
    /// 404. Records every request's path and `Authorization` header. Serves a
    /// few requests, then stops.
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

    fn studio() -> MachineEntry {
        MachineEntry { address: "127.0.0.1".to_string(), ..entry("studio") }
    }

    fn listener_body() -> String {
        serde_json::json!({"card": card_of("studio"), "grant": {"state": "listed", "accepts": grant()}}).to_string()
    }

    fn refusal_body(reason: &str) -> String {
        serde_json::json!({"status": "refused", "machine": "studio", "reason": reason}).to_string()
    }

    /// Ask `studio`'s listener (the fake) for its card, through the same
    /// verification a gather runs.
    fn fetch(listener: &FakePeer, known_version: Option<&str>) -> Fetched {
        with_fleet_token(|| {
            let target = listener_target(&provider(true), &studio(), listener.port).map_err(|e| format!("{e:?}")).unwrap();
            fetch_listener_card(&target, &studio(), known_version)
        })
    }

    fn serve_card(status: &'static str, body: String) -> FakePeer {
        FakePeer::serve(vec![(darkmux_fleet::CARD_PATH, status, body)])
    }

    fn reason_of(f: &Fetched) -> UnreachableReason {
        match &f.outcome {
            CardOutcome::Unreachable { reason, .. } => *reason,
            other => panic!("{other:?}"),
        }
    }

    /// The promise: the listener is asked with the fleet token, and its answer
    /// is the card with the grant.
    #[test]
    #[serial_test::serial]
    fn the_listener_is_asked_with_the_token_and_its_card_carries_the_grant() {
        let listener = serve_card("200 OK", listener_body());
        let f = fetch(&listener, None);
        let CardOutcome::Available { card, source } = f.outcome else { panic!("a card") };
        assert_eq!(source, CardSource::Listener);
        assert_eq!(f.accepts, AcceptsState::Granted { accepts: grant() });
        assert_eq!(card.specs.machine_id.as_deref(), Some("studio"));
        assert_eq!(listener.paths(), vec![darkmux_fleet::CARD_PATH.to_string()]);
        assert_eq!(listener.authorization_of(darkmux_fleet::CARD_PATH), Some(format!("Bearer {FLEET_TOKEN}")));
    }

    /// Symmetry: a peer that does not list this machine still gives its card,
    /// and the row says "not listed".
    #[test]
    #[serial_test::serial]
    fn a_peer_that_does_not_list_this_machine_still_gives_its_card() {
        let body = serde_json::json!({"card": card_of("studio"), "grant": {"state": "not_listed"}}).to_string();
        let f = fetch(&serve_card("200 OK", body), None);
        assert!(matches!(f.outcome, CardOutcome::Available { .. }), "{:?}", f.outcome);
        assert_eq!(f.accepts, AcceptsState::NotListed);
    }

    /// The listener's own gate has typed reasons, each with the peer's own
    /// (sanitized, bounded) sentence: 401 is the token, 403 is the caller not
    /// placed on the network, 503 and 429 are a listener that cannot serve now.
    #[test]
    #[serial_test::serial]
    fn the_listeners_gate_answers_have_typed_reasons_with_the_peers_sentence() {
        let cases = [
            ("401 Unauthorized", UnreachableReason::AuthRequired),
            ("403 Forbidden", UnreachableReason::RefusedByPeer),
            ("503 Service Unavailable", UnreachableReason::ListenerUnavailable),
            ("429 Too Many Requests", UnreachableReason::ListenerUnavailable),
            ("500 Internal Server Error", UnreachableReason::BadAnswer),
        ];
        for (status, want) in cases {
            let f = fetch(&serve_card(status, refusal_body("studio says no")), None);
            assert_eq!(reason_of(&f), want, "{status}");
            let CardOutcome::Unreachable { detail, .. } = &f.outcome else { unreachable!() };
            assert_eq!(detail.as_deref(), Some("studio says no"), "{status}");
            assert_eq!(f.accepts, AcceptsState::Unknown, "a gate refusal says nothing about the grant");
        }
    }

    /// A refusal sentence from a listener is sanitized and cut like any other
    /// peer string before it is shown.
    #[test]
    #[serial_test::serial]
    fn a_listeners_refusal_sentence_is_sanitized_and_bounded() {
        let sentence = format!("no\u{1b}[31m {}", "x".repeat(2000));
        let f = fetch(&serve_card("403 Forbidden", refusal_body(&sentence)), None);
        let CardOutcome::Unreachable { detail: Some(reason), .. } = f.outcome else { panic!("unreachable") };
        assert!(!reason.contains('\u{1b}'), "{reason:?}");
        assert!(reason.chars().count() <= PEER_REASON_MAX_CHARS, "{}", reason.chars().count());
    }

    /// With the listener off (nothing on its port) the row is unreachable
    /// with a typed reason: there is no second channel to fall back to.
    #[test]
    #[serial_test::serial]
    fn a_peer_with_its_listener_off_is_unreachable_with_a_typed_reason() {
        let daemon = FakePeer::serve(vec![("/machine/card", "200 OK", serde_json::to_string(&sample_card()).unwrap())]);
        let f = with_fleet_token(|| {
            let target = listener_target(&provider(true), &studio(), closed_port()).map_err(|e| format!("{e:?}")).unwrap();
            fetch_listener_card(&target, &studio(), None)
        });
        assert_eq!(reason_of(&f), UnreachableReason::ListenerOff);
        assert!(daemon.paths().is_empty(), "a daemon is never asked for a card");
    }

    /// A listener with no card route is an older darkmux: unavailable, not an
    /// error, and the version is the presence beat's, said to be so.
    #[test]
    #[serial_test::serial]
    fn a_listener_with_no_card_route_is_an_older_darkmux_with_the_presence_version_marked() {
        let f = fetch(&serve_card("404 Not Found", "{}".into()), Some("4.9.1"));
        match f.outcome {
            CardOutcome::Unavailable { why, peer_version, peer_version_source } => {
                assert_eq!(why, UnavailableWhy::NoCardRoute);
                assert_eq!((peer_version.as_deref(), peer_version_source), (Some("4.9.1"), Some(VersionSource::Presence)));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(fetch(&serve_card("404 Not Found", "{}".into()), None).outcome, CardOutcome::Unavailable { peer_version: None, peer_version_source: None, .. }));
    }

    /// A card of another schema major, and a card of this major that does not
    /// parse, are unavailable for different reasons; the peer's own version
    /// wins over the presence beat's.
    #[test]
    #[serial_test::serial]
    fn a_card_this_darkmux_cannot_read_says_why_and_prefers_the_peers_own_version() {
        let mut v: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        v["card"]["card_schema_version"] = serde_json::json!("2.0");
        v["card"]["specs"]["darkmux_version"] = serde_json::json!("9.9.9");
        let f = fetch(&serve_card("200 OK", v.to_string()), Some("5.0.0"));
        match f.outcome {
            CardOutcome::Unavailable { why, peer_version, peer_version_source } => {
                assert_eq!(why, UnavailableWhy::OtherSchemaMajor);
                assert_eq!((peer_version.as_deref(), peer_version_source), (Some("9.9.9"), Some(VersionSource::Peer)), "the peer's word, not presence's");
            }
            other => panic!("{other:?}"),
        }
        let mut broken: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        broken["card"]["profiles"] = serde_json::json!("not a list");
        match fetch(&serve_card("200 OK", broken.to_string()), None).outcome {
            CardOutcome::Unavailable { why, peer_version_source, .. } => {
                assert_eq!(why, UnavailableWhy::Unparseable, "a writer bug is not 'an older darkmux'");
                assert_eq!(peer_version_source, Some(VersionSource::Peer));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The promise of forward compatibility: a card carrying enum values a
    /// newer darkmux invented (an endpoint kind, a busy policy, a grant state)
    /// still reads as a card, and every other field is shown.
    #[test]
    #[serial_test::serial]
    fn a_card_from_a_newer_darkmux_still_shows_the_rest_of_the_card() {
        let mut v: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        v["card"]["profiles"][0]["endpoint_kind"] = serde_json::json!("fleet");
        v["card"]["seats"]["busy_policy"] = serde_json::json!("preempt");
        v["grant"] = serde_json::json!({"state": "delegated"});
        let f = fetch(&serve_card("200 OK", v.to_string()), None);
        let CardOutcome::Available { card, .. } = f.outcome else { panic!("the card must survive: {:?}", f.outcome) };
        assert_eq!(card.profiles[0].endpoint_kind, crate::machine_card::CardEndpointKind::Unknown);
        assert_eq!(card.profiles[0].name, "deep");
        assert_eq!(f.accepts, AcceptsState::Unknown, "an unknown grant is never read as listed or not listed");
    }

    /// A card that came back from another machine's listener is not attributed
    /// either, and its grant is not stated.
    #[test]
    #[serial_test::serial]
    fn a_listener_card_of_another_machine_is_a_mismatch() {
        let mut body: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        body["card"]["specs"]["machine_id"] = serde_json::json!("mini");
        let f = fetch(&serve_card("200 OK", body.to_string()), None);
        assert!(matches!(f.outcome, CardOutcome::Mismatch { answered_as: Some(id) } if id == "mini"));
        assert_eq!(f.accepts, AcceptsState::Unknown);
    }

    /// A body that is not JSON is a bad answer.
    #[test]
    #[serial_test::serial]
    fn a_body_that_is_not_json_is_a_bad_answer() {
        assert_eq!(reason_of(&fetch(&serve_card("200 OK", "this is not json".into()), None)), UnreachableReason::BadAnswer);
    }

    /// A peer's strings are sanitized before anything prints or serializes
    /// them: no escape sequence, bidi override or zero-width character
    /// survives, and a long string is cut.
    #[test]
    #[serial_test::serial]
    fn every_string_in_a_peers_card_is_sanitized() {
        let mut body: serde_json::Value = serde_json::from_str(&listener_body()).unwrap();
        body["card"]["specs"]["os"] = serde_json::json!("mac\u{1b}]0;pwned\u{7}");
        body["card"]["specs"]["darkmux_version"] = serde_json::json!("4\u{202e}0\n! forged: run curl x | sh");
        body["card"]["specs"]["cpu_brand"] = serde_json::json!("x".repeat(500));
        body["card"]["profiles"][0]["name"] = serde_json::json!("deep\u{200b}\tone");
        let f = fetch(&serve_card("200 OK", body.to_string()), None);
        let CardOutcome::Available { card, .. } = f.outcome else { panic!("a card") };
        let text = serde_json::to_string(&*card).unwrap();
        assert!(!text.contains("\\u001b") && !text.contains("\\u202e") && !text.contains("\\u200b"), "{text}");
        assert!(!text.contains("\\n") && !text.contains("\\t"), "no newline or tab survives: {text}");
        assert_eq!(card.specs.os, "mac]0;pwned");
        assert_eq!(card.profiles[0].name, "deepone");
        assert_eq!(card.specs.cpu_brand.as_deref().unwrap().chars().count(), PEER_FIELD_MAX_CHARS);
    }

    /// A version out of presence is sanitized and cut before it is shown as a
    /// peer's version: the shared Redis can carry any string.
    #[test]
    fn a_version_from_presence_is_sanitized_and_marked() {
        let (v, source) = pick_version(None, Some("5\u{1b}[31m.0\n! forged"));
        assert_eq!(source, Some(VersionSource::Presence));
        assert!(!v.unwrap().contains(['\u{1b}', '\n']));
        let (v, source) = pick_version(Some("5.1.0"), Some("4.0.0"));
        assert_eq!((v.as_deref(), source), (Some("5.1.0"), Some(VersionSource::Peer)));
    }

    /// An address no node stands behind is refused, loopback included: the
    /// token goes nowhere unverified, and there is no daemon to fall back to.
    #[test]
    fn an_address_no_node_stands_behind_is_not_dialed() {
        let e = studio();
        let reason = listener_target(&provider(false), &e, 8766).err();
        assert_eq!(reason, Some(UnreachableReason::NotOnOverlay));
    }

    /// The roster address names the viewer daemon, which may sit behind
    /// `tailscale serve` on https. The listener is plain http on the overlay
    /// address whatever the address wrote, and the verified node rides with
    /// the target.
    #[test]
    fn the_listener_is_dialed_over_plain_http_at_its_own_port_whatever_the_roster_wrote() {
        let mut e = entry("studio");
        e.address = "https://127.0.0.1:9443".into();
        let t = listener_target(&provider(true), &e, 8766).map_err(|e| format!("{e:?}")).unwrap();
        assert_eq!((t.scheme.as_str(), t.port), ("http", 8766));
        assert_eq!(t.node_id.as_deref(), Some("nSTUDIO"));
        assert_eq!(t.pinned_ip, Some("127.0.0.1".parse().unwrap()));
    }

    /// Each way an address can fail to verify has its own reason, so each can
    /// name its own remedy.
    #[test]
    fn an_address_that_cannot_be_verified_says_why() {
        let reason = |provider: darkmux_fleet::StaticIdentityProvider, mut e: MachineEntry, address: &str| {
            e.address = address.into();
            match listener_target(&provider, &e, 8766) {
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

    /// The bound is on the whole request, not on each read: a peer that
    /// trickles its answer byte by byte, each byte inside any per-read
    /// timeout, is still cut off at `PEER_CARD_TIMEOUT`.
    #[test]
    #[serial_test::serial]
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
        let started = std::time::Instant::now();
        let f = with_fleet_token(|| {
            let target = listener_target(&provider(true), &studio(), port).map_err(|e| format!("{e:?}")).unwrap();
            fetch_listener_card(&target, &studio(), None)
        });
        let took = started.elapsed();
        assert!(matches!(f.outcome, CardOutcome::Unreachable { .. }), "{:?}", f.outcome);
        assert!(took < PEER_CARD_TIMEOUT + Duration::from_millis(1500), "a trickling peer held the fetch for {took:?}");
    }

    /// A CLI prints the view its own machine's daemon gathered when one
    /// answers, and gathers one itself only when none does (an older darkmux
    /// with no `/fleet/view` route is no answer either).
    #[test]
    fn a_cli_reads_its_own_daemons_view_and_falls_back_when_none_answers() {
        let s = scripted(identity("laptop", None, None), vec![]);
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

    // ── forward compatibility of the view itself ────────────────────────

    /// The promise: a view from a newer darkmux, with a value of every enum a
    /// row carries that this darkmux has never seen, still parses, and the row
    /// keeps every field it can read.
    #[test]
    fn a_view_from_a_newer_darkmux_still_parses_with_unknown_arms() {
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, available("studio"), AcceptsState::NotListed);
        let mut v = serde_json::to_value(gather_view(&s, FLEET_VIEW_CACHE_TTL)).unwrap();
        v["gathered_by"] = serde_json::json!("hub");
        v["presence"] = serde_json::json!({"state": "delegated"});
        let studio = v["machines"].as_array_mut().unwrap().iter_mut().find(|m| m["entry"]["id"] == "studio").unwrap();
        studio["liveness"] = serde_json::json!("asleep");
        studio["uid_source"] = serde_json::json!("tpm");
        studio["accepts"] = serde_json::json!({"state": "delegated"});
        studio["card"]["source"] = serde_json::json!("relay");
        let template = studio.clone();
        let with_card = |id: &str, card: serde_json::Value| {
            let mut m = template.clone();
            m["entry"]["id"] = serde_json::json!(id);
            m["card"] = card;
            m
        };
        let extra = [
            with_card("other", serde_json::json!({"state": "teleported", "how": "unknown"})),
            with_card("unreachable", serde_json::json!({"state": "unreachable", "reason": "quantum", "detail": null})),
            with_card(
                "unavailable",
                serde_json::json!({"state": "unavailable", "why": "moved", "peer_version": "9.9.9", "peer_version_source": "blockchain"}),
            ),
        ];
        v["machines"].as_array_mut().unwrap().extend(extra);
        let view: FleetView = serde_json::from_value(v).expect("unknown enum values must not discard the view");
        assert_eq!(view.gathered_by, GatheredBy::Unknown);
        assert!(matches!(view.presence, SourceState::Unavailable { .. }), "an unknown presence state reads as unavailable, the cautious reading");
        let studio = row(&view, "studio");
        assert_eq!((studio.liveness, studio.uid_source, &studio.accepts), (Liveness::Unknown, Some(UidSource::Unknown), &AcceptsState::Unknown));
        assert!(matches!(&studio.card, CardOutcome::Available { source: CardSource::Unknown, card } if card.profiles.len() == 2), "the card is intact");
        assert!(matches!(row(&view, "other").card, CardOutcome::Unknown));
        assert!(matches!(row(&view, "unreachable").card, CardOutcome::Unreachable { reason: UnreachableReason::Unknown, .. }));
        assert!(
            matches!(row(&view, "unavailable").card, CardOutcome::Unavailable { why: UnavailableWhy::Unknown, peer_version_source: Some(VersionSource::Unknown), .. }),
            "the version is kept, and its unknown source is not read as the peer's or presence's"
        );
    }

    // ── the routes ──────────────────────────────────────────────────────

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_status(ctx: FleetContext, uri: &str, peer: &str, headers: &[(&str, &str)]) -> (u16, Vec<u8>) {
        let app = crate::build_router_full(std::path::PathBuf::new(), None, None, ctx);
        let mut req = Request::builder().uri(uri).header("host", "localhost");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        let res = app.oneshot(req).await.unwrap();
        (res.status().as_u16(), axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap().to_vec())
    }

    async fn get(ctx: FleetContext, uri: &str, peer: &str, headers: &[(&str, &str)]) -> serde_json::Value {
        let (status, body) = get_status(ctx, uri, peer, headers).await;
        assert_eq!(status, 200, "GET {uri}");
        serde_json::from_slice(&body).unwrap()
    }

    /// The daemon no longer serves a card of its own: the listener is the one
    /// channel, and this machine's own card is its row of `/fleet/view`.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_daemon_serves_no_machine_card_route() {
        let ctx = FleetContext::with_sources(Arc::new(Scripted::default()));
        let (status, _) = get_status(ctx, "/machine/card", "127.0.0.1:5555", &[]).await;
        assert_eq!(status, 404, "the viewer daemon has no card route");
    }

    /// This machine's card is its own row of the view: the same card the
    /// listener serves, with its seats.
    #[tokio::test]
    #[serial_test::serial]
    async fn this_machines_card_is_its_own_row_of_the_view() {
        let s = scripted(identity("laptop", Some("UID-1"), Some("nME")), vec![]);
        let view = get(FleetContext::with_sources(Arc::new(s)), "/fleet/view", "127.0.0.1:5555", &[]).await;
        let me = &view["machines"][0];
        assert_eq!(me["is_this_machine"], true);
        assert_eq!(me["entry"], serde_json::Value::Null);
        assert_eq!(me["card"]["state"], "available");
        assert_eq!(me["card"]["source"], "local");
        assert_eq!(me["card"]["card"]["specs"]["machine_id"], "laptop");
        assert!(me["card"]["card"]["seats"].is_object(), "the operator side sees this machine's seats");
        assert_eq!(me["accepts"]["state"], "this_machine");
    }

    /// The audience of `accepts` and seats is the doctor panel's: this machine
    /// or a holder of the fleet token. A caller that is neither gets neither.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_fleet_view_route_shows_grants_to_local_or_token_readers_only() {
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, available("studio"), AcceptsState::Granted { accepts: grant() });
        let ctx = FleetContext::with_sources(Arc::new(s));
        let studio = |v: &serde_json::Value| v["machines"].as_array().unwrap().iter().find(|m| m["entry"]["id"] == "studio").cloned().unwrap();
        let local = get(ctx.clone(), "/fleet/view", "127.0.0.1:5555", &[]).await;
        assert_eq!(local["cache_ttl_ms"], 5000);
        assert_eq!(studio(&local)["accepts"]["state"], "granted", "this machine's own reader: {local}");
        assert!(studio(&local)["card"]["card"]["seats"].is_object());
        let outsider = get(ctx.clone(), "/fleet/view", "10.0.0.9:5555", &[]).await;
        assert_eq!(studio(&outsider)["accepts"]["state"], "withheld", "{outsider}");
        assert!(studio(&outsider)["card"]["card"].get("seats").is_none(), "{outsider}");
    }

    /// A reader from another machine that presents the fleet token is on the
    /// operator side: the same predicate the doctor panel uses.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_token_holding_reader_from_another_machine_sees_grants() {
        let prev = std::env::var("DARKMUX_SERVE_TOKEN").ok();
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", FLEET_TOKEN) };
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, available("studio"), AcceptsState::Granted { accepts: grant() });
        let ctx = FleetContext::with_sources(Arc::new(s));
        let auth = format!("Bearer {FLEET_TOKEN}");
        let view = get(ctx, "/fleet/view", "10.0.0.9:5555", &[("authorization", &auth)]).await;
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_TOKEN", v),
                None => std::env::remove_var("DARKMUX_SERVE_TOKEN"),
            }
        }
        let studio = view["machines"].as_array().unwrap().iter().find(|m| m["entry"]["id"] == "studio").cloned().unwrap();
        assert_eq!(studio["accepts"]["state"], "granted", "{view}");
    }

    /// What a CLI reads back from a daemon is the same view.
    #[test]
    fn a_view_survives_the_wire_the_cli_reads_it_over() {
        let s = scripted(identity("laptop", None, None), vec![entry("studio")]);
        peer_says(&s, "studio", 0, available("studio"), AcceptsState::NotListed);
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
