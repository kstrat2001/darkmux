//! The machine card: what one machine says about itself, served at
//! `GET /machine/card` and gathered by every other daemon's fleet view
//! ([`crate::fleet_view`]).
//!
//! A card states facts and never infers them. Each block is read from the
//! thing that owns the fact:
//!
//! - identity, hardware, loaded and utility models: [`crate::gather_specs`],
//!   the same gather `/machine/specs` serves (the card embeds its type);
//! - profiles and their endpoint kind: the profile registry, through
//!   `ProfileModel::endpoint_kind`, the classification every dispatch,
//!   residency and doctor path reads;
//! - what this machine accepts from the CALLER: the allow-list entry
//!   [`crate::fleet_listener::Admission`] verified, and only that entry;
//! - seats: the running fleet listener's `SeatBook`;
//! - governor: the host sampler's own reading, plus the battery policy the
//!   operator wrote.
//!
//! **Trust.** `accepts` is present only when the request carried the fleet
//! token from a node the overlay network names and the allow-list lists
//! ([`accepts_for`]). Any other reader gets the card without it. A card is
//! never read from Redis: presence says who is alive, and the peer itself
//! says what it is.

use crate::fleet_listener::Admission;
use crate::wire::MachineSpecsResponse;
use darkmux_crew::power_policy::{self, PowerPolicyConfig, StartDecision};
use darkmux_fleet::{Admitted, SeatSnapshot};
use darkmux_flow::payload::{BatteryCharge, HostSampleNow, ThermalNow};
use darkmux_types::config::BusyPolicy;
use darkmux_types::{EndpointKind, ProfileRegistry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The card's own shape version. Minor for an added field a reader can
/// ignore, major for a rename or retype. A peer whose card is on another
/// major is shown as "card unavailable", never guessed at.
pub const CARD_SCHEMA_VERSION: &str = "1.0";

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

/// Seats a submitted job holds on local models: one per model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardLocalSeats {
    /// Local models a submitted job holds right now.
    pub held_models: Vec<String>,
    /// Managed models in this machine's profiles that no submitted job holds.
    pub free_models: Vec<String>,
}

/// Seats a submitted job holds on hosted endpoints: up to a cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardHostedSeats {
    pub held: u32,
    /// `remote.concurrent_cap`; `null` is unbounded.
    pub cap: Option<u32>,
    /// `cap - held`; `null` when unbounded.
    pub free: Option<u32>,
}

/// The seat book of this machine's fleet listener: what OTHER machines'
/// submitted jobs hold. This machine's own dispatches do not register here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CardSeats {
    /// What a job does when its seat is busy (`fleet.busy_policy`).
    #[serde(deserialize_with = "busy_policy_from_token")]
    pub busy_policy: BusyPolicy,
    pub local: CardLocalSeats,
    pub hosted: CardHostedSeats,
    /// Submitted jobs waiting for a seat.
    pub waiting: u32,
}

/// A config enum is deliberately not `Deserialize` (a bad value in
/// `config.json` must not discard the whole config), so a card read from a
/// peer parses the token through the enum's own table instead.
fn busy_policy_from_token<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BusyPolicy, D::Error> {
    use darkmux_types::config_enum::ConfigEnum;
    let raw = String::deserialize(d)?;
    BusyPolicy::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown busy policy `{raw}`")))
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
    /// be refused. `false` with no battery reading.
    pub refusing_start: bool,
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

/// `GET /machine/card`.
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
    /// What this machine accepts from the caller, present only for a request
    /// that carried the fleet token from a verified, allow-listed node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub accepts: Option<CardAccepts>,
    /// The listener's seats. Absent when this process serves no fleet work:
    /// the listener is off, or the card was built by the CLI, not the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub seats: Option<CardSeats>,
    pub governor: CardGovernor,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
    /// What building this card cost, in milliseconds (the observer stamps its
    /// own cost).
    #[cfg_attr(test, ts(type = "number"))]
    pub gather_ms: u64,
}

impl MachineCard {
    /// The same card without the caller-scoped block: what a reader other
    /// than the verified caller may see.
    pub fn without_accepts(mut self) -> Self {
        self.accepts = None;
        self
    }
}

/// The literal a card carries when the profile registry could not be read.
const REGISTRY_UNREADABLE: &str = "this machine's profile registry could not be read";

fn kind_of(model: &darkmux_types::ProfileModel) -> CardEndpointKind {
    match model.endpoint_kind() {
        Ok(EndpointKind::Managed(_)) => CardEndpointKind::Managed,
        Ok(EndpointKind::Unmanaged) => CardEndpointKind::Unmanaged,
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
                .map(|m| CardModel { id: m.id.clone(), n_ctx: m.n_ctx, endpoint_kind: kind_of(m) })
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

/// The seat block from the listener's book and the profiles it can run.
pub(crate) fn card_seats(policy: BusyPolicy, seats: &SeatSnapshot, profiles: &[CardProfile]) -> CardSeats {
    let managed: BTreeSet<&str> = profiles
        .iter()
        .flat_map(|p| p.models.iter())
        .filter(|m| m.endpoint_kind == CardEndpointKind::Managed)
        .map(|m| m.id.as_str())
        .collect();
    let held: BTreeSet<&str> = seats.local_held.iter().map(String::as_str).collect();
    let held_hosted = seats.hosted_held as u32;
    let cap = seats.hosted_cap.map(|c| c as u32);
    CardSeats {
        busy_policy: policy,
        local: CardLocalSeats {
            held_models: seats.local_held.clone(),
            free_models: managed.difference(&held).map(|m| (*m).to_string()).collect(),
        },
        hosted: CardHostedSeats { held: held_hosted, cap, free: cap.map(|c| c.saturating_sub(held_hosted)) },
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
        charging: b.charging,
        minutes_to_empty: b.minutes_to_empty,
    });
    let refusing_start = matches!(power_policy::start_decision(sample.as_ref(), cfg), StartDecision::Refuse(_));
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

/// This machine's card. Blocking: the specs gather shells out. `accepts` is
/// what the caller verified as (see [`accepts_for`]); `None` builds the card
/// every unverified reader gets.
pub(crate) fn gather_local_card(accepts: Option<CardAccepts>) -> MachineCard {
    let started = std::time::Instant::now();
    let specs = crate::gather_specs();
    let (profiles, default_profile, profiles_error) = match darkmux_profiles::profiles::load_registry(None) {
        Ok(loaded) => {
            let profiles = card_profiles(&loaded.registry);
            let default = loaded
                .registry
                .default_profile
                .clone()
                .filter(|d| profiles.iter().any(|p| &p.name == d));
            (profiles, default, None)
        }
        Err(e) => {
            eprintln!("darkmux serve: machine card: reading the profile registry failed ({e:#})");
            (Vec::new(), None, Some(REGISTRY_UNREADABLE.to_string()))
        }
    };
    let seats = crate::fleet_listener::listener_seats().map(|(policy, snap)| card_seats(policy, &snap, &profiles));
    let governor = card_governor(
        crate::host_sampler::ring().snapshot().map(|l| l.now).as_ref(),
        &PowerPolicyConfig::from_env(),
    );
    MachineCard {
        card_schema_version: CARD_SCHEMA_VERSION.to_string(),
        work_job_schema_version: darkmux_fleet::WORK_JOB_SCHEMA_VERSION.to_string(),
        specs,
        profiles,
        default_profile,
        profiles_error,
        accepts,
        seats,
        governor,
        generated_at_ms: crate::current_millis(),
        gather_ms: started.elapsed().as_millis() as u64,
    }
}

/// The allow-list entry of the caller, when the request is from a verified
/// fleet node: the fleet token, then the connecting node as the overlay
/// network names it, then the allow-list, all through [`Admission::admit`],
/// the listener's own admission path. Any refusal is "no `accepts`", and says
/// nothing about why.
///
/// The peer address is the socket's, never a header: a proxy's
/// `X-Forwarded-For` is not evidence. A loopback caller is this machine, or a
/// proxy in front of it; neither is a node, so the provider is not even asked
/// (the test-only `e2e-fleet-loopback` build is the one place a loopback
/// address stands for a node).
pub(crate) async fn accepts_for(
    admission: &Admission,
    peer: Option<std::net::IpAddr>,
    headers: &axum::http::HeaderMap,
) -> Option<CardAccepts> {
    let peer = peer?;
    if peer.to_canonical().is_loopback() && !crate::fleet_listener::LOOPBACK_FOR_E2E {
        return None;
    }
    admission.admit(peer, headers).await.ok().map(|(admitted, _)| CardAccepts::from(&admitted))
}

#[cfg(test)]
mod tests {
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

    fn snap(local: &[&str], hosted: usize, cap: Option<usize>, waiting: usize) -> SeatSnapshot {
        SeatSnapshot { local_held: local.iter().map(|s| s.to_string()).collect(), hosted_held: hosted, hosted_cap: cap, waiting }
    }

    #[test]
    fn seats_report_held_and_free_per_class_and_the_busy_policy() {
        let cards = card_profiles(&mixed_registry());
        let seats = card_seats(BusyPolicy::Queue, &snap(&["qwen-35b"], 1, Some(3), 2), &cards);
        assert_eq!(seats.busy_policy, BusyPolicy::Queue);
        assert_eq!(seats.local.held_models, vec!["qwen-35b".to_string()]);
        assert_eq!(seats.local.free_models, vec!["qwen-4b".to_string()], "the managed model nobody holds");
        assert_eq!(seats.hosted, CardHostedSeats { held: 1, cap: Some(3), free: Some(2) });
        assert_eq!(seats.waiting, 2);
    }

    #[test]
    fn an_unbounded_hosted_cap_has_no_free_count() {
        let seats = card_seats(BusyPolicy::Refuse, &snap(&[], 4, None, 0), &[]);
        assert_eq!(seats.hosted, CardHostedSeats { held: 4, cap: None, free: None });
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
        BatteryCharge { charge_pct: pct, on_ac: false, charging: false, minutes_to_empty: None }
    }

    fn cfg(refuse: bool) -> PowerPolicyConfig {
        PowerPolicyConfig { min_battery_pct: 50, refuse_start_below_min: refuse, pause_running_below_min: true }
    }

    #[test]
    fn the_governor_states_the_thermal_word_and_the_battery_gate() {
        let low = card_governor(Some(&now(Some("serious"), Some(charge(30)))), &cfg(true));
        assert_eq!(low.thermal.as_ref().unwrap().state, "serious");
        assert_eq!(low.battery.as_ref().unwrap().charge_pct, 30);
        assert!(low.battery_gate.refusing_start, "30% is under a 50% floor the operator turned on");
        assert_eq!((low.battery_gate.floor_pct, low.battery_gate.pause_running_below_min), (50, true));

        assert!(!card_governor(Some(&now(None, Some(charge(30)))), &cfg(false)).battery_gate.refusing_start, "the policy is off");
        assert!(!card_governor(Some(&now(None, Some(charge(50)))), &cfg(true)).battery_gate.refusing_start, "at the floor starts");
    }

    #[test]
    fn a_machine_with_no_battery_or_no_reading_is_never_gated() {
        assert!(!card_governor(Some(&now(Some("nominal"), None)), &cfg(true)).battery_gate.refusing_start);
        let nothing = card_governor(None, &cfg(true));
        assert!(nothing.thermal.is_none() && nothing.battery.is_none());
        assert!(!nothing.battery_gate.refusing_start);
    }

    #[test]
    fn without_accepts_drops_only_the_caller_scoped_block() {
        let card = gather_local_card(Some(CardAccepts {
            peer_name: "laptop".into(),
            profiles: vec!["deep".into()],
            roles: vec![],
            images: vec![],
            workspace: false,
        }));
        assert!(card.accepts.is_some());
        let plain = card.clone().without_accepts();
        assert!(plain.accepts.is_none());
        assert_eq!(plain.card_schema_version, card.card_schema_version);
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("accepts").is_none(), "absent, not null: {json}");
    }
}
