//! (#2916) `darkmux doctor`'s fleet work-submission rows: who this machine
//! takes work from and in what scope, whether the fleet token resolves,
//! whether the listener is bound, and what the identity provider reports.
//!
//! A pure evaluator over [`FleetSubmissionFacts`]; the binary gathers the
//! facts (the doctor crate does not depend on the fleet crate). No row ever
//! prints a node id, a token, or a hardware id.

use crate::{Check, Status};

/// What the identity provider reported, reduced to what doctor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderReport {
    /// The value names no provider this darkmux knows.
    Unknown { value: String },
    /// The provider could not answer.
    Down { value: String, detail: String },
    /// This machine's node on the network: its name and the address the
    /// listener binds.
    Up { value: String, local_name: String, local_addr: Option<String> },
}

/// One allow-list entry, as doctor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustView {
    /// The allow-list key (the peer's machine name).
    pub name: String,
    /// Whether the entry carries a node id at all.
    pub has_node_id: bool,
    /// The node's name on the network NOW (looked up by its id), when the
    /// provider could list nodes and the node is still there.
    pub network_name: Option<String>,
    /// Whether the provider sees the node online, when it says.
    pub online: Option<bool>,
    /// False when the provider answered and the node is not on the network.
    pub node_on_network: bool,
    pub profiles: Vec<String>,
    pub roles: Vec<String>,
    pub images: Vec<String>,
    /// Profiles or roles in scope that cannot run here, each with why.
    pub profile_problems: Vec<String>,
    pub workspace: bool,
}

/// (#2916 stage 2) A listener's busy settings: its `fleet.busy_policy`
/// and its `remote.concurrent_cap` (hosted jobs at once, `0` = unbounded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusySettings {
    pub policy: darkmux_types::config::BusyPolicy,
    pub hosted_cap: u32,
}

/// (#2916 stage 2 review C5) The busy settings the running daemon reports
/// (what is in force; it reads config once, at start) and the ones
/// `config.json` holds now (`None` when a value is bad: the generic
/// enum-settings row reports that).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BusyFacts {
    pub running: Option<BusySettings>,
    pub configured: Option<BusySettings>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSubmissionFacts {
    pub listener_enabled: bool,
    pub port: u16,
    pub token_present: bool,
    pub provider: ProviderReport,
    /// `Some(true)` = something accepts on the listener address,
    /// `Some(false)` = nothing does, `None` = not probed (off, or no address).
    pub listener_bound: Option<bool>,
    /// `Err` = the allow-list could not be read.
    pub trusted: Result<Vec<TrustView>, String>,
    /// The retired `darkmux:work` streams still present in Redis, when Redis
    /// was configured and could be read.
    pub retired_streams: Vec<String>,
    /// Consumers still registered on those streams (`XINFO CONSUMERS`), by
    /// name (a machine id), with how long each has been idle. A consumer
    /// idle for seconds is a 3.x daemon still claiming queue work.
    pub queue_consumers: Vec<(String, u64)>,
    /// What the running daemon reports about its listener (`/health`'s
    /// `fleet_listener`), when the daemon answered.
    pub daemon_listener_state: Option<String>,
    /// Whether the running daemon reports a fleet token in ITS environment
    /// (`/health`'s `fleet_token_set`), when it answered. The shell doctor
    /// runs in may not have the one the daemon was started with.
    pub daemon_token_set: Option<bool>,
    /// Whether the running daemon can publish to the fleet hub's flow stream
    /// (`/health`'s `hub_link`), when the daemon answered and a hub is configured.
    pub daemon_hub_link: Option<darkmux_flow::HubLink>,
    /// (#2916 stage 2) How the listener answers a busy seat.
    pub busy: BusyFacts,
    /// (#2916 stage 2) This machine's resolved `machine_id`: the name a
    /// `profile@machine` address uses for it.
    pub local_machine: Option<String>,
}

fn check(name: &str, status: Status, message: String, hint: Option<String>) -> Check {
    Check { name: name.into(), status, message, hint }
}

/// The rows. Nothing at all for a machine with the listener off and an
/// empty allow-list and no leftover queue (a single machine has nothing to
/// report).
pub fn fleet_submission_checks(f: &FleetSubmissionFacts) -> Vec<Check> {
    let trusted_any = f.trusted.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
    let mut rows = Vec::new();
    if f.listener_enabled || trusted_any {
        rows.push(token_row(f));
        // (#2947) An unknown provider value is bad config, reported ONCE by
        // the generic `fleet.identity.provider` enum-settings row (value,
        // where it was set, valid values). No second Fail row here.
        if !matches!(f.provider, ProviderReport::Unknown { .. }) {
            rows.push(identity_row(f));
        }
        rows.push(listener_row(f));
        rows.push(trust_row(f));
    }
    rows.extend(f.daemon_hub_link.as_ref().map(hub_link_row));
    if !f.retired_streams.is_empty() {
        let live: Vec<String> = f
            .queue_consumers
            .iter()
            .filter(|(_, idle)| *idle < 60_000)
            .map(|(n, idle)| format!("{n} (idle {}s)", idle / 1000))
            .collect();
        let (status, message, hint) = if live.is_empty() {
            (
                Status::Warn,
                format!(
                    "Redis still holds the retired work queue ({}); no 4.0 daemon reads it (#2916)",
                    f.retired_streams.join(", ")
                ),
                format!(
                    "Delete it on the hub once every machine runs 4.0: `redis-cli DEL {}`. A 3.x \
                     daemon still consumes it (and re-creates it when it starts).",
                    f.retired_streams.join(" ")
                ),
            )
        } else {
            (
                Status::Fail,
                format!(
                    "3.x daemons are still consuming the retired work queue: {}. Any node that can \
                     write this Redis can make them run work, unauthenticated",
                    live.join(", ")
                ),
                "Upgrade those machines to 4.0 (or stop their `darkmux serve`), then delete the \
                 streams: the queue hole closes only when no 3.x daemon is left."
                    .to_string(),
            )
        };
        rows.push(check("retired work queue", status, message, Some(hint)));
    }
    rows
}

/// The daemon's link to the hub's flow stream. While it is down the daemon
/// keeps writing its local day files and re-sends them when the hub answers, so
/// the row says what is owed, not that anything is lost.
fn hub_link_row(link: &darkmux_flow::HubLink) -> Check {
    use darkmux_flow::HubLink;
    match link {
        HubLink::Connected => check("flow hub link", Status::Pass, "publishing to the hub's flow stream".into(), None),
        HubLink::Unverified => {
            check("flow hub link", Status::Pass, "no write has been attempted yet".into(), None)
        }
        // (#3035) A link state a newer darkmux reported: not a pass.
        HubLink::Unknown => check(
            "flow hub link",
            Status::Warn,
            "the daemon reported a hub link state this darkmux does not know".into(),
            Some("Upgrade this darkmux to the version the daemon runs.".into()),
        ),
        HubLink::Unreachable { since, reason } => check(
            "flow hub link",
            Status::Warn,
            format!(
                "the hub's flow stream has been unreachable since {since} ({reason}); this daemon \
                 keeps its records in its local day files and re-sends them when the hub answers"
            ),
            Some(
                "Check that the hub's Redis is up and reachable from this machine. The daemon \
                 probes on its own, so nothing needs a restart."
                    .into(),
            ),
        ),
    }
}

fn token_row(f: &FleetSubmissionFacts) -> Check {
    if f.token_present {
        check("fleet token", Status::Pass, "resolves (the serve token)".into(), None)
    } else if f.daemon_token_set == Some(true) {
        check(
            "fleet token",
            Status::Pass,
            "set in the running daemon's environment only: this shell cannot resolve it, so a \
             `dispatch` from here cannot send fleet work"
                .into(),
            None,
        )
    } else {
        check(
            "fleet token",
            if f.listener_enabled { Status::Fail } else { Status::Warn },
            "does not resolve: this machine can neither take nor submit fleet work".into(),
            Some(
                "Store the fleet's one shared token (the serve token) in the Keychain: \
                 `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` and set \
                 `darkmux config set serve.token_keychain true` (or export DARKMUX_SERVE_TOKEN). \
                 Every machine holds the same value."
                    .into(),
            ),
        )
    }
}

fn identity_row(f: &FleetSubmissionFacts) -> Check {
    match &f.provider {
        ProviderReport::Unknown { value } => check(
            "fleet identity",
            Status::Fail,
            format!("unknown identity provider `{value}`: every submission is refused"),
            Some("`darkmux config set fleet.identity.provider tailscale`".into()),
        ),
        ProviderReport::Down { value, detail } => check(
            "fleet identity",
            if f.listener_enabled { Status::Fail } else { Status::Warn },
            format!("{value} cannot answer ({detail}): every submission is refused until it can"),
            Some(format!(
                "Check that {value} is running and logged in on this machine. If its command is \
                 not on the daemon's PATH, `darkmux config set fleet.identity.bin <path>`."
            )),
        ),
        ProviderReport::Up { value, local_name, local_addr } => check(
            "fleet identity",
            Status::Pass,
            format!(
                "{value}: this machine is `{local_name}`{}{}",
                local_addr.as_deref().map(|a| format!(" at {a}")).unwrap_or_default(),
                // (#2916 stage 2, decision 10) Addresses resolve at dispatch
                // time against the roster, by machine_id; presence reads it
                // once per daemon start.
                f.local_machine
                    .as_deref()
                    .map(|m| format!(
                        "; profile addresses name it `<profile>@{m}` (its machine_id; after `darkmux \
                         config set machine_id`, restart `darkmux serve` so presence shows the new name)"
                    ))
                    .unwrap_or_default()
            ),
            None,
        ),
    }
}

fn listener_row(f: &FleetSubmissionFacts) -> Check {
    if !f.listener_enabled {
        return check(
            "fleet listener",
            Status::Warn,
            "off: this machine takes no work from other machines, though its allow-list names some".into(),
            Some("`darkmux config set fleet.listener.enabled true`, then restart `darkmux serve`.".into()),
        );
    }
    let addr = match &f.provider {
        ProviderReport::Up { local_addr: Some(a), .. } => format!("{a}:{}", f.port),
        _ => format!("<overlay address>:{}", f.port),
    };
    match f.listener_bound {
        Some(true) => check("fleet listener", Status::Pass, format!("listening on {addr}{}", busy_note(f)), None),
        Some(false) => check(
            "fleet listener",
            Status::Warn,
            match &f.daemon_listener_state {
                Some(state) => format!("enabled, but nothing is listening on {addr}; the daemon says: {state}"),
                None => format!("enabled, but nothing is listening on {addr}"),
            },
            Some(
                "Is `darkmux serve` running, and was it restarted after enabling the listener? A \
                 daemon started by launchd has a short PATH: `darkmux config set fleet.identity.bin \
                 <path to tailscale>` if the daemon cannot find it."
                    .into(),
            ),
        ),
        None => check(
            "fleet listener",
            Status::Warn,
            format!("enabled, but not checked: no overlay address to bind (port {})", f.port),
            None,
        ),
    }
}

/// (#2916 stage 2) What a busy seat gets, for the listener row: the
/// running daemon's settings, or the file's when the daemon did not say.
fn busy_note(f: &FleetSubmissionFacts) -> String {
    let BusyFacts { running, configured } = f.busy;
    let Some(shown) = running.or(configured) else { return String::new() };
    let mut note = format!("; {}", describe_busy(shown));
    match (running, configured) {
        (None, Some(_)) => {
            note.push_str(" (from config.json; the running daemon did not report its own)");
        }
        (Some(r), Some(c)) if r != c => {
            note.push_str(&format!("; config.json now says {}: restart `darkmux serve` to apply it", describe_busy(c)));
        }
        // The daemon and the file agree, or the file's value is bad (the
        // enum-settings row reports that), or neither said anything.
        (Some(_), Some(_)) | (Some(_), None) | (None, None) => {}
    }
    note
}

fn describe_busy(b: BusySettings) -> String {
    use darkmux_types::config::BusyPolicy;
    let hosted = match b.hosted_cap {
        0 => "hosted jobs unbounded".to_string(),
        n => format!("hosted jobs up to {n} (remote.concurrent_cap)"),
    };
    let past = match b.policy {
        BusyPolicy::Queue => "queues the rest",
        BusyPolicy::Refuse => "refuses the rest at once",
    };
    // (#2916 stage 2 review C3) The listener counts only the jobs other
    // machines submit; a local model this machine is using for its own
    // dispatch is not "busy" to it.
    format!(
        "one fleet job per local model (this machine's own dispatches are not counted), {hosted}; \
         fleet.busy_policy `{}` {past}",
        b.policy.as_str()
    )
}

fn trust_row(f: &FleetSubmissionFacts) -> Check {
    let entries = match &f.trusted {
        Err(e) => {
            return check(
                "fleet trust",
                Status::Fail,
                format!("the allow-list cannot be read ({e}): the listener refuses everything"),
                Some("Fix the JSON in config.json; `darkmux machine trust` rewrites one entry.".into()),
            )
        }
        Ok(t) => t,
    };
    if entries.is_empty() {
        return check(
            "fleet trust",
            Status::Pass,
            "no machine is trusted: every submission is refused (deny by default)".into(),
            Some("`darkmux machine trust <machine> --profiles <profile>` lets one in.".into()),
        );
    }
    let mut worst = Status::Pass;
    let mut lines = Vec::new();
    let mut hints = Vec::new();
    for e in entries {
        let mut notes = Vec::new();
        if !e.has_node_id {
            worst = Status::Warn;
            notes.push("NO node id, matches nothing".to_string());
            hints.push(format!("re-run `darkmux machine trust {}` to resolve its node", e.name));
        } else if !e.node_on_network {
            worst = Status::Warn;
            notes.push("its node is no longer on the network".to_string());
            hints.push(format!("`darkmux machine untrust {}` if it left the fleet", e.name));
        } else if let Some(n) = &e.network_name {
            let state = match e.online {
                Some(true) => ", online",
                Some(false) => ", offline",
                None => "",
            };
            notes.push(format!("node `{n}`{state}"));
        }
        if !e.profile_problems.is_empty() {
            worst = Status::Warn;
            notes.extend(e.profile_problems.iter().cloned());
        }
        if e.roles.is_empty() {
            worst = Status::Warn;
            notes.push("no roles listed, so nothing can run".to_string());
            hints.push(format!("`darkmux machine trust {} --roles <role>[,...]`", e.name));
        }
        lines.push(format!(
            "{} may run {} (roles: {}; images: {}; workspace: {}){}",
            e.name,
            if e.profiles.is_empty() { "nothing".to_string() } else { e.profiles.join(", ") },
            if e.roles.is_empty() { "none".to_string() } else { e.roles.join(", ") },
            if e.images.is_empty() { "runtime only".to_string() } else { e.images.join(", ") },
            if e.workspace { "yes" } else { "no" },
            if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) }
        ));
    }
    let provider = match &f.provider {
        ProviderReport::Up { value, .. } | ProviderReport::Down { value, .. } | ProviderReport::Unknown { value } => value,
    };
    check(
        "fleet trust",
        worst,
        format!("verified by {provider}: {}", lines.join(" · ")),
        (!hints.is_empty()).then(|| hints.join("; ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> ProviderReport {
        ProviderReport::Up { value: "tailscale".into(), local_name: "studio".into(), local_addr: Some("100.64.0.2".into()) }
    }

    fn trusted() -> TrustView {
        TrustView {
            name: "laptop".into(),
            has_node_id: true,
            network_name: Some("laptop".into()),
            online: Some(true),
            node_on_network: true,
            profiles: vec!["host".into(), "coder-studio".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            profile_problems: vec![],
            workspace: false,
        }
    }

    fn refuse(hosted_cap: u32) -> BusySettings {
        BusySettings { policy: darkmux_types::config::BusyPolicy::Refuse, hosted_cap }
    }

    fn queue(hosted_cap: u32) -> BusySettings {
        BusySettings { policy: darkmux_types::config::BusyPolicy::Queue, hosted_cap }
    }

    fn facts() -> FleetSubmissionFacts {
        FleetSubmissionFacts {
            listener_enabled: true,
            port: 8766,
            token_present: true,
            provider: up(),
            listener_bound: Some(true),
            trusted: Ok(vec![trusted()]),
            retired_streams: vec![],
            queue_consumers: vec![],
            daemon_listener_state: None,
            daemon_token_set: None,
            daemon_hub_link: None,
            busy: BusyFacts { running: Some(refuse(1)), configured: Some(refuse(1)) },
            local_machine: Some("studio".into()),
        }
    }

    fn row<'a>(rows: &'a [Check], name: &str) -> &'a Check {
        rows.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no `{name}` row in {rows:?}"))
    }

    #[test]
    fn the_hub_link_row_warns_while_unreachable_and_passes_once_connected() {
        use darkmux_flow::HubLink;
        let absent = fleet_submission_checks(&facts());
        assert!(absent.iter().all(|c| c.name != "flow hub link"), "no hub configured: no row");
        let down = FleetSubmissionFacts {
            daemon_hub_link: Some(HubLink::Unreachable {
                since: "2026-10-01T03:04:05Z".into(),
                reason: "Connection refused".into(),
            }),
            ..facts()
        };
        let rows = fleet_submission_checks(&down);
        let r = row(&rows, "flow hub link");
        assert_eq!(r.status, Status::Warn);
        assert!(r.message.contains("2026-10-01T03:04:05Z") && r.message.contains("Connection refused"), "{r:?}");
        let up = FleetSubmissionFacts { daemon_hub_link: Some(HubLink::Connected), ..facts() };
        assert_eq!(row(&fleet_submission_checks(&up), "flow hub link").status, Status::Pass);
    }

    #[test]
    fn a_healthy_receiver_lists_who_it_trusts_and_passes() {
        let rows = fleet_submission_checks(&facts());
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|c| c.status == Status::Pass), "{rows:?}");
        let t = row(&rows, "fleet trust");
        assert!(t.message.contains("verified by tailscale"), "{}", t.message);
        assert!(t.message.contains("laptop may run host, coder-studio (roles: radio-host; images: runtime only; workspace: no)"), "{}", t.message);
        assert!(t.message.contains("node `laptop`, online"), "{}", t.message);
        assert_eq!(
            row(&rows, "fleet listener").message,
            "listening on 100.64.0.2:8766; one fleet job per local model (this machine's own \
             dispatches are not counted), hosted jobs up to 1 (remote.concurrent_cap); \
             fleet.busy_policy `refuse` refuses the rest at once"
        );
        assert!(row(&rows, "fleet identity").message.contains("this machine is `studio`"));
    }

    /// (#2916 stage 2) The rows say how a busy seat is answered, and what
    /// name a `profile@machine` address uses for this machine.
    #[test]
    fn the_rows_name_the_busy_policy_and_the_address_name() {
        let f = FleetSubmissionFacts { busy: BusyFacts { running: Some(queue(0)), configured: Some(queue(0)) }, ..facts() };
        let rows = fleet_submission_checks(&f);
        let l = &row(&rows, "fleet listener").message;
        assert!(l.contains("hosted jobs unbounded") && l.contains("`queue` queues the rest"), "{l}");
        assert!(!l.contains("config.json"), "the daemon and the file agree: {l}");
        let i = &row(&rows, "fleet identity").message;
        assert!(i.contains("`<profile>@studio`") && i.contains("restart `darkmux serve`"), "{i}");
        // A bad policy value is the enum-settings row's to report, not this one's.
        let f = FleetSubmissionFacts { busy: BusyFacts::default(), ..facts() };
        assert_eq!(row(&fleet_submission_checks(&f), "fleet listener").message, "listening on 100.64.0.2:8766");
    }

    /// (#2916 stage 2 review C5) The row shows what the running daemon
    /// uses; a file that says otherwise is named, with the restart that
    /// applies it; with no daemon answer, the file's values are marked as
    /// such.
    #[test]
    fn the_busy_row_shows_the_running_values_and_what_a_restart_would_change() {
        let f = FleetSubmissionFacts { busy: BusyFacts { running: Some(refuse(1)), configured: Some(queue(3)) }, ..facts() };
        let l = row(&fleet_submission_checks(&f), "fleet listener").message.clone();
        assert!(l.contains("`refuse` refuses the rest at once"), "shows the running policy: {l}");
        assert!(l.contains("config.json now says") && l.contains("up to 3") && l.contains("restart `darkmux serve`"), "{l}");
        let f = FleetSubmissionFacts { busy: BusyFacts { running: None, configured: Some(queue(2)) }, ..facts() };
        let l = row(&fleet_submission_checks(&f), "fleet listener").message.clone();
        assert!(l.contains("`queue`") && l.contains("from config.json; the running daemon did not report"), "{l}");
    }

    /// (#2947) A bad provider value gets no `fleet identity` row: the
    /// generic enum-settings row is the one Fail for it.
    #[test]
    fn an_unknown_provider_value_is_left_to_the_enum_settings_row() {
        let f = FleetSubmissionFacts { provider: ProviderReport::Unknown { value: "zz".into() }, ..facts() };
        let rows = fleet_submission_checks(&f);
        assert!(rows.iter().all(|c| c.name != "fleet identity"), "{rows:?}");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_single_machine_gets_no_rows() {
        let f = FleetSubmissionFacts { listener_enabled: false, trusted: Ok(vec![]), ..facts() };
        assert!(fleet_submission_checks(&f).is_empty());
    }

    /// The shell has no token but the running daemon does: the row passes and
    /// says whose environment holds it. Without that word from the daemon
    /// (none reachable, or it reports none) the row keeps failing.
    #[test]
    fn a_token_only_the_daemon_holds_passes_with_a_note() {
        let f = FleetSubmissionFacts { token_present: false, daemon_token_set: Some(true), ..facts() };
        let r = fleet_submission_checks(&f);
        let r = row(&r, "fleet token");
        assert_eq!(r.status, Status::Pass);
        assert!(r.message.contains("daemon's environment only"), "{}", r.message);
        for daemon in [None, Some(false)] {
            let f = FleetSubmissionFacts { token_present: false, daemon_token_set: daemon, ..facts() };
            assert_eq!(row(&fleet_submission_checks(&f), "fleet token").status, Status::Fail, "{daemon:?}");
        }
    }

    #[test]
    fn each_failure_is_named() {
        let f = FleetSubmissionFacts { token_present: false, ..facts() };
        assert_eq!(row(&fleet_submission_checks(&f), "fleet token").status, Status::Fail);

        let f = FleetSubmissionFacts {
            provider: ProviderReport::Down { value: "tailscale".into(), detail: "not running".into() },
            listener_bound: None,
            ..facts()
        };
        let rows = fleet_submission_checks(&f);
        assert_eq!(row(&rows, "fleet identity").status, Status::Fail);
        assert!(row(&rows, "fleet identity").message.contains("every submission is refused"));

        let f = FleetSubmissionFacts { listener_bound: Some(false), ..facts() };
        assert_eq!(row(&fleet_submission_checks(&f), "fleet listener").status, Status::Warn);

        let f = FleetSubmissionFacts { listener_enabled: false, listener_bound: None, ..facts() };
        let l = fleet_submission_checks(&f);
        assert!(row(&l, "fleet listener").message.starts_with("off"));

        let mut gone = trusted();
        gone.node_on_network = false;
        gone.network_name = None;
        let mut unresolved = trusted();
        unresolved.name = "mini".into();
        unresolved.has_node_id = false;
        let mut util = trusted();
        util.name = "peer".into();
        util.profile_problems = vec!["profile `utility` lists only this machine's utility model".into()];
        let f = FleetSubmissionFacts { trusted: Ok(vec![gone, unresolved, util]), ..facts() };
        let t = fleet_submission_checks(&f);
        let t = row(&t, "fleet trust");
        assert_eq!(t.status, Status::Warn);
        assert!(t.message.contains("no longer on the network"), "{}", t.message);
        assert!(t.message.contains("NO node id"), "{}", t.message);
        assert!(t.message.contains("utility model"), "{}", t.message);

        let f = FleetSubmissionFacts { trusted: Err("bad json".into()), ..facts() };
        assert_eq!(row(&fleet_submission_checks(&f), "fleet trust").status, Status::Fail);
    }

    #[test]
    fn the_retired_queue_is_named_with_the_cleanup() {
        let f = FleetSubmissionFacts {
            listener_enabled: false,
            trusted: Ok(vec![]),
            retired_streams: vec!["darkmux:work".into(), "darkmux:work:inference".into()],
            ..facts()
        };
        let rows = fleet_submission_checks(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Warn);
        assert!(rows[0].hint.as_deref().unwrap().contains("redis-cli DEL darkmux:work darkmux:work:inference"));
    }

    /// (#2916 review C6) A 3.x daemon still consuming the queue is named and
    /// fails; the listener row carries the daemon's own reason.
    #[test]
    fn live_queue_consumers_fail_and_the_daemon_reason_shows() {
        let f = FleetSubmissionFacts {
            listener_enabled: false,
            trusted: Ok(vec![]),
            retired_streams: vec!["darkmux:work".into()],
            queue_consumers: vec![("studio".into(), 1_500), ("old-box".into(), 9_000_000)],
            ..facts()
        };
        let rows = fleet_submission_checks(&f);
        assert_eq!(rows[0].status, Status::Fail);
        assert!(rows[0].message.contains("studio (idle 1s)") && !rows[0].message.contains("old-box"), "{}", rows[0].message);
        let f = FleetSubmissionFacts {
            listener_bound: Some(false),
            daemon_listener_state: Some("waiting for the tailscale network to answer".into()),
            ..facts()
        };
        let rows = fleet_submission_checks(&f);
        assert!(row(&rows, "fleet listener").message.contains("the daemon says: waiting for the tailscale network"));
        let mut noroles = trusted();
        noroles.roles.clear();
        let f = FleetSubmissionFacts { trusted: Ok(vec![noroles]), ..facts() };
        assert_eq!(row(&fleet_submission_checks(&f), "fleet trust").status, Status::Warn);
    }

    #[test]
    fn no_row_prints_a_node_id() {
        // TrustView carries no id at all; this pins that the rendered text
        // is built only from names.
        let rows = fleet_submission_checks(&facts());
        for c in rows {
            assert!(!c.message.contains("nLAPTOP") && !c.hint.unwrap_or_default().contains("nLAPTOP"));
        }
    }
}
