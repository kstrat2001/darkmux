//! The fleet hub and what it hands out (#3022).
//!
//! One machine declares `fleet.mode hub`. It runs the Redis the fleet's
//! records and presence use, and it holds the fleet defaults: settings a
//! machine takes when it has none of its own. The defaults travel ONLY in the
//! hub's machine card, read from the hub's row of the fleet view
//! ([`crate::machine_list::local_fleet_view`], the gatherer every fleet reader
//! here uses), never from Redis: Redis is writable by anything that reaches it,
//! and a default like radio's answering seat decides where full grounding goes.
//!
//! Each machine keeps the last copy it read under the darkmux home, with the
//! time it read it, and uses that copy only when the hub cannot be asked.
//!
//! This module holds the two decisions every reader shares, so the viewer's
//! HUB badge, `darkmux doctor` and the defaults resolution cannot disagree:
//! which machine is the hub ([`FleetView::declared_hubs`]) and what it says
//! ([`resolve_hub_default`]).

use darkmux_doctor::{Check, Status};
use darkmux_serve::fleet_view::{CardOutcome, DeclaredHubs, FleetMachine, FleetView};
use darkmux_serve::machine_card::CardFleetDefaults;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The last-known copy, under the darkmux home.
const LAST_KNOWN_FILE: &str = "fleet-defaults.json";

/// How long one resolution is reused in this process. One radio answer
/// resolves its seat three times (grounding scope, busy check, dispatch); the
/// daemon's own view is cached for about this long, so a second look inside it
/// learns nothing.
const MEMO_TTL: Duration = Duration::from_secs(5);

/// What the hub said the last time it was read, as kept on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LastKnown {
    /// The hub's name in the fleet view, for display.
    hub: String,
    /// When this machine read it, on this machine's clock.
    seen_at_ms: u64,
    defaults: CardFleetDefaults,
}

/// Where a resolved hub default was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HubDefaultOrigin {
    /// The hub answered in the view just read.
    Live,
    /// The hub could not be asked: the copy saved on this machine.
    LastKnown,
}

/// The hub's defaults as this machine resolves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HubDefaultSeen {
    pub hub: String,
    pub seen_at_ms: u64,
    pub origin: HubDefaultOrigin,
    pub defaults: CardFleetDefaults,
}

impl HubDefaultSeen {
    /// The hub's default answering seat, `<profile>@<machine>`; `None` when
    /// the hub states none.
    pub(crate) fn answerer_profile(&self) -> Option<&str> {
        self.defaults.radio.answerer_profile.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// "how long ago this machine read it", one unit.
    pub(crate) fn age_words(&self, now_ms: u64) -> String {
        crate::mission_status::format_age_span(now_ms.saturating_sub(self.seen_at_ms) / 1000)
    }
}

/// What the view says about the hub's defaults.
enum HubRead {
    /// The hub answered, and states these defaults.
    Stated(LastKnown),
    /// A hub could be among the machines that did not answer: the saved copy
    /// is the best evidence there is.
    Silent,
    /// Every card was read and there is no single hub, or the one hub's card
    /// carries no defaults this darkmux reads: nothing to fall back to.
    Absent,
}

fn machine_name(view: &FleetView, m: &FleetMachine) -> String {
    crate::machine_list::row_name(view, m)
}

fn read_hub(view: &FleetView, now_ms: u64) -> HubRead {
    match view.declared_hubs() {
        DeclaredHubs::One(m) => match &m.card {
            CardOutcome::Available { card, .. } => card.hub_defaults().map_or(HubRead::Absent, |d| {
                HubRead::Stated(LastKnown {
                    hub: machine_name(view, m),
                    seen_at_ms: m.received_at_ms.unwrap_or(now_ms),
                    defaults: d.clone(),
                })
            }),
            _ => HubRead::Silent,
        },
        DeclaredHubs::Several(_) => HubRead::Absent,
        DeclaredHubs::None => {
            if view.machines.iter().all(|m| m.declared_mode().is_some()) {
                HubRead::Absent
            } else {
                HubRead::Silent
            }
        }
    }
}

fn read_last_known(path: &Path) -> Option<LastKnown> {
    let known: LastKnown = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (known.defaults.version == darkmux_serve::machine_card::FLEET_DEFAULTS_VERSION).then_some(known)
}

/// Best effort: a home that cannot be written leaves the next read with the
/// live hub or nothing, as before.
fn write_last_known(path: &Path, known: &LastKnown) {
    let Ok(bytes) = serde_json::to_vec_pretty(known) else { return };
    let tmp = path.with_extension("json.tmp");
    let written = path.parent().map(std::fs::create_dir_all).transpose().is_ok()
        && std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).is_ok();
    if !written {
        eprintln!("darkmux: could not save the fleet defaults to {}; they are read from the hub each time", path.display());
    }
}

/// The hub's defaults for `view`: what the one machine declaring `hub` states
/// now (and is saved at `cache`), else, when a hub could not be asked, the
/// copy last saved. `None` when there is no single hub to read or none of its
/// defaults are usable. Defaults are never taken from a card that does not
/// declare `hub` ([`darkmux_serve::machine_card::MachineCard::hub_defaults`]).
pub(crate) fn resolve_hub_default(view: &FleetView, cache: &Path, now_ms: u64) -> Option<HubDefaultSeen> {
    match read_hub(view, now_ms) {
        HubRead::Stated(known) => {
            write_last_known(cache, &known);
            Some(seen(known, HubDefaultOrigin::Live))
        }
        HubRead::Silent => read_last_known(cache).map(|known| seen(known, HubDefaultOrigin::LastKnown)),
        HubRead::Absent => None,
    }
}

fn seen(known: LastKnown, origin: HubDefaultOrigin) -> HubDefaultSeen {
    HubDefaultSeen { hub: known.hub, seen_at_ms: known.seen_at_ms, origin, defaults: known.defaults }
}

fn last_known_path() -> PathBuf {
    darkmux_types::paths::user_root_guarded().join(LAST_KNOWN_FILE)
}

fn now_ms() -> u64 {
    darkmux_flow::presence::now_ms()
}

type Memo = Mutex<Option<(Instant, Option<HubDefaultSeen>)>>;
static MEMO: Memo = Mutex::new(None);

/// The hub's defaults as this machine resolves them now, from the fleet view
/// its own gatherer produces. Reused for [`MEMO_TTL`].
pub(crate) fn hub_default() -> Option<HubDefaultSeen> {
    let mut memo = MEMO.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, value)) = memo.as_ref() {
        if at.elapsed() < MEMO_TTL {
            return value.clone();
        }
    }
    let value = resolve_hub_default(&crate::machine_list::local_fleet_view(), &last_known_path(), now_ms());
    *memo = Some((Instant::now(), value.clone()));
    value
}

// ── doctor ─────────────────────────────────────────────────────────────

fn check(name: &str, status: Status, message: String, hint: Option<&str>) -> Check {
    Check { name: name.into(), status, message, hint: hint.map(str::to_string) }
}

fn names(view: &FleetView, machines: &[&FleetMachine]) -> String {
    machines.iter().map(|m| machine_name(view, m)).collect::<Vec<_>>().join(", ")
}

/// Whether `redis_host` (this machine's `redis.host`) points at the machine
/// whose roster address is `hub_address`: the same host, or a name and an
/// address that resolve to one another.
fn redis_points_at(redis_host: &str, hub_address: &str, resolve: impl Fn(&str) -> Vec<std::net::IpAddr>) -> bool {
    if darkmux_fleet::address_host(hub_address).is_some_and(|h| darkmux_fleet::same_machine(&h, redis_host)) {
        return true;
    }
    let hub_ips = resolve(hub_address);
    resolve(redis_host).iter().any(|ip| hub_ips.contains(ip))
}

/// The "fleet hub" row: exactly one machine in the view declares `hub`, and it
/// is the machine this machine's Redis is. `redis_host` is this machine's own
/// `redis.host` (`None` when Redis is off here).
pub(crate) fn fleet_hub_check(
    view: &FleetView,
    redis_host: Option<&str>,
    resolve: impl Fn(&str) -> Vec<std::net::IpAddr>,
) -> Check {
    const NAME: &str = "fleet hub";
    match view.declared_hubs() {
        DeclaredHubs::Several(hubs) => check(
            NAME,
            Status::Fail,
            format!("{} machines declare `fleet.mode hub` ({}); exactly one may", hubs.len(), names(view, &hubs)),
            Some("On every machine but the one that runs the fleet's Redis: `darkmux config set fleet.mode peer`."),
        ),
        DeclaredHubs::None => {
            let unread = view.machines.iter().filter(|m| m.declared_mode().is_none()).count();
            if view.machines.len() <= 1 {
                return check(NAME, Status::Pass, "no fleet: this is the only machine in the view".into(), None);
            }
            check(
                NAME,
                Status::Warn,
                format!(
                    "no machine declares `fleet.mode hub` ({unread} of {} cards could not be read)",
                    view.machines.len()
                ),
                Some("On the machine that runs the fleet's Redis: `darkmux config set fleet.mode hub`."),
            )
        }
        DeclaredHubs::One(hub) => hub_row(view, hub, redis_host, resolve),
    }
}

fn hub_row(
    view: &FleetView,
    hub: &FleetMachine,
    redis_host: Option<&str>,
    resolve: impl Fn(&str) -> Vec<std::net::IpAddr>,
) -> Check {
    const NAME: &str = "fleet hub";
    let hub_name = machine_name(view, hub);
    let hosts = match &hub.card {
        CardOutcome::Available { card, .. } => card.hosts_fleet_redis == Some(true),
        _ => false,
    };
    if !hosts {
        return check(
            NAME,
            Status::Fail,
            format!("{hub_name} declares `hub` but its own `redis.host` is not itself, or Redis is off there"),
            Some("On the hub: `darkmux config set redis.enabled true` and `redis.host` to 127.0.0.1 or its own address."),
        );
    }
    if hub.is_this_machine {
        return check(NAME, Status::Pass, format!("{hub_name} (this machine) is the hub and hosts the fleet's Redis"), None);
    }
    let Some(host) = redis_host else {
        return check(NAME, Status::Pass, format!("{hub_name} is the hub; Redis is off on this machine"), None);
    };
    let points_at_hub = hub.entry.as_ref().is_some_and(|e| redis_points_at(host, &e.address, &resolve));
    if points_at_hub {
        return check(NAME, Status::Pass, format!("{hub_name} is the hub and this machine's `redis.host` is it"), None);
    }
    check(
        NAME,
        Status::Fail,
        format!("{hub_name} is the hub, but this machine's `redis.host` (`{host}`) is not that machine"),
        Some("Point this machine at the hub's Redis: `darkmux config set redis.host <the hub's address>`."),
    )
}

/// The "fleet defaults" row: what this machine resolves for the defaults the
/// hub hands out, and where each came from.
pub(crate) fn fleet_defaults_check(seat: Result<crate::radio_answer::SeatProvenance, String>, now_ms: u64) -> Check {
    match seat {
        Ok(p) => check("fleet defaults", Status::Pass, format!("radio answering seat: {}", p.describe(now_ms)), None),
        Err(e) => check(
            "fleet defaults",
            Status::Warn,
            format!("radio answering seat does not resolve: {e}"),
            Some("Fix the seat named above (`darkmux config set radio.answerer_profile <profile>@<machine>`)."),
        ),
    }
}

/// The two fleet rows `darkmux doctor` appends. They read the fleet view, which
/// the doctor crate does not depend on: it evaluates, this layer gathers.
pub(crate) fn doctor_checks() -> Vec<Check> {
    let view = crate::machine_list::local_fleet_view();
    let redis_host = darkmux_types::config_access::redis_enabled()
        .then(darkmux_types::config_access::redis_host)
        .flatten();
    vec![
        fleet_hub_check(&view, redis_host.as_deref(), darkmux_fleet::resolve_host_addrs),
        fleet_defaults_check(
            crate::radio_answer::answering_seat_provenance(&crate::radio_answer::AnswererOverrides::default()),
            now_ms(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_serve::fleet_view::{AcceptsState, CardSource, Liveness, UidSource};
    use darkmux_serve::machine_card::{CardRadioDefaults, MachineCard, FLEET_DEFAULTS_VERSION};
    use darkmux_serve::wire::RosterMachineEntry;
    use darkmux_types::config::DeclaredFleetMode;

    fn defaults(profile: Option<&str>) -> CardFleetDefaults {
        CardFleetDefaults {
            version: FLEET_DEFAULTS_VERSION,
            radio: CardRadioDefaults { answerer_profile: profile.map(str::to_string) },
        }
    }

    fn card(mode: DeclaredFleetMode, hosts_redis: bool, profile: Option<&str>) -> MachineCard {
        let mut card: MachineCard =
            serde_json::from_str(include_str!("../crates/darkmux-serve/tests/fixtures/machine-card-1.0.json")).unwrap();
        card.fleet_mode = Some(mode);
        card.hosts_fleet_redis = Some(hosts_redis);
        card.fleet_defaults = Some(defaults(profile));
        card
    }

    /// One row of a view: `outcome` is what asking it produced.
    fn row(id: &str, this: bool, outcome: CardOutcome) -> FleetMachine {
        FleetMachine {
            entry: Some(RosterMachineEntry {
                id: id.into(),
                address: format!("{id}.example.invalid:8765"),
                description: None,
                added_unix_ms: 1,
                machine_uid: None,
                loopback_intended: None,
            }),
            is_this_machine: this,
            machine_uid: None,
            uid_source: None::<UidSource>,
            liveness: Liveness::Unknown,
            last_beat_ms: None,
            received_at_ms: Some(1_000),
            fetch_ms: None,
            card: outcome,
            accepts: AcceptsState::Unknown,
        }
    }

    fn available(mode: DeclaredFleetMode, hosts_redis: bool, profile: Option<&str>) -> CardOutcome {
        CardOutcome::Available { card: Box::new(card(mode, hosts_redis, profile)), source: CardSource::Listener }
    }

    fn unreachable() -> CardOutcome {
        CardOutcome::Unreachable { reason: darkmux_serve::fleet_view::UnreachableReason::ListenerOff, detail: None }
    }

    fn view_of(machines: Vec<FleetMachine>) -> FleetView {
        FleetView {
            gathered_by: darkmux_serve::fleet_view::GatheredBy::Daemon,
            local_machine_id: Some("laptop".into()),
            presence: darkmux_serve::source_state::SourceState::Ok,
            roster_error: None,
            fetched_at_ms: 5_000,
            cache_ttl_ms: 0,
            gather_ms: 1,
            machines,
        }
    }

    fn fleet(hub: CardOutcome) -> FleetView {
        view_of(vec![
            row("laptop", true, available(DeclaredFleetMode::Peer, false, None)),
            row("studio", false, hub),
        ])
    }

    fn cache() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_KNOWN_FILE);
        (dir, path)
    }

    // ── resolution: live, last-known, refused ──

    /// The hub answers: its default is used, and saved with the time it was read.
    #[test]
    fn a_hub_that_answers_sets_the_default_and_is_saved() {
        let (_d, path) = cache();
        let view = fleet(available(DeclaredFleetMode::Hub, true, Some("deep@studio")));
        let seen = resolve_hub_default(&view, &path, 9_000).expect("the hub's default");
        assert_eq!((seen.answerer_profile(), seen.origin, seen.hub.as_str()), (Some("deep@studio"), HubDefaultOrigin::Live, "studio"));
        assert_eq!(seen.seen_at_ms, 1_000, "the time this machine READ it, not now");
        assert_eq!(read_last_known(&path).unwrap().defaults, defaults(Some("deep@studio")));
    }

    /// The hub cannot be asked: the saved copy is used and says how old it is.
    #[test]
    fn an_unreachable_hub_falls_back_to_the_saved_copy_with_its_age() {
        let (_d, path) = cache();
        let live = fleet(available(DeclaredFleetMode::Hub, true, Some("deep@studio")));
        resolve_hub_default(&live, &path, 9_000).unwrap();
        let down = fleet(unreachable());
        let seen = resolve_hub_default(&down, &path, 9_000).expect("the saved copy");
        assert_eq!((seen.answerer_profile(), seen.origin), (Some("deep@studio"), HubDefaultOrigin::LastKnown));
        assert_eq!(seen.age_words(65_000), "1m", "read at 1s, now 65s");
        // Recovery: the hub comes back with a new default and replaces the copy.
        let back = fleet(available(DeclaredFleetMode::Hub, true, Some("fast@studio")));
        assert_eq!(resolve_hub_default(&back, &path, 70_000).unwrap().answerer_profile(), Some("fast@studio"));
        assert_eq!(read_last_known(&path).unwrap().defaults, defaults(Some("fast@studio")));
    }

    /// No copy and no hub: nothing, not an invented default.
    #[test]
    fn no_saved_copy_and_no_hub_answer_is_no_default() {
        let (_d, path) = cache();
        assert_eq!(resolve_hub_default(&fleet(unreachable()), &path, 9_000), None);
    }

    /// A hub that answers and states no default clears the saved copy's
    /// effect: the stale value is not resurrected.
    #[test]
    fn a_hub_that_states_no_default_is_not_overridden_by_the_saved_copy() {
        let (_d, path) = cache();
        resolve_hub_default(&fleet(available(DeclaredFleetMode::Hub, true, Some("deep@studio"))), &path, 9_000).unwrap();
        let cleared = fleet(available(DeclaredFleetMode::Hub, true, None));
        let seen = resolve_hub_default(&cleared, &path, 9_500).expect("the hub answered");
        assert_eq!((seen.answerer_profile(), seen.origin), (None, HubDefaultOrigin::Live));
        let down = fleet(unreachable());
        assert_eq!(resolve_hub_default(&down, &path, 9_900).unwrap().answerer_profile(), None);
    }

    /// Defaults on a card that does not declare `hub` are refused, whatever
    /// else it carries, and the saved copy is not consulted for them either.
    #[test]
    fn defaults_on_a_non_hub_card_are_refused() {
        let (_d, path) = cache();
        for mode in [DeclaredFleetMode::Peer, DeclaredFleetMode::Standalone, DeclaredFleetMode::Unknown] {
            let view = fleet(available(mode, true, Some("evil@studio")));
            assert_eq!(resolve_hub_default(&view, &path, 9_000), None, "{mode:?}");
        }
        assert!(read_last_known(&path).is_none(), "nothing was saved from a non-hub card");
    }

    /// A hub card whose defaults are in a shape this darkmux does not read
    /// sets nothing, and never replaces the saved copy.
    #[test]
    fn a_hub_whose_defaults_are_in_an_unknown_shape_sets_nothing() {
        let (_d, path) = cache();
        resolve_hub_default(&fleet(available(DeclaredFleetMode::Hub, true, Some("deep@studio"))), &path, 9_000).unwrap();
        let mut newer = card(DeclaredFleetMode::Hub, true, Some("x@y"));
        newer.fleet_defaults.as_mut().unwrap().version = FLEET_DEFAULTS_VERSION + 1;
        let view = fleet(CardOutcome::Available { card: Box::new(newer), source: CardSource::Listener });
        assert_eq!(resolve_hub_default(&view, &path, 9_500), None);
        assert_eq!(read_last_known(&path).unwrap().defaults, defaults(Some("deep@studio")), "the saved copy is untouched");
    }

    /// Every card was read and none declares hub: the hub is gone, not merely
    /// quiet, so a copy saved while one existed is not used.
    #[test]
    fn a_fleet_with_no_hub_among_cards_all_read_ignores_the_saved_copy() {
        let (_d, path) = cache();
        resolve_hub_default(&fleet(available(DeclaredFleetMode::Hub, true, Some("deep@studio"))), &path, 9_000).unwrap();
        let headless = fleet(available(DeclaredFleetMode::Peer, false, None));
        assert_eq!(resolve_hub_default(&headless, &path, 9_500), None);
        // A machine that did not answer might be the hub: then the copy is used.
        let maybe = view_of(vec![
            row("laptop", true, available(DeclaredFleetMode::Peer, false, None)),
            row("studio", false, unreachable()),
        ]);
        assert!(resolve_hub_default(&maybe, &path, 9_500).is_some());
    }

    /// Two hubs is not one: no default is taken from either.
    #[test]
    fn two_hubs_set_no_default() {
        let (_d, path) = cache();
        let view = view_of(vec![
            row("laptop", true, available(DeclaredFleetMode::Hub, true, Some("a@laptop"))),
            row("studio", false, available(DeclaredFleetMode::Hub, true, Some("b@studio"))),
        ]);
        assert_eq!(resolve_hub_default(&view, &path, 9_000), None);
    }

    // ── doctor ──

    fn doctor(view: &FleetView, redis_host: Option<&str>) -> Check {
        let resolve = |s: &str| -> Vec<std::net::IpAddr> {
            match darkmux_fleet::address_host(s).as_deref() {
                Some("studio.example.invalid") => vec!["100.64.0.2".parse().unwrap()],
                Some("100.64.0.2") => vec!["100.64.0.2".parse().unwrap()],
                _ => Vec::new(),
            }
        };
        fleet_hub_check(view, redis_host, resolve)
    }

    #[test]
    fn doctor_passes_one_hub_that_hosts_the_redis_this_machine_points_at() {
        let view = fleet(available(DeclaredFleetMode::Hub, true, None));
        for host in ["studio.example.invalid", "100.64.0.2"] {
            let c = doctor(&view, Some(host));
            assert_eq!(c.status, Status::Pass, "{host}: {}", c.message);
        }
    }

    #[test]
    fn doctor_fails_two_hubs_and_names_them() {
        let view = view_of(vec![
            row("laptop", true, available(DeclaredFleetMode::Hub, true, None)),
            row("studio", false, available(DeclaredFleetMode::Hub, true, None)),
        ]);
        let c = doctor(&view, None);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("laptop") && c.message.contains("studio") && c.message.contains("exactly one"), "{}", c.message);
    }

    #[test]
    fn doctor_fails_a_hub_that_is_not_the_redis_host() {
        let c = doctor(&fleet(available(DeclaredFleetMode::Hub, false, None)), Some("studio.example.invalid"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("studio") && c.message.contains("redis.host"), "{}", c.message);
    }

    #[test]
    fn doctor_fails_a_machine_whose_redis_points_at_another_machine_than_the_hub() {
        let c = doctor(&fleet(available(DeclaredFleetMode::Hub, true, None)), Some("100.64.0.9"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("100.64.0.9") && c.message.contains("studio"), "{}", c.message);
        let loopback = doctor(&fleet(available(DeclaredFleetMode::Hub, true, None)), Some("127.0.0.1"));
        assert_eq!(loopback.status, Status::Fail, "this machine's own Redis is not the hub's");
    }

    #[test]
    fn doctor_warns_when_no_machine_declares_the_hub_and_passes_a_lone_machine() {
        let c = doctor(&fleet(available(DeclaredFleetMode::Peer, true, None)), None);
        assert_eq!(c.status, Status::Warn);
        let lone = view_of(vec![row("laptop", true, available(DeclaredFleetMode::Standalone, false, None))]);
        assert_eq!(doctor(&lone, None).status, Status::Pass);
    }

    #[test]
    fn the_hub_itself_passes_when_it_hosts_the_redis() {
        let view = view_of(vec![row("laptop", true, available(DeclaredFleetMode::Hub, true, None))]);
        assert_eq!(doctor(&view, Some("127.0.0.1")).status, Status::Pass);
    }
}
