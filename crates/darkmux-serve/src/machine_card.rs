//! The machine card: what one machine says about itself, served by its fleet
//! listener at `GET /fleet/card` and gathered by every other daemon's fleet
//! view ([`crate::fleet_view`]). The listener is the one channel a card
//! travels between machines; a machine reads its OWN card from its own row of
//! `GET /fleet/view`.
//!
//! A card states facts and never infers them. Each block is read from the
//! thing that owns the fact:
//!
//! - identity, hardware, loaded and utility models: [`crate::gather_specs`],
//!   the same gather `/machine/specs` serves (the card embeds its type);
//! - profiles and their endpoint kind: the profile registry, through
//!   `ProfileModel::endpoint_kind`, the classification every dispatch,
//!   residency and doctor path reads;
//! - what this machine accepts from the CALLER: the caller's own allow-list
//!   entry and no other ([`CardGrant`], beside the card in [`ListenerCard`];
//!   the card itself never carries it);
//! - seats: the running fleet listener's `SeatBook`, which counts only jobs
//!   OTHER machines submitted (`counts_own_work` says so, and no field says
//!   "free");
//! - governor: the host sampler's own reading, plus the battery policy the
//!   operator wrote;
//! - fleet role: the position the operator DECLARED (`fleet.mode`), whether
//!   this machine's own `redis.host` reaches this machine, and, on a card
//!   that declares `hub` only, the fleet defaults the hub hands out
//!   ([`CardFleetDefaults`]). Nobody has to ask a machine which role it plays.
//!
//! **Trust.** A card read is AUTHENTICATED, not authorized: the caller holds
//! the fleet token and comes from a node the overlay network names
//! ([`darkmux_fleet::authenticate`]). Whether this machine also LETS that
//! node run work is reported as data in the grant, so a machine that grants
//! nothing is still visible. A card is never read from Redis: presence says
//! who is alive, and the peer itself says what it is.
//!
//! **Forward compatibility.** Every enum a peer's card carries has an
//! `unknown` arm, so a newer machine's card still reads on an older one
//! (`Unknown` is never read as any known value). The shape is tied to
//! [`CARD_SCHEMA_VERSION`] by a golden hash: a shape change without a version
//! bump fails a test.

use crate::wire::MachineSpecsResponse;
use darkmux_crew::power_policy::{self, PowerPolicyConfig, StartDecision};
use darkmux_fleet::{Admitted, SeatSnapshot};
use darkmux_flow::payload::{BatteryCharge, ChargeState, HostSampleNow, ThermalNow};
use darkmux_types::config::{BusyPolicy, DeclaredFleetMode};
use darkmux_types::{EndpointKind, ProfileRegistry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// The card's own shape version. Minor for an added field a reader can
/// ignore, major for a rename or retype. A peer whose card is on another
/// major is shown as "card unavailable", never guessed at.
///
/// 1.2 (#3035): `seats.hosted` is `seats.unmanaged` ("remote" and "hosted"
/// were the wrong axis), and its `cap` is gone. `cap` was the machine's
/// `remote.concurrent_cap`; concurrency is per endpoint now
/// (`endpoints.<id>.limits.concurrent_calls`), so there is no one number to
/// report. 1.2 also adds `serves_radio` and `serves_profiles`, optional facts
/// about what this machine serves to peers (absent when not stated), and
/// `specs.redis_url_redacted` is `specs.hub_configured`: whether a hub is
/// configured, never where it is (#3072). 1.0 and
/// 1.1 are unreleased, so no shipped reader meets a 1.2 card.
pub const CARD_SCHEMA_VERSION: &str = "1.2";

/// What darkmux does at an endpoint (darkmux's own action, never a location
/// or a cost), for a model, and for a profile as the sum of its models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum CardEndpointKind {
    /// darkmux loads and unloads the model.
    Managed,
    /// darkmux only sends requests.
    Unmanaged,
    /// A profile whose models are not all one kind. Never a model's kind.
    Mixed,
    /// The endpoint cannot be classified (it names an id no `endpoints`
    /// entry defines), or a profile declares no model.
    Unresolved,
    /// A kind a newer darkmux states that this one does not know. Never read
    /// as managed or unmanaged.
    #[serde(other)]
    Unknown,
}

/// One model a profile declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardModel {
    pub id: String,
    /// The declared window: the load size on a managed endpoint, a declared
    /// ceiling on an unmanaged one; `null` when none is declared.
    pub n_ctx: Option<u32>,
    pub endpoint_kind: CardEndpointKind,
}

/// One profile in this machine's registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardProfile {
    pub name: String,
    pub description: Option<String>,
    /// Whether this is the registry's `default_profile`.
    pub is_default: bool,
    pub endpoint_kind: CardEndpointKind,
    pub models: Vec<CardModel>,
}

/// What this machine lets the calling peer do: that peer's allow-list entry,
/// and nothing about any other entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardAccepts {
    /// The caller's own name in this machine's allow-list.
    pub peer_name: String,
    /// This machine's profile names the caller may run.
    pub profiles: Vec<String>,
    /// This machine's role ids the caller may dispatch.
    pub roles: Vec<String>,
    /// Docker images the caller may name.
    pub images: Vec<String>,
    /// Whether the caller may name a working directory.
    pub workspace: bool,
}

impl From<&Admitted> for CardAccepts {
    fn from(a: &Admitted) -> Self {
        Self {
            peer_name: a.peer_name.clone(),
            profiles: a.profiles.clone(),
            roles: a.roles.clone(),
            images: a.images.clone(),
            workspace: a.workspace,
        }
    }
}

/// What the answering machine lets the CALLER do, as the machine's own
/// allow-list says. It rides beside the card, never inside it, so a card
/// stays the same document whoever asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CardGrant {
    /// The caller's node has an entry: this is that entry, and no other.
    Listed { accepts: CardAccepts },
    /// The caller is a verified fleet node with no entry: this machine takes
    /// no work from it. The card is still served.
    NotListed,
    /// This machine could not say (its allow-list could not be read, or names
    /// the caller twice), or a state this darkmux does not know. Never read as
    /// listed or as not listed.
    #[serde(other)]
    Unknown,
}

/// `GET /fleet/card` on the fleet listener: this machine's card and what it
/// lets the authenticated caller do. The two travel together because the
/// listener that authenticated the caller is where the grant is read.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ListenerCard {
    pub card: MachineCard,
    pub grant: CardGrant,
}

impl CardGrant {
    /// The grant an authorization result states: an entry, no entry, or "could
    /// not say". A refusal that is not about the allow-list is not a grant
    /// decision, so it is `Unknown`, never `NotListed`.
    pub fn from_authorization(result: &Result<Admitted, darkmux_fleet::Refusal>) -> Self {
        match result {
            Ok(admitted) => CardGrant::Listed { accepts: CardAccepts::from(admitted) },
            Err(darkmux_fleet::Refusal::NotAllowed { .. }) => CardGrant::NotListed,
            Err(_) => CardGrant::Unknown,
        }
    }
}

/// What a job another machine submitted does to this machine's seats.
///
/// **Only those jobs are counted.** This machine's own dispatches do not pass
/// through the fleet listener, so `counts_own_work` is `false` and NOTHING in
/// this block means "free": a model with `held_by_peer_job: false` may be
/// running this machine's own coder turn, and an endpoint's held count may be
/// nearer its `limits.concurrent_calls` once own work counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardSeats {
    /// What a job does when its seat is busy (`fleet.busy_policy`).
    pub busy_policy: CardBusyPolicy,
    /// Whether this machine's own dispatches are counted in the seats below.
    /// `false` today.
    pub counts_own_work: bool,
    /// The local models a peer's job could seat, or does: each managed model
    /// in a work profile, and each model a peer job holds.
    pub local: Vec<CardLocalSeat>,
    /// Seats on endpoints darkmux does not manage.
    pub unmanaged: CardUnmanagedSeats,
    /// Submitted jobs waiting for a seat.
    pub waiting: u32,
}

/// What `fleet.busy_policy` says a card reader may rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum CardBusyPolicy {
    /// A job whose seat is busy is refused at once.
    Refuse,
    /// A job whose seat is busy waits for it.
    Queue,
    /// A policy a newer darkmux has that this one does not know.
    #[serde(other)]
    Unknown,
}

impl From<BusyPolicy> for CardBusyPolicy {
    fn from(p: BusyPolicy) -> Self {
        match p {
            BusyPolicy::Refuse => CardBusyPolicy::Refuse,
            BusyPolicy::Queue => CardBusyPolicy::Queue,
        }
    }
}

/// One local model a peer's job could seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardLocalSeat {
    pub model: String,
    /// A job another machine submitted holds this model now.
    pub held_by_peer_job: bool,
}

/// Seats peers' jobs hold on endpoints this machine only sends requests to
/// (each endpoint's `limits.concurrent_calls` bounds its own).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardUnmanagedSeats {
    /// Jobs other machines submitted that run now on an unmanaged endpoint.
    pub held_by_peer_jobs: u32,
}

/// The battery policy the operator wrote, and whether it refuses a start now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardBatteryGate {
    /// `power.min_battery_pct`.
    pub floor_pct: u8,
    /// `power.refuse_start_below_min`.
    pub refuse_start_below_min: bool,
    /// `power.pause_running_below_min`.
    pub pause_running_below_min: bool,
    /// The start decision at the current reading: `true` when a new run would
    /// be refused, `false` when it would not. `null` when no battery reading
    /// was observed: no host sampler ran in the process that built the card,
    /// or the probe reported no battery. The probe cannot tell a machine with
    /// no battery from a failed read, so a desktop publishes `null`, never
    /// a confident `false`.
    pub refusing_start: Option<bool>,
}

/// What the host reports and the battery policy in force. The thermal state
/// is the operating system's word, verbatim. Runs that a governor has paused
/// are per-run state (each run's pace file), so the card does not claim them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardGovernor {
    /// `null` until the host sampler has a reading.
    pub thermal: Option<ThermalNow>,
    /// `null` on a machine with no battery, and until the sampler has a reading.
    pub battery: Option<BatteryCharge>,
    pub battery_gate: CardBatteryGate,
}

/// The shape version of [`CardFleetDefaults`]. A reader that meets another
/// value uses none of the block: a default it cannot read is never guessed at.
pub const FLEET_DEFAULTS_VERSION: u32 = 1;

/// What a fleet hub hands to the machines that have no setting of their own.
/// Present on a card only when its `fleet_mode` is `hub` (the writer adds it
/// only then, and every reader takes it only from such a card through
/// [`MachineCard::hub_defaults`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardFleetDefaults {
    /// [`FLEET_DEFAULTS_VERSION`] when written.
    pub version: u32,
    pub radio: CardRadioDefaults,
}

/// The hub's default for radio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardRadioDefaults {
    /// A `<profile>@<machine>` address; `null` when the hub states none.
    pub answerer_profile: Option<String>,
}

/// One machine's card: what it says about itself. Served inside a
/// [`ListenerCard`] by the fleet listener.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineCard {
    pub card_schema_version: String,
    pub work_job_schema_version: String,
    /// Identity, hardware, loaded models and the utility model: the same
    /// document `/machine/specs` serves.
    pub specs: MachineSpecsResponse,
    pub profiles: Vec<CardProfile>,
    /// The registry's `default_profile`, when it names one of `profiles`.
    pub default_profile: Option<String>,
    /// Why `profiles` is empty when the registry could not be read; `null`
    /// otherwise. A fixed sentence, never the underlying error (it carries a
    /// path).
    pub profiles_error: Option<String>,
    /// The listener's seats. Absent when this process serves no fleet work:
    /// the listener is off, or the card was built by the CLI, not the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub seats: Option<CardSeats>,
    pub governor: CardGovernor,
    /// The fleet position this machine declares (`fleet.mode`). `unknown` is a
    /// machine whose own setting is not a registered value, or a value a
    /// newer darkmux states. Absent on a card from a darkmux that predates the
    /// field (card schema 1.0): that machine states no role, and none is
    /// guessed for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub fleet_mode: Option<DeclaredFleetMode>,
    /// Whether this machine's own `redis.host` reaches this machine (loopback,
    /// or its own overlay node): it runs the Redis its records and presence
    /// go to. `false` when Redis is off here or points at another machine;
    /// absent on a card that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub hosts_fleet_redis: Option<bool>,
    /// Whether this machine serves radio: its `fleet.accept_work` grants the
    /// `radio-host` role to at least one peer. A fact about this machine alone,
    /// so it reads the same from every machine that views this card; it never
    /// names who is granted (that is a relationship, which the card does not
    /// carry). Absent on a card that predates the field (schema 1.1), or when
    /// the allow-list could not be read: not stated, never guessed `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub serves_radio: Option<bool>,
    /// How many distinct profiles this machine serves to peers: the profiles
    /// in its registry that its `fleet.accept_work` grants to at least one
    /// peer, leaving out a profile that runs only the utility model (admission
    /// refuses it). A fact about this machine alone, so it reads the same from
    /// every machine that views this card; it never names a profile's grantee.
    /// Absent on a card that predates the field (schema 1.1), or when the
    /// allow-list or the registry could not be read: not stated, never 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub serves_profiles: Option<u32>,
    /// The fleet defaults this machine hands out. Absent unless `fleet_mode`
    /// is `hub`; read only through [`MachineCard::hub_defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub fleet_defaults: Option<CardFleetDefaults>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
    /// What building this card cost, in milliseconds (the observer stamps its
    /// own cost).
    #[cfg_attr(test, ts(type = "number"))]
    pub gather_ms: u64,
    /// How long the serving machine keeps a gathered card before gathering
    /// again: this card may be that much older than `generated_at_ms` says
    /// when it was read. `0` for a card built for one reader.
    #[cfg_attr(test, ts(type = "number"))]
    pub cache_ttl_ms: u64,
}

impl MachineCard {
    /// The fleet defaults this card may hand out: present, written in a shape
    /// this darkmux reads, and on a card that declares `hub`. The one place
    /// that decides it, so a card that is not the hub's never sets a default
    /// on any machine, whatever else it carries.
    pub fn hub_defaults(&self) -> Option<&CardFleetDefaults> {
        (self.fleet_mode == Some(DeclaredFleetMode::Hub))
            .then_some(self.fleet_defaults.as_ref())
            .flatten()
            .filter(|d| d.version == FLEET_DEFAULTS_VERSION)
    }
}

/// The literal a card carries when the profile registry could not be read.
const REGISTRY_UNREADABLE: &str = "this machine's profile registry could not be read";

/// A model's endpoint kind, resolved the way every dispatch resolves its
/// target ([`darkmux_crew::target::target_for`]): a target that cannot be
/// built (an undefined endpoint id, a bad URL, an unknown dialect) is
/// `Unresolved`, never a guess at managed or unmanaged.
fn kind_of(profile_name: &str, profile: &darkmux_types::Profile, model: &darkmux_types::ProfileModel) -> CardEndpointKind {
    match darkmux_crew::target::target_for(profile_name.to_string(), profile.clone(), model.clone()) {
        Ok(target) => match target.kind {
            EndpointKind::Managed(_) => CardEndpointKind::Managed,
            EndpointKind::Unmanaged => CardEndpointKind::Unmanaged,
        },
        Err(_) => CardEndpointKind::Unresolved,
    }
}

/// A profile's kind: the one kind all its models share, `Mixed` when they
/// differ, `Unresolved` when it declares none.
fn profile_kind(models: &[CardModel]) -> CardEndpointKind {
    let mut kinds = models.iter().map(|m| m.endpoint_kind);
    let Some(first) = kinds.next() else { return CardEndpointKind::Unresolved };
    if kinds.all(|k| k == first) {
        first
    } else {
        CardEndpointKind::Mixed
    }
}

/// The registry's profiles as a card states them. A quarantined profile is
/// absent from the registry, so it is absent here.
pub(crate) fn card_profiles(registry: &ProfileRegistry) -> Vec<CardProfile> {
    registry
        .profiles
        .iter()
        .map(|(name, profile)| {
            let models: Vec<CardModel> = profile
                .models
                .iter()
                .map(|m| CardModel { id: m.id.clone(), n_ctx: m.n_ctx, endpoint_kind: kind_of(name, profile, m) })
                .collect();
            CardProfile {
                name: name.clone(),
                description: profile.description.clone(),
                is_default: registry.default_profile.as_deref() == Some(name.as_str()),
                endpoint_kind: profile_kind(&models),
                models,
            }
        })
        .collect()
}

/// A profile whose every model is the utility model: the listener refuses it
/// (utility work is never taken from another machine), so it seats nothing.
fn is_utility_only(profile: &CardProfile, utility: Option<&str>) -> bool {
    utility.is_some() && !profile.models.is_empty() && profile.models.iter().all(|m| Some(m.id.as_str()) == utility)
}

/// The seat block from the listener's book and the profiles it can run.
/// `utility` is the registry's utility model id: a profile of only that model
/// is not one a peer can seat, so its model is not listed unless a peer job
/// holds it or a work profile also names it.
pub(crate) fn card_seats(
    policy: BusyPolicy,
    seats: &SeatSnapshot,
    profiles: &[CardProfile],
    utility: Option<&str>,
) -> CardSeats {
    let mut local: BTreeMap<&str, bool> = profiles
        .iter()
        .filter(|p| !is_utility_only(p, utility))
        .flat_map(|p| p.models.iter())
        .filter(|m| m.endpoint_kind == CardEndpointKind::Managed)
        .map(|m| (m.id.as_str(), false))
        .collect();
    for held in &seats.local_held {
        local.insert(held.as_str(), true);
    }
    CardSeats {
        busy_policy: policy.into(),
        counts_own_work: false,
        local: local.into_iter().map(|(model, held)| CardLocalSeat { model: model.to_string(), held_by_peer_job: held }).collect(),
        unmanaged: CardUnmanagedSeats { held_by_peer_jobs: seats.unmanaged_held as u32 },
        waiting: seats.waiting as u32,
    }
}

/// The governor block from the host sampler's reading and the operator's
/// battery policy. The start decision is `power_policy::start_decision`, the
/// one the launch pre-flight runs.
pub(crate) fn card_governor(now: Option<&HostSampleNow>, cfg: &PowerPolicyConfig) -> CardGovernor {
    let battery = now.and_then(|n| n.battery.clone());
    let sample = battery.as_ref().map(|b| darkmux_crew::host_probe::BatterySample {
        charge_pct: b.charge_pct,
        on_ac: b.on_ac,
        charging: b.state == ChargeState::Charging,
        state: b.state,
        minutes_to_empty: b.minutes_to_empty,
    });
    // (#3074) When there is no battery sample (or probe failed), publish
    // refusing_start as None so viewers render unknown rather than a confident false.
    let refusing_start = sample
        .as_ref()
        .map(|s| matches!(power_policy::start_decision(Some(s), cfg), StartDecision::Refuse(_)));
    CardGovernor {
        thermal: now.and_then(|n| n.thermal.clone()),
        battery,
        battery_gate: CardBatteryGate {
            floor_pct: cfg.min_battery_pct,
            refuse_start_below_min: cfg.refuse_start_below_min,
            pause_running_below_min: cfg.pause_running_below_min,
            refusing_start,
        },
    }
}

/// The role a peer is granted to have this machine answer its radio questions.
const RADIO_HOST_ROLE: &str = "radio-host";

/// Whether any peer in the allow-list may dispatch the `radio-host` role here.
fn serves_radio(list: &std::collections::BTreeMap<String, darkmux_types::config::AcceptWorkEntry>) -> bool {
    list.values().any(|e| e.roles.as_ref().is_some_and(|r| r.iter().any(|role| role == RADIO_HOST_ROLE)))
}

/// How many distinct profiles of `registry` the allow-list grants to at least
/// one peer. A name the registry does not define, and a profile that runs only
/// the utility model (admission refuses both), do not count.
fn serves_profiles(
    list: &std::collections::BTreeMap<String, darkmux_types::config::AcceptWorkEntry>,
    registry: &[CardProfile],
    utility: Option<&str>,
) -> u32 {
    let granted: std::collections::BTreeSet<&str> =
        list.values().flat_map(|e| e.profiles.iter().flatten()).map(String::as_str).collect();
    let n = registry.iter().filter(|p| granted.contains(p.name.as_str()) && !is_utility_only(p, utility)).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Whether the Redis this machine is configured to use is on this machine:
/// Redis is on here, and its host is loopback or this machine's own node.
/// Pure over its inputs so the decision is testable without a provider.
fn hosts_fleet_redis(
    redis_enabled: bool,
    redis_host: Option<&str>,
    local_node: impl FnOnce() -> Option<darkmux_fleet::NodeIdentity>,
) -> bool {
    redis_enabled && redis_host.is_some_and(|host| darkmux_fleet::host_reaches_this_machine(host, local_node))
}

/// The defaults this machine hands out, stated only by a hub.
fn card_fleet_defaults(mode: DeclaredFleetMode) -> Option<CardFleetDefaults> {
    (mode == DeclaredFleetMode::Hub).then(|| CardFleetDefaults {
        version: FLEET_DEFAULTS_VERSION,
        radio: CardRadioDefaults { answerer_profile: darkmux_types::config_access::fleet_defaults_radio_answerer_profile() },
    })
}

/// The work wire version a card states: the one this build speaks (#3028).
/// The gathered card and the fixture builder both take it from here.
pub(crate) fn stated_work_version() -> String {
    darkmux_fleet::WORK_JOB_SCHEMA_VERSION.to_string()
}

/// This machine's card. Blocking: the specs gather shells out.
pub(crate) fn gather_local_card() -> MachineCard {
    let started = std::time::Instant::now();
    let specs = crate::gather_specs();
    let (profiles, default_profile, utility, profiles_error) = match darkmux_profiles::profiles::load_registry(None) {
        Ok(loaded) => {
            let profiles = card_profiles(&loaded.registry);
            let default = loaded
                .registry
                .default_profile
                .clone()
                .filter(|d| profiles.iter().any(|p| &p.name == d));
            let utility = loaded.registry.utility_model_id().map(str::to_string);
            (profiles, default, utility, None)
        }
        Err(e) => {
            // Informational: the card itself carries the failure (`REGISTRY_UNREADABLE`), and a
            // CLI read of the fleet view (`profile list --machine <x>`) gathers this card on
            // the way. Printed for a log (non-terminal stderr, the daemon) or `--verbose`, quiet
            // in a terminal.
            darkmux_types::diag_eprintln!("darkmux serve: machine card: reading the profile registry failed ({e:#})");
            (Vec::new(), None, None, Some(REGISTRY_UNREADABLE.to_string()))
        }
    };
    let seats = crate::fleet_listener::listener_seats()
        .map(|(policy, snap)| card_seats(policy, &snap, &profiles, utility.as_deref()));
    let governor = card_governor(
        crate::host_sampler::ring().snapshot().map(|l| l.now).as_ref(),
        &PowerPolicyConfig::from_env(),
    );
    let fleet_mode = darkmux_types::config_access::declared_fleet_mode();
    let allow_list = darkmux_fleet::read_user_allow_list().ok();
    let serves_profiles = allow_list
        .as_ref()
        .filter(|_| profiles_error.is_none())
        .map(|list| serves_profiles(list, &profiles, utility.as_deref()));
    MachineCard {
        card_schema_version: CARD_SCHEMA_VERSION.to_string(),
        work_job_schema_version: stated_work_version(),
        specs,
        profiles,
        default_profile,
        profiles_error,
        seats,
        governor,
        fleet_mode: Some(fleet_mode),
        hosts_fleet_redis: Some(hosts_fleet_redis(
            darkmux_types::config_access::redis_enabled(),
            darkmux_types::config_access::redis_host().as_deref(),
            || darkmux_fleet::configured_provider().ok().and_then(|p| p.local_node().ok()),
        )),
        serves_radio: allow_list.as_ref().map(serves_radio),
        serves_profiles,
        fleet_defaults: card_fleet_defaults(fleet_mode),
        generated_at_ms: crate::current_millis(),
        gather_ms: started.elapsed().as_millis() as u64,
        cache_ttl_ms: 0,
    }
}

/// How long a serving machine keeps its gathered card. Recorded on the card
/// (`cache_ttl_ms`), never adaptive.
pub(crate) const CARD_CACHE_TTL: Duration = Duration::from_secs(2);

/// The gathered card, kept for a TTL. A card read costs a specs gather (a
/// `lms ps` spawn) on the observed machine, and every peer's view reads it
/// about every five seconds, so serving from a cache keeps the observer from
/// joining the observed. Single-flight: the lock is held across the gather,
/// so a burst on a cold cache costs one. Wall-clock time, so a daemon that
/// slept does not serve a pre-sleep card as fresh.
pub(crate) struct CardCache {
    ttl: Duration,
    slot: std::sync::Mutex<Option<(std::time::SystemTime, MachineCard)>>,
}

impl CardCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self { ttl, slot: std::sync::Mutex::new(None) }
    }

    /// The cached card, or the result of `gather` stamped with this cache's
    /// TTL. Blocking.
    pub(crate) fn get(&self, gather: impl FnOnce() -> MachineCard) -> MachineCard {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, card)) = slot.as_ref() {
            if crate::wall_clock_cache_is_fresh(*at, std::time::SystemTime::now(), self.ttl) {
                return card.clone();
            }
        }
        let mut card = gather();
        card.cache_ttl_ms = self.ttl.as_millis() as u64;
        *slot = Some((std::time::SystemTime::now(), card.clone()));
        card
    }
}

/// The one card cache of a daemon process: the fleet listener serves from it
/// and this machine's own row of the fleet view reads it.
pub(crate) fn process_card_cache() -> std::sync::Arc<CardCache> {
    static CACHE: std::sync::OnceLock<std::sync::Arc<CardCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Arc::new(CardCache::new(CARD_CACHE_TTL))).clone()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use darkmux_types::{Profile, ProfileModel};

    fn registry(json: serde_json::Value) -> ProfileRegistry {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(&path, json.to_string()).unwrap();
        darkmux_profiles::profiles::load_registry(Some(path.to_str().unwrap())).unwrap().registry
    }

    fn mixed_registry() -> ProfileRegistry {
        registry(serde_json::json!({
            "profiles": {
                "local": {"description": "on this machine", "models": [{"id": "qwen-35b", "n_ctx": 65536}]},
                "cloud": {"models": [{"id": "gpt-x", "n_ctx": 32000, "endpoint": "azure"}]},
                "both": {"models": [{"id": "qwen-4b", "n_ctx": 8000}, {"id": "gpt-y", "endpoint": "azure"}]}
            },
            "endpoints": {"azure": {"url": "https://example.invalid/v1"}},
            "default_profile": "local"
        }))
    }

    fn by_name<'a>(p: &'a [CardProfile], n: &str) -> &'a CardProfile {
        p.iter().find(|x| x.name == n).unwrap_or_else(|| panic!("no profile {n} in {p:?}"))
    }

    /// The promise: a card's endpoint kinds ARE the registry's resolution.
    /// Cross-checked against the resolver every dispatch uses
    /// (`target_for`), not against the function the card calls.
    #[test]
    fn endpoint_kinds_match_the_registry_resolution() {
        let reg = mixed_registry();
        let cards = card_profiles(&reg);
        assert_eq!(by_name(&cards, "local").endpoint_kind, CardEndpointKind::Managed);
        assert_eq!(by_name(&cards, "cloud").endpoint_kind, CardEndpointKind::Unmanaged);
        for (name, profile) in &reg.profiles {
            for model in &profile.models {
                let target = darkmux_crew::target::target_for(name.clone(), profile.clone(), model.clone()).unwrap();
                let card = by_name(&cards, name).models.iter().find(|m| m.id == model.id).unwrap();
                let expected = match target.kind {
                    EndpointKind::Managed(_) => CardEndpointKind::Managed,
                    EndpointKind::Unmanaged => CardEndpointKind::Unmanaged,
                };
                assert_eq!(card.endpoint_kind, expected, "{name}/{}", model.id);
                assert_eq!(card.n_ctx, model.n_ctx);
            }
        }
    }

    #[test]
    fn a_profile_of_both_kinds_is_mixed_and_one_with_no_models_is_unresolved() {
        let cards = card_profiles(&mixed_registry());
        assert_eq!(by_name(&cards, "both").endpoint_kind, CardEndpointKind::Mixed);
        let mut reg = ProfileRegistry::default();
        reg.profiles.insert("empty".into(), Profile::default());
        assert_eq!(card_profiles(&reg)[0].endpoint_kind, CardEndpointKind::Unresolved);
    }

    #[test]
    fn a_model_on_an_endpoint_no_entry_defines_is_unresolved_never_guessed() {
        let model = ProfileModel {
            id: "m".into(),
            endpoint: Some(darkmux_types::ModelEndpoint::reference("nowhere")),
            ..Default::default()
        };
        let mut reg = ProfileRegistry::default();
        reg.profiles.insert("p".into(), Profile { models: vec![model], ..Default::default() });
        let cards = card_profiles(&reg);
        assert_eq!(cards[0].models[0].endpoint_kind, CardEndpointKind::Unresolved);
        assert_eq!(cards[0].endpoint_kind, CardEndpointKind::Unresolved);
    }

    #[test]
    fn the_default_profile_is_marked() {
        let cards = card_profiles(&mixed_registry());
        assert!(by_name(&cards, "local").is_default);
        assert!(!by_name(&cards, "cloud").is_default);
    }

    fn snap(local: &[&str], unmanaged: usize, waiting: usize) -> SeatSnapshot {
        SeatSnapshot { local_held: local.iter().map(|s| s.to_string()).collect(), unmanaged_held: unmanaged, waiting }
    }

    fn seat_of<'a>(seats: &'a CardSeats, model: &str) -> &'a CardLocalSeat {
        seats.local.iter().find(|s| s.model == model).unwrap_or_else(|| panic!("no seat for {model} in {seats:?}"))
    }

    /// The promise: the seat block states what peers' jobs hold and NOTHING
    /// that reads as "free". A model no peer job holds is `held_by_peer_job:
    /// false`, and the block says it does not count this machine's own work.
    #[test]
    fn seats_state_what_peer_jobs_hold_and_say_they_do_not_count_own_work() {
        let cards = card_profiles(&mixed_registry());
        let seats = card_seats(BusyPolicy::Queue, &snap(&["qwen-35b"], 1, 2), &cards, None);
        assert_eq!(seats.busy_policy, CardBusyPolicy::Queue);
        assert!(!seats.counts_own_work, "the listener never sees this machine's own dispatches");
        assert!(seat_of(&seats, "qwen-35b").held_by_peer_job);
        assert!(!seat_of(&seats, "qwen-4b").held_by_peer_job, "the managed model nobody holds");
        assert_eq!(seats.local.len(), 2);
        assert_eq!(seats.unmanaged, CardUnmanagedSeats { held_by_peer_jobs: 1 });
        assert_eq!(seats.waiting, 2);
        let wire = serde_json::to_string(&seats).unwrap();
        assert!(!wire.contains("free"), "no seat field may read as free: {wire}");
    }

    /// A profile of only the utility model is refused by the listener, so its
    /// model is not a seat; the same model inside a work profile still is.
    #[test]
    fn the_utility_model_is_not_a_seat() {
        let cards = card_profiles(&mixed_registry());
        let models = |utility| {
            card_seats(BusyPolicy::Refuse, &snap(&[], 0, 0), &cards, utility)
                .local
                .into_iter()
                .map(|s| s.model)
                .collect::<Vec<_>>()
        };
        assert_eq!(models(None), vec!["qwen-35b".to_string(), "qwen-4b".to_string()]);
        assert_eq!(models(Some("qwen-4b")), vec!["qwen-35b".to_string(), "qwen-4b".to_string()], "qwen-4b is also in `both`, a work profile");
        assert_eq!(models(Some("qwen-35b")), vec!["qwen-4b".to_string()], "`local` holds only the utility model");
    }

    /// A model a peer job holds is listed even when no work profile names it.
    #[test]
    fn a_held_model_is_listed_whatever_the_profiles_say() {
        let seats = card_seats(BusyPolicy::Refuse, &snap(&["orphan"], 0, 0), &[], None);
        assert_eq!(seats.local, vec![CardLocalSeat { model: "orphan".into(), held_by_peer_job: true }]);
    }

    #[test]
    fn held_endpoint_jobs_count() {
        let seats = card_seats(BusyPolicy::Refuse, &snap(&[], 4, 0), &[], None);
        assert_eq!(seats.unmanaged, CardUnmanagedSeats { held_by_peer_jobs: 4 });
    }

    fn now(thermal: Option<&str>, battery: Option<BatteryCharge>) -> HostSampleNow {
        HostSampleNow {
            sampled_at_ms: 1,
            sampler_cost_ms: 1,
            cpu_pct: None,
            cpu_clusters: None,
            mem_pct: None,
            gpu_pct: None,
            gpu_mhz: None,
            gpu_mem_bytes: None,
            thermal: thermal.map(|s| ThermalNow { state: s.into(), cpu_speed_limit_pct: 90 }),
            battery,
            power_mw: None,
        }
    }

    fn charge(pct: u8) -> BatteryCharge {
        BatteryCharge { charge_pct: pct, on_ac: false, state: ChargeState::Discharging, minutes_to_empty: None }
    }

    fn cfg(refuse: bool) -> PowerPolicyConfig {
        PowerPolicyConfig { min_battery_pct: 50, refuse_start_below_min: refuse, pause_running_below_min: true }
    }

    #[test]
    fn the_governor_states_the_thermal_word_and_the_battery_gate() {
        let low = card_governor(Some(&now(Some("serious"), Some(charge(30)))), &cfg(true));
        assert_eq!(low.thermal.as_ref().unwrap().state, "serious");
        assert_eq!(low.battery.as_ref().unwrap().charge_pct, 30);
        assert_eq!(low.battery_gate.refusing_start, Some(true), "30% is under a 50% floor the operator turned on");
        assert_eq!((low.battery_gate.floor_pct, low.battery_gate.pause_running_below_min), (50, true));

        assert_eq!(card_governor(Some(&now(None, Some(charge(30)))), &cfg(false)).battery_gate.refusing_start, Some(false), "the policy is off");
        assert_eq!(card_governor(Some(&now(None, Some(charge(50)))), &cfg(true)).battery_gate.refusing_start, Some(false), "at the floor starts");
    }

    /// (#3074) A machine with no battery sample (or probe failed) has no
    /// measurement, so refusing_start is `None` (`null`), not a confident `false`.
    #[test]
    fn a_gate_nobody_measured_is_not_observed_rather_than_false() {
        assert_eq!(card_governor(Some(&now(Some("nominal"), None)), &cfg(true)).battery_gate.refusing_start, None);
        let nothing = card_governor(None, &cfg(true));
        assert!(nothing.thermal.is_none() && nothing.battery.is_none());
        assert_eq!(nothing.battery_gate.refusing_start, None);
        assert!(serde_json::to_value(&nothing).unwrap()["battery_gate"]["refusing_start"].is_null());
    }

    /// A profile whose dispatch target cannot be built is `unresolved`, by the
    /// same resolution every dispatch runs: an unknown dialect, not only an
    /// undefined endpoint id.
    #[test]
    fn a_target_that_cannot_be_built_is_unresolved_whatever_the_reason() {
        let reg = registry(serde_json::json!({
            "profiles": {
                "baddialect": {"models": [{"id": "b", "endpoint": "odd"}]},
                "fine": {"models": [{"id": "c", "endpoint": "ok"}]}
            },
            "endpoints": {
                "odd": {"url": "https://example.invalid/v1", "dialect": "no-such-dialect"},
                "ok": {"url": "https://example.invalid/v1"}
            }
        }));
        let cards = card_profiles(&reg);
        for (name, profile) in &reg.profiles {
            for model in &profile.models {
                let built = darkmux_crew::target::target_for(name.clone(), profile.clone(), model.clone()).is_ok();
                let kind = by_name(&cards, name).models[0].endpoint_kind;
                assert_eq!(kind == CardEndpointKind::Unresolved, !built, "{name}: the card and target_for must agree");
            }
        }
        assert_eq!(by_name(&cards, "fine").endpoint_kind, CardEndpointKind::Unmanaged);
        assert_eq!(by_name(&cards, "baddialect").endpoint_kind, CardEndpointKind::Unresolved);
    }

    /// The card states no grant itself: the grant rides beside it in the
    /// listener's answer, so a card is the same document whoever asks.
    #[test]
    fn the_cards_own_shape_has_no_grant_and_the_listeners_answer_carries_it() {
        let card = sample_card();
        assert!(serde_json::to_value(&card).unwrap().get("grant").is_none());
        let accepts = CardAccepts { peer_name: "laptop".into(), profiles: vec!["deep".into()], roles: vec![], images: vec![], workspace: false };
        let json = serde_json::to_value(ListenerCard { card, grant: CardGrant::Listed { accepts } }).unwrap();
        assert_eq!(json["grant"]["state"], "listed");
        assert_eq!(json["grant"]["accepts"]["peer_name"], "laptop");
        assert!(json["card"].get("grant").is_none() && json["card"].get("accepts").is_none());
    }

    fn admitted() -> Admitted {
        Admitted { peer_name: "laptop".into(), node_id: "nLAPTOP".into(), profiles: vec!["deep".into()], roles: vec![], images: vec![], workspace: false }
    }

    /// The grant is what the allow-list said: an entry, no entry, or "could
    /// not say". A refusal that is not about the allow-list is never "not
    /// listed".
    #[test]
    fn a_grant_is_listed_not_listed_or_unknown_and_never_guessed() {
        use darkmux_fleet::Refusal;
        assert!(matches!(CardGrant::from_authorization(&Ok(admitted())), CardGrant::Listed { accepts } if accepts.peer_name == "laptop"));
        assert_eq!(CardGrant::from_authorization(&Err(Refusal::NotAllowed { node_name: "phone".into(), ask: Default::default() })), CardGrant::NotListed);
        for other in [
            Refusal::AmbiguousEntry { names: vec!["a".into(), "b".into()] },
            Refusal::BadRequest("the allow-list cannot be read".into()),
        ] {
            assert_eq!(CardGrant::from_authorization(&Err(other.clone())), CardGrant::Unknown, "{other:?}");
        }
    }

    // ── forward compatibility ──────────────────────────────────────────

    /// A card carrying enum values a newer darkmux invented still parses, and
    /// the rest of the card is intact: each unknown value reads as `Unknown`.
    #[test]
    fn a_card_with_values_from_a_newer_darkmux_still_parses() {
        let mut v = serde_json::to_value(sample_card()).unwrap();
        v["profiles"][0]["endpoint_kind"] = serde_json::json!("fleet");
        v["profiles"][0]["models"][0]["endpoint_kind"] = serde_json::json!("fleet");
        v["seats"]["busy_policy"] = serde_json::json!("preempt");
        let card: MachineCard = serde_json::from_value(v).expect("an unknown enum value must not discard the card");
        assert_eq!(card.profiles[0].endpoint_kind, CardEndpointKind::Unknown);
        assert_eq!(card.profiles[0].models[0].endpoint_kind, CardEndpointKind::Unknown);
        assert_eq!(card.seats.as_ref().unwrap().busy_policy, CardBusyPolicy::Unknown);
        assert_eq!(card.profiles[0].name, "deep", "the rest of the card is intact");
        assert_eq!(card.specs.machine_id.as_deref(), Some("studio"));
        let grant: CardGrant = serde_json::from_value(serde_json::json!({"state": "delegated", "to": "x"})).unwrap();
        assert_eq!(grant, CardGrant::Unknown);
    }

    // ── the card cache ─────────────────────────────────────────────────

    /// The promise: a burst of reads inside the TTL costs one gather; a read
    /// after it gathers again; the TTL is recorded on the card.
    #[test]
    fn a_card_is_gathered_once_per_ttl_and_says_so() {
        let gathers = std::sync::atomic::AtomicUsize::new(0);
        let gather = || {
            gathers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sample_card()
        };
        let cache = CardCache::new(Duration::from_secs(60));
        let first = cache.get(gather);
        let second = cache.get(gather);
        assert_eq!(gathers.load(std::sync::atomic::Ordering::SeqCst), 1, "the second read came from the cache");
        assert_eq!((first.cache_ttl_ms, second.cache_ttl_ms), (60_000, 60_000));
        let expired = CardCache::new(Duration::ZERO);
        expired.get(gather);
        expired.get(gather);
        assert_eq!(gathers.load(std::sync::atomic::Ordering::SeqCst), 3, "a zero TTL never serves a cached card");
    }

    // ── the declared role (#3022) ──────────────────────────────────────

    fn card_declaring(mode: DeclaredFleetMode, defaults: Option<CardFleetDefaults>) -> MachineCard {
        MachineCard { fleet_mode: Some(mode), fleet_defaults: defaults, ..sample_card() }
    }

    fn defaults_with(version: u32, profile: Option<&str>) -> CardFleetDefaults {
        CardFleetDefaults { version, radio: CardRadioDefaults { answerer_profile: profile.map(str::to_string) } }
    }

    /// The promise: a card's defaults are taken only from a card that declares
    /// `hub`, in a shape this darkmux reads. A peer, a standalone machine, an
    /// `unknown` mode or another block version that carries the same block
    /// sets nothing.
    #[test]
    fn defaults_are_taken_only_from_a_hub_card_in_a_known_shape() {
        let hub = card_declaring(DeclaredFleetMode::Hub, Some(defaults_with(FLEET_DEFAULTS_VERSION, Some("deep@studio"))));
        assert_eq!(hub.hub_defaults().unwrap().radio.answerer_profile.as_deref(), Some("deep@studio"));
        for mode in [DeclaredFleetMode::Peer, DeclaredFleetMode::Standalone, DeclaredFleetMode::Unknown] {
            let card = card_declaring(mode, Some(defaults_with(FLEET_DEFAULTS_VERSION, Some("deep@studio"))));
            assert!(card.hub_defaults().is_none(), "a {mode:?} card's defaults are refused");
        }
        let newer = card_declaring(DeclaredFleetMode::Hub, Some(defaults_with(FLEET_DEFAULTS_VERSION + 1, Some("x@y"))));
        assert!(newer.hub_defaults().is_none(), "a block version this darkmux does not read is not guessed at");
        assert!(card_declaring(DeclaredFleetMode::Hub, None).hub_defaults().is_none());
    }

    /// A card from a newer darkmux with a mode this one does not name reads as
    /// `unknown`, never as a known mode.
    #[test]
    fn a_mode_this_darkmux_does_not_know_reads_as_unknown() {
        let mut v = serde_json::to_value(sample_card()).unwrap();
        v["fleet_mode"] = serde_json::json!("regional_hub");
        let card: MachineCard = serde_json::from_value(v).unwrap();
        assert_eq!(card.fleet_mode, Some(DeclaredFleetMode::Unknown));
        assert!(card.hub_defaults().is_none());
    }

    /// Only a hub states defaults, and what it states is its own config.
    #[serial_test::serial]
    #[test]
    fn only_a_hub_states_defaults_and_they_are_its_config() {
        let cfg = |mode: &str| darkmux_types::config::DarkmuxConfig {
            fleet: Some(darkmux_types::config::FleetConfig {
                mode: Some(mode.into()),
                defaults: Some(darkmux_types::config::FleetDefaultsConfig {
                    radio: Some(darkmux_types::config::FleetDefaultsRadioConfig {
                        answerer_profile: Some("deep@studio".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let card_for = |mode: &str| {
            let _g = darkmux_types::config_access::set_config_for_test(cfg(mode));
            card_fleet_defaults(darkmux_types::config_access::declared_fleet_mode())
        };
        let hub = card_for("hub").expect("a hub states its defaults");
        assert_eq!((hub.version, hub.radio.answerer_profile.as_deref()), (FLEET_DEFAULTS_VERSION, Some("deep@studio")));
        for mode in ["peer", "standalone", "hubb"] {
            assert!(card_for(mode).is_none(), "`{mode}` states no defaults even with the block set");
        }
    }

    /// A machine hosts the fleet's Redis only when Redis is on here and its
    /// host is this machine.
    #[test]
    fn a_machine_hosts_the_fleet_redis_only_when_its_redis_host_is_itself() {
        let me = || Some(darkmux_fleet::test_node("n1", "hub", "100.64.0.1"));
        assert!(hosts_fleet_redis(true, Some("127.0.0.1"), me));
        assert!(hosts_fleet_redis(true, Some("hub.tailnet-example.ts.net"), me));
        assert!(!hosts_fleet_redis(true, Some("100.64.0.9"), me), "another machine's address");
        assert!(!hosts_fleet_redis(false, Some("127.0.0.1"), me), "Redis off here");
        assert!(!hosts_fleet_redis(true, None, me), "no host");
    }

    // ── the frozen shape ───────────────────────────────────────────────

    /// A deterministic card built by hand, so a fixture of it is the
    /// writer's own output and does not move with the machine running the test.
    pub(crate) fn sample_card() -> MachineCard {
        MachineCard {
            card_schema_version: CARD_SCHEMA_VERSION.to_string(),
            work_job_schema_version: stated_work_version(),
            specs: MachineSpecsResponse {
                darkmux_version: "5.0.0".into(),
                flow_schema_version: "2.0.0".into(),
                machine_id: Some("studio".into()),
                machine_uid: Some("STUDIO-UID".into()),
                os: "macos aarch64".into(),
                ram_total_bytes: Some(128 * 1024 * 1024 * 1024),
                ram_free_for_ai_bytes: Some(64 * 1024 * 1024 * 1024),
                cpu_brand: Some("Apple M5 Max".into()),
                loaded_models: vec![darkmux_types::LoadedModel {
                    identifier: "darkmux:qwen".into(),
                    model: "qwen".into(),
                    status: "idle".into(),
                    size: "20 GB".into(),
                    context: 65536,
                    queued: Some(0),
                }],
                lms_unreachable: false,
                utility_model: None,
                hub_configured: false,
                generated_at_ms: 1_000,
            },
            profiles: vec![
                CardProfile {
                    name: "deep".into(),
                    description: Some("the big local model".into()),
                    is_default: true,
                    endpoint_kind: CardEndpointKind::Managed,
                    models: vec![CardModel { id: "qwen".into(), n_ctx: Some(65536), endpoint_kind: CardEndpointKind::Managed }],
                },
                CardProfile {
                    name: "cloud".into(),
                    description: None,
                    is_default: false,
                    endpoint_kind: CardEndpointKind::Unmanaged,
                    models: vec![CardModel { id: "gpt".into(), n_ctx: None, endpoint_kind: CardEndpointKind::Unmanaged }],
                },
            ],
            default_profile: Some("deep".into()),
            profiles_error: None,
            seats: Some(CardSeats {
                busy_policy: CardBusyPolicy::Queue,
                counts_own_work: false,
                local: vec![CardLocalSeat { model: "qwen".into(), held_by_peer_job: true }],
                unmanaged: CardUnmanagedSeats { held_by_peer_jobs: 1 },
                waiting: 0,
            }),
            governor: CardGovernor {
                thermal: Some(ThermalNow { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
                battery: None,
                battery_gate: CardBatteryGate {
                    floor_pct: 20,
                    refuse_start_below_min: false,
                    pause_running_below_min: false,
                    refusing_start: Some(false),
                },
            },
            fleet_mode: Some(DeclaredFleetMode::Hub),
            hosts_fleet_redis: Some(true),
            serves_radio: Some(true),
            serves_profiles: Some(2),
            fleet_defaults: Some(CardFleetDefaults {
                version: FLEET_DEFAULTS_VERSION,
                radio: CardRadioDefaults { answerer_profile: Some("deep@studio".into()) },
            }),
            generated_at_ms: 2_000,
            gather_ms: 3,
            cache_ttl_ms: 2_000,
        }
    }

    fn fixtures_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// Write-or-assert a fixture: the writer's output for [`sample_card`] and
    /// its listener wrapper is committed, so a card of schema 1.0 keeps
    /// reading for as long as the major holds.
    fn fixture(name: &str, value: &impl Serialize) -> serde_json::Value {
        let path = fixtures_dir().join(name);
        let written = serde_json::to_value(value).unwrap();
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            std::fs::create_dir_all(fixtures_dir()).unwrap();
            std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(value).unwrap())).unwrap();
            return written;
        }
        let on_disk: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("missing fixture {name} ({e}): regenerate with DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p darkmux-serve machine_card")
        }))
        .unwrap();
        assert_eq!(
            on_disk, written,
            "the card's writer no longer produces the committed {name}. A change to what a card says is a change to \
             CARD_SCHEMA_VERSION: bump it (minor for an added field a reader can ignore, major for a rename or \
             retype), then regenerate with DARKMUX_REGENERATE_FIXTURES=1"
        );
        on_disk
    }

    /// (#3028) The card states the work wire version this build speaks, which
    /// is what a sender reads to know what a peer takes.
    #[test]
    fn a_card_states_the_work_wire_version_this_build_speaks() {
        assert_eq!(stated_work_version(), darkmux_fleet::WORK_JOB_SCHEMA_VERSION);
        assert_eq!(sample_card().work_job_schema_version, darkmux_fleet::WORK_JOB_SCHEMA_VERSION);
    }

    /// The current version's fixtures parse as themselves, through the parser
    /// peers use.
    #[test]
    fn the_current_card_fixtures_still_parse() {
        let card = fixture(&format!("machine-card-{CARD_SCHEMA_VERSION}.json"), &sample_card());
        let card: MachineCard = serde_json::from_value(card).expect("the current card fixture parses");
        assert_eq!(card.card_schema_version, CARD_SCHEMA_VERSION);
        let listed = ListenerCard { card: sample_card(), grant: CardGrant::Listed { accepts: CardAccepts::from(&admitted()) } };
        let listener = fixture(&format!("listener-card-{CARD_SCHEMA_VERSION}.json"), &listed);
        let listener: ListenerCard = serde_json::from_value(listener).expect("the current listener fixture parses");
        assert!(matches!(listener.grant, CardGrant::Listed { .. }));
        assert_eq!(listener.card.profiles.len(), 2);
    }

    // ── serves_radio (card schema 1.2) ────────────────────────────────

    fn entry(roles: Option<Vec<&str>>) -> darkmux_types::config::AcceptWorkEntry {
        darkmux_types::config::AcceptWorkEntry {
            roles: roles.map(|r| r.into_iter().map(String::from).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn a_machine_serves_radio_when_any_peer_may_dispatch_radio_host() {
        let mut list = std::collections::BTreeMap::new();
        assert!(!serves_radio(&list), "an empty allow-list serves no radio");
        list.insert("a".to_string(), entry(None));
        list.insert("b".to_string(), entry(Some(vec!["coder"])));
        assert!(!serves_radio(&list), "other roles and no roles do not count");
        list.insert("c".to_string(), entry(Some(vec!["coder", "radio-host"])));
        assert!(serves_radio(&list), "one peer granted radio-host is enough");
    }

    /// The card says THAT the machine serves radio, never to whom.
    #[test]
    fn the_card_carries_the_flag_and_no_peer_names() {
        let card = MachineCard { serves_radio: Some(true), ..sample_card() };
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["serves_radio"], serde_json::json!(true));
        let absent = serde_json::to_value(MachineCard { serves_radio: None, ..sample_card() }).unwrap();
        assert!(absent.get("serves_radio").is_none(), "not stated is absent, not false");
    }

    // ── serves_profiles (card schema 1.2) ─────────────────────────────

    fn granting(profiles: Vec<&str>) -> darkmux_types::config::AcceptWorkEntry {
        darkmux_types::config::AcceptWorkEntry {
            profiles: Some(profiles.into_iter().map(String::from).collect()),
            ..Default::default()
        }
    }

    fn registry_of(models: &[(&str, &str)]) -> Vec<CardProfile> {
        models
            .iter()
            .map(|(name, model)| CardProfile {
                name: name.to_string(),
                description: None,
                is_default: false,
                endpoint_kind: CardEndpointKind::Managed,
                models: vec![CardModel { id: model.to_string(), n_ctx: None, endpoint_kind: CardEndpointKind::Managed }],
            })
            .collect()
    }

    #[test]
    fn serves_profiles_counts_distinct_granted_work_class_profiles() {
        let registry = registry_of(&[("fast", "qwen-4b"), ("deep", "qwen-35b"), ("tiny", "util"), ("unused", "qwen-35b")]);
        let mut list = std::collections::BTreeMap::new();
        assert_eq!(serves_profiles(&list, &registry, Some("util")), 0, "an empty allow-list serves none");
        list.insert("a".to_string(), granting(vec!["fast", "deep"]));
        list.insert("b".to_string(), granting(vec!["fast", "ghost", "tiny"]));
        list.insert("c".to_string(), entry(None));
        assert_eq!(
            serves_profiles(&list, &registry, Some("util")),
            2,
            "fast is granted twice but counts once; ghost is not in the registry; tiny runs only the utility model"
        );
        assert_eq!(serves_profiles(&list, &registry, None), 3, "with no utility model, tiny is ordinary work");
    }

    #[test]
    fn serves_profiles_is_absent_when_not_stated_and_names_nobody() {
        let absent = serde_json::to_value(MachineCard { serves_profiles: None, ..sample_card() }).unwrap();
        assert!(absent.get("serves_profiles").is_none(), "not stated is absent, never 0");
        let json = serde_json::to_value(MachineCard { serves_profiles: Some(3), ..sample_card() }).unwrap();
        assert_eq!(json["serves_profiles"], serde_json::json!(3));
    }

    /// A schema with its object keys sorted, so its hash does not move with
    /// map ordering.
    fn canonical(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (_, child) in &mut entries {
                    canonical(child);
                }
                map.extend(entries);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(canonical),
            _ => {}
        }
    }

    /// The hash of the listener card's JSON Schema: the card and everything a
    /// reader can meet in it.
    fn shape_hash() -> String {
        let mut schema = serde_json::to_value(schemars::schema_for!(ListenerCard)).unwrap();
        canonical(&mut schema);
        blake3::hash(schema.to_string().as_bytes()).to_hex().to_string()
    }

    /// The promise: the card's shape and [`CARD_SCHEMA_VERSION`] move
    /// together. `tests/fixtures/card-shape.golden` holds one `<version>
    /// <hash>` line per released version; the line for the current version
    /// must match the shape. Change the shape without bumping the version
    /// and the hash disagrees; bump the version and the line is missing.
    #[test]
    fn the_cards_shape_is_tied_to_its_schema_version() {
        let path = fixtures_dir().join("card-shape.golden");
        let line = format!("{CARD_SCHEMA_VERSION} {}", shape_hash());
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            let mut lines: Vec<String> = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.starts_with(&format!("{CARD_SCHEMA_VERSION} ")))
                .map(str::to_string)
                .collect();
            lines.push(line);
            std::fs::create_dir_all(fixtures_dir()).unwrap();
            std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
            return;
        }
        let golden = std::fs::read_to_string(&path).expect("card-shape.golden is missing");
        let recorded = golden.lines().find(|l| l.starts_with(&format!("{CARD_SCHEMA_VERSION} ")));
        assert_eq!(
            recorded,
            Some(line.as_str()),
            "the card's shape and CARD_SCHEMA_VERSION ({CARD_SCHEMA_VERSION}) disagree. If the shape changed, bump \
             CARD_SCHEMA_VERSION (minor for an added field a reader can ignore, major for a rename or retype) and \
             regenerate with DARKMUX_REGENERATE_FIXTURES=1; a line already committed for a released version is never \
             edited."
        );
    }
}
