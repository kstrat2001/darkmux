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
    /// Profiles in scope that cannot run here, each with why.
    pub profile_problems: Vec<String>,
    pub workspace: bool,
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
        rows.push(identity_row(f));
        rows.push(listener_row(f));
        rows.push(trust_row(f));
    }
    if !f.retired_streams.is_empty() {
        rows.push(check(
            "retired work queue",
            Status::Warn,
            format!(
                "Redis still holds the retired work queue ({}); nothing reads it any more (#2916)",
                f.retired_streams.join(", ")
            ),
            Some(format!(
                "Delete it on the hub: `redis-cli DEL {}`. Work now goes machine to machine \
                 over the fleet listener.",
                f.retired_streams.join(" ")
            )),
        ));
    }
    rows
}

fn token_row(f: &FleetSubmissionFacts) -> Check {
    if f.token_present {
        check("fleet token", Status::Pass, "resolves (the serve token)".into(), None)
    } else {
        check(
            "fleet token",
            if f.listener_enabled { Status::Fail } else { Status::Warn },
            "does not resolve: this machine can neither take nor submit fleet work".into(),
            Some(
                "Store the fleet's one shared token (the serve token) in the Keychain: \
                 `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` and set \
                 `darkmux config set runtime.daemon_auth_enabled true` (or export DARKMUX_SERVE_TOKEN). \
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
                "{value}: this machine is `{local_name}`{}",
                local_addr.as_deref().map(|a| format!(" at {a}")).unwrap_or_default()
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
        Some(true) => check("fleet listener", Status::Pass, format!("listening on {addr}"), None),
        Some(false) => check(
            "fleet listener",
            Status::Warn,
            format!("enabled, but nothing is listening on {addr}"),
            Some(
                "Is `darkmux serve` running, and was it restarted after enabling the listener? Its \
                 log names why the listener did not start."
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
        lines.push(format!(
            "{} may run {} (workspace: {}){}",
            e.name,
            if e.profiles.is_empty() { "nothing".to_string() } else { e.profiles.join(", ") },
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
            profile_problems: vec![],
            workspace: false,
        }
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
        }
    }

    fn row<'a>(rows: &'a [Check], name: &str) -> &'a Check {
        rows.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no `{name}` row in {rows:?}"))
    }

    #[test]
    fn a_healthy_receiver_lists_who_it_trusts_and_passes() {
        let rows = fleet_submission_checks(&facts());
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|c| c.status == Status::Pass), "{rows:?}");
        let t = row(&rows, "fleet trust");
        assert!(t.message.contains("verified by tailscale"), "{}", t.message);
        assert!(t.message.contains("laptop may run host, coder-studio (workspace: no)"), "{}", t.message);
        assert!(t.message.contains("node `laptop`, online"), "{}", t.message);
        assert_eq!(row(&rows, "fleet listener").message, "listening on 100.64.0.2:8766");
        assert!(row(&rows, "fleet identity").message.contains("this machine is `studio`"));
    }

    #[test]
    fn a_single_machine_gets_no_rows() {
        let f = FleetSubmissionFacts { listener_enabled: false, trusted: Ok(vec![]), ..facts() };
        assert!(fleet_submission_checks(&f).is_empty());
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
