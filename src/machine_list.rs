//! `darkmux machine list`: the fleet view, printed.
//!
//! The verb gathers the same view every daemon serves at `GET /fleet/view`
//! (`darkmux_serve::fleet_view`): one row per machine, each with the card
//! that machine states about itself, fetched in parallel from its fleet
//! listener over the verified peer path, and this machine's own row always.
//! Under `--json` it prints the `FleetView` itself.
//!
//! When this machine's own daemon answers, the verb prints the view THAT
//! daemon gathered, so this machine's row has its seats and governor readings
//! like on any other machine. When none answers, the view is gathered here and
//! this machine's row has none: the text and `--json` (`gathered_by`) say they
//! are not observed, not that they are absent.
//!
//! Every enum's `unknown` arm prints as "unknown", never as a known value.

use darkmux_serve::fleet_view::{
    AcceptsState, CardOutcome, FleetMachine, FleetView, GatheredBy, Liveness, UnavailableWhy, UnreachableReason,
    VersionSource,
};
use darkmux_serve::machine_card::{CardBusyPolicy, CardEndpointKind, CardSeats, MachineCard};
use darkmux_types::style;

/// A peer's strings are cut to their column so padding cannot push text into
/// the next one.
const MODELS_COL_CHARS: usize = 60;

fn human_gb(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{:.0} GB", gb.round())
}

fn liveness_cell(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "live",
        Liveness::NoBeat => "none",
        Liveness::Unknown => "?",
    }
}

fn kind_word(k: CardEndpointKind) -> &'static str {
    match k {
        CardEndpointKind::Managed => "managed",
        CardEndpointKind::Unmanaged => "unmanaged",
        CardEndpointKind::Mixed => "mixed",
        CardEndpointKind::Unresolved => "unresolved",
        CardEndpointKind::Unknown => "unknown",
    }
}

fn policy_word(p: CardBusyPolicy) -> &'static str {
    match p {
        CardBusyPolicy::Refuse => "refuse",
        CardBusyPolicy::Queue => "queue",
        CardBusyPolicy::Unknown => "unknown",
    }
}

fn unreachable_phrase(reason: UnreachableReason, detail: Option<&str>) -> String {
    let base = match reason {
        UnreachableReason::BadAddress => "roster address is not a usable address; nothing sent",
        UnreachableReason::DnsFailed => "roster address did not resolve; nothing sent",
        UnreachableReason::IdentityUnavailable => "the network identity tool could not verify it; nothing sent",
        UnreachableReason::NotOnOverlay => "address is not a node on the network; nothing sent",
        UnreachableReason::PinMismatch => "address is not its pinned node; nothing sent",
        UnreachableReason::ListenerOff => "its fleet listener did not answer",
        UnreachableReason::AuthRequired => "needs a fleet token this machine is not sending",
        UnreachableReason::RefusedByPeer => "its fleet listener refused this machine",
        UnreachableReason::ListenerUnavailable => "its fleet listener cannot serve a card now",
        UnreachableReason::BadAnswer => "answered with something that is not a card",
        UnreachableReason::Unknown => "a reason this darkmux does not know",
    };
    match detail {
        Some(d) => format!("unreachable: {base} ({d})"),
        None => format!("unreachable: {base}"),
    }
}

/// "peer 5.0.0" or "peer 5.0.0, from presence", or "peer version unknown".
fn peer_words(version: Option<&str>, source: Option<VersionSource>) -> String {
    match (version, source) {
        (Some(v), Some(VersionSource::Presence)) => format!("peer {v}, from presence"),
        (Some(v), _) => format!("peer {v}"),
        (None, _) => "peer version unknown".to_string(),
    }
}

fn unavailable_phrase(why: UnavailableWhy, peer: &str) -> String {
    let what = match why {
        UnavailableWhy::NoCardRoute => "no card route",
        UnavailableWhy::OtherSchemaMajor => "unreadable card schema",
        UnavailableWhy::Unparseable => "its card did not parse",
        UnavailableWhy::Unknown => "a reason this darkmux does not know",
    };
    format!("card unavailable ({peer}; {what})")
}

/// The table cells of one machine: headroom, OS, version, and the last
/// column (loaded models, or why there is no card).
fn cells(m: &FleetMachine) -> [String; 4] {
    let dash = || "-".to_string();
    match &m.card {
        CardOutcome::Available { card, .. } => {
            let s = &card.specs;
            let models = s.loaded_models.iter().map(|x| x.identifier.as_str()).collect::<Vec<_>>().join(", ");
            [
                s.ram_free_for_ai_bytes.map(human_gb).unwrap_or_else(dash),
                s.os.clone(),
                s.darkmux_version.clone(),
                if models.is_empty() { dash() } else { models },
            ]
        }
        CardOutcome::Unavailable { why, peer_version, peer_version_source } => [
            dash(),
            dash(),
            peer_version.clone().unwrap_or_else(dash),
            unavailable_phrase(*why, &peer_words(peer_version.as_deref(), *peer_version_source)),
        ],
        CardOutcome::Mismatch { answered_as } => [
            dash(),
            dash(),
            dash(),
            format!("answered as {}; not this machine's card, not used", answered_as.as_deref().unwrap_or("no name")),
        ],
        CardOutcome::Unreachable { reason, detail } => {
            [dash(), dash(), dash(), unreachable_phrase(*reason, detail.as_deref())]
        }
        CardOutcome::Unknown => [dash(), dash(), dash(), "an answer this darkmux does not know".to_string()],
    }
}

/// What a peer lets this machine do, in a line's words.
fn accepts_phrase(accepts: &AcceptsState) -> Option<String> {
    Some(match accepts {
        AcceptsState::Granted { accepts } => format!(
            "accepts from this machine: profiles {}; roles {}",
            list_or_none(&accepts.profiles),
            list_or_none(&accepts.roles)
        ),
        AcceptsState::NotListed => "does not list this machine: it takes no work from it".to_string(),
        AcceptsState::ThisMachine | AcceptsState::Withheld => return None,
        AcceptsState::Unknown => "accepts from this machine: unknown".to_string(),
    })
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn profiles_line(card: &MachineCard) -> String {
    let profiles = &card.profiles;
    if let Some(why) = &card.profiles_error {
        return format!("profiles: unreadable ({why})");
    }
    if profiles.is_empty() {
        return "profiles: none".to_string();
    }
    let each: Vec<String> = profiles.iter().map(|p| format!("{} ({})", p.name, kind_word(p.endpoint_kind))).collect();
    format!("profiles: {}", each.join(", "))
}

/// The seat line. It states what jobs from OTHER machines hold, and says so:
/// the seat block never counts this machine's own work, so it never says
/// "free".
fn seats_line(seats: &CardSeats) -> String {
    let hosted = match seats.hosted.cap {
        Some(cap) => format!("{}/{cap}", seats.hosted.held_by_peer_jobs),
        None => format!("{}/unbounded", seats.hosted.held_by_peer_jobs),
    };
    let held = seats.local.iter().filter(|s| s.held_by_peer_job).count();
    let scope = if seats.counts_own_work { "" } else { " (jobs from other machines only)" };
    format!(
        "seats{scope}: {held} local held, hosted {hosted}, {} waiting, busy policy {}",
        seats.waiting,
        policy_word(seats.busy_policy)
    )
}

/// The dim lines under a machine whose card was read: profiles, seats,
/// thermal, and what it lets this machine do.
fn detail_lines(card: &MachineCard, accepts: &AcceptsState) -> Vec<String> {
    let mut lines = vec![detail_line(card)];
    if let Some(phrase) = accepts_phrase(accepts) {
        lines.push(format!("  {phrase}"));
    }
    lines
}

/// The first dim line: profiles, seats, thermal.
fn detail_line(card: &MachineCard) -> String {
    let mut parts = vec![profiles_line(card)];
    if let Some(seats) = &card.seats {
        parts.push(seats_line(seats));
    }
    if let Some(t) = &card.governor.thermal {
        parts.push(format!("thermal: {}", t.state));
    }
    format!("  {}", parts.join("; "))
}

/// The name a row is listed under: its roster id, or this machine's own name
/// when the roster has no entry for it.
fn row_name(view: &FleetView, m: &FleetMachine) -> String {
    match &m.entry {
        Some(e) => e.id.clone(),
        None => view.local_machine_id.clone().unwrap_or_else(|| "<this machine>".to_string()),
    }
}

fn remedies(view: &FleetView) -> Vec<String> {
    let names = |want: &dyn Fn(&CardOutcome) -> bool| -> Vec<String> {
        view.machines.iter().filter(|m| want(&m.card)).map(|m| row_name(view, m)).collect()
    };
    let reason = |r: UnreachableReason| -> Vec<String> {
        names(&|c| matches!(c, CardOutcome::Unreachable { reason, .. } if *reason == r))
    };
    let mut out = Vec::new();
    let mut note = |names: Vec<String>, what: &str, fix: &str| {
        if !names.is_empty() {
            out.push(format!("! {} machine(s) {what} ({}): {fix}", names.len(), names.join(", ")));
        }
    };
    note(
        reason(UnreachableReason::PinMismatch),
        "not asked",
        "the address is not their pinned tailnet node, so the fleet token was not sent. Re-add each by its tailnet DNS name (`darkmux machine add <id> --address <dns-name>`).",
    );
    note(
        reason(UnreachableReason::NotOnOverlay),
        "not asked",
        "the address is not a node on the network, so the fleet token was not sent. Re-add each by its tailnet DNS name (`darkmux machine add <id> --address <dns-name>`).",
    );
    note(
        reason(UnreachableReason::DnsFailed),
        "not asked",
        "the roster address did not resolve. Check the name (`darkmux machine list` shows the roster path) or this machine's DNS.",
    );
    note(
        reason(UnreachableReason::IdentityUnavailable),
        "not asked",
        "the network identity tool could not verify the address (is it running and signed in on this machine?). `darkmux doctor` names the tool's own error.",
    );
    note(
        reason(UnreachableReason::BadAddress),
        "not asked",
        "the roster address is not a usable address. Re-add each with `darkmux machine add <id> --address <dns-name>`.",
    );
    note(
        reason(UnreachableReason::ListenerOff),
        "did not answer on their fleet listener",
        "the listener is off or not reachable from here. A machine is visible to the fleet only while its listener runs: enable `fleet.listener` on each (`darkmux doctor` there says why it is not up).",
    );
    note(
        reason(UnreachableReason::AuthRequired),
        "require a fleet token this machine isn't sending",
        "set DARKMUX_SERVE_TOKEN (or the darkmux-serve-token Keychain item) to the shared fleet token.",
    );
    note(
        reason(UnreachableReason::RefusedByPeer),
        "refused this machine at their fleet listener",
        "the detail above is each one's own sentence: it could not place this machine's address on its network, or the request came from its own node.",
    );
    note(
        names(&|c| matches!(c, CardOutcome::Mismatch { .. })),
        "answered with another machine's card",
        "another machine holds that address, or the roster entry names the wrong one. Its card was not used; re-add the entry with its own tailnet DNS name.",
    );
    note(
        names(&|c| matches!(c, CardOutcome::Unavailable { why: UnavailableWhy::NoCardRoute, .. })),
        "answered with no readable card",
        "they may run an older darkmux. Upgrade darkmux there to see their profiles, seats and state.",
    );
    if let Some(e) = &view.roster_error {
        out.push(format!("! {e}"));
    }
    out
}

/// The whole text view.
pub(crate) fn render_text(view: &FleetView, roster_path: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", style::header("darkmux machine list")));
    out.push_str(&format!("  roster:           {}\n", style::dim(roster_path)));
    out.push_str(&format!(
        "  local machine_id: {}\n",
        style::dim(view.local_machine_id.as_deref().unwrap_or("<unknown>"))
    ));
    if view.gathered_by == GatheredBy::CliProcess {
        out.push_str(&format!(
            "{}\n",
            style::warn("  this machine: not observed: no daemon running, so its seats, thermal state and battery are not shown")
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "{}\n",
        style::dim(&format!("{:<14} {:<5} {:<11} {:<13} {:<12} LOADED", "MACHINE", "LIVE", "AI-HEADROOM", "OS", "VERSION"))
    ));
    for m in &view.machines {
        let [ram, os, version, last] = cells(m);
        let row = format!(
            "{:<14} {:<5} {:<11} {:<13} {:<12} {}",
            darkmux_fleet::truncate_chars(&row_name(view, m), 14),
            liveness_cell(m.liveness),
            darkmux_fleet::truncate_chars(&ram, 11),
            darkmux_fleet::truncate_chars(&os, 13),
            darkmux_fleet::truncate_chars(&version, 12),
            darkmux_fleet::truncate_chars(&last, MODELS_COL_CHARS)
        );
        let reachable = matches!(m.card, CardOutcome::Available { .. } | CardOutcome::Unavailable { .. });
        out.push_str(&format!("{}\n", if reachable { row } else { style::dim(&row) }));
        if let CardOutcome::Available { card, .. } = &m.card {
            for line in detail_lines(card, &m.accepts) {
                out.push_str(&format!("{}\n", style::dim(&line)));
            }
        }
    }
    if view.machines.iter().all(|m| m.is_this_machine) {
        out.push_str("\n(no peers in roster: single-machine fleet)\n");
        out.push_str("Add a peer: darkmux machine add <id> --address <dns-name>\n");
    }
    for line in remedies(view) {
        out.push_str(&format!("{}\n", style::warn(&format!("  {line}"))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_serve::fleet_view::CardSource;

    fn card_json(version: &str) -> serde_json::Value {
        serde_json::json!({
            "card_schema_version": "1.0",
            "work_job_schema_version": "8.0",
            "specs": {
                "darkmux_version": version, "flow_schema_version": "2.0.0", "machine_id": "studio",
                "machine_uid": null, "os": "macos aarch64", "ram_total_bytes": null,
                "ram_free_for_ai_bytes": 64u64 * 1024 * 1024 * 1024, "cpu_brand": null,
                "loaded_models": [{"identifier": "darkmux:qwen", "model": "qwen", "status": "idle", "size": "20 GB", "context": 65536}],
                "lms_unreachable": false, "utility_model": null, "redis_url_redacted": null, "generated_at_ms": 1
            },
            "profiles": [
                {"name": "deep", "description": null, "is_default": true, "endpoint_kind": "managed",
                 "models": [{"id": "qwen", "n_ctx": 65536, "endpoint_kind": "managed"}]},
                {"name": "cloud", "description": null, "is_default": false, "endpoint_kind": "unmanaged",
                 "models": [{"id": "gpt", "n_ctx": null, "endpoint_kind": "unmanaged"}]}
            ],
            "default_profile": "deep", "profiles_error": null,
            "seats": {"busy_policy": "queue", "counts_own_work": false,
                      "local": [{"model": "qwen", "held_by_peer_job": true}, {"model": "small", "held_by_peer_job": false}],
                      "hosted": {"held_by_peer_jobs": 1, "cap": 3}, "waiting": 0},
            "governor": {"thermal": {"state": "nominal", "cpu_speed_limit_pct": 100}, "battery": null,
                         "battery_gate": {"floor_pct": 50, "refuse_start_below_min": true,
                                          "pause_running_below_min": true, "refusing_start": false}},
            "generated_at_ms": 1, "gather_ms": 2, "cache_ttl_ms": 2000
        })
    }

    fn card(version: &str) -> Box<MachineCard> {
        Box::new(serde_json::from_value(card_json(version)).expect("a card of this build's shape"))
    }

    fn read(card: Box<MachineCard>) -> CardOutcome {
        CardOutcome::Available { card, source: CardSource::Listener }
    }

    fn machine(id: &str, liveness: Liveness, outcome: CardOutcome) -> FleetMachine {
        FleetMachine {
            entry: Some(darkmux_serve::wire::RosterMachineEntry {
                id: id.to_string(),
                address: format!("{id}.example.invalid"),
                description: None,
                added_unix_ms: 1,
                machine_uid: None,
                loopback_intended: None,
            }),
            is_this_machine: false,
            machine_uid: None,
            uid_source: None,
            liveness,
            last_beat_ms: None,
            received_at_ms: None,
            fetch_ms: Some(1),
            card: outcome,
            accepts: AcceptsState::Unknown,
        }
    }

    fn with_accepts(mut m: FleetMachine, accepts: AcceptsState) -> FleetMachine {
        m.accepts = accepts;
        m
    }

    fn unreachable(id: &str, reason: UnreachableReason, detail: Option<&str>) -> FleetMachine {
        machine(id, Liveness::Unknown, CardOutcome::Unreachable { reason, detail: detail.map(str::to_string) })
    }

    fn view(machines: Vec<FleetMachine>) -> FleetView {
        FleetView {
            gathered_by: GatheredBy::Daemon,
            local_machine_id: Some("laptop".into()),
            presence: darkmux_serve::source_state::SourceState::Off,
            roster_error: None,
            fetched_at_ms: 1,
            cache_ttl_ms: 0,
            gather_ms: 1,
            machines,
        }
    }

    fn text(v: &FleetView) -> String {
        style::set_colorize_override(Some(false));
        render_text(v, "/roster.json")
    }

    fn own_row() -> FleetMachine {
        let mut m = with_accepts(machine("x", Liveness::Live, CardOutcome::Available { card: card("5.0.0"), source: CardSource::Local }), AcceptsState::ThisMachine);
        m.entry = None;
        m.is_this_machine = true;
        m
    }

    #[test]
    fn a_read_card_fills_the_row_and_the_detail_line() {
        let out = text(&view(vec![machine("studio", Liveness::Live, read(card("5.0.0")))]));
        let row = out.lines().find(|l| l.starts_with("studio")).unwrap();
        for want in ["live", "64 GB", "macos aarch64", "5.0.0", "darkmux:qwen"] {
            assert!(row.contains(want), "{want} missing: {row}");
        }
        assert!(out.contains("profiles: deep (managed), cloud (unmanaged)"), "{out}");
        assert!(out.contains("hosted 1/3") && out.contains("busy policy queue") && out.contains("thermal: nominal"), "{out}");
    }

    /// The seat line says whose jobs it counts, and never says "free": the
    /// card does not know this machine's own work.
    #[test]
    fn the_seat_line_counts_peer_jobs_only_and_never_says_free() {
        let out = text(&view(vec![machine("studio", Liveness::Live, read(card("5.0.0")))]));
        assert!(out.contains("seats (jobs from other machines only): 1 local held"), "{out}");
        assert!(!out.contains("free"), "{out}");
    }

    /// This machine's own row is listed under its own name when the roster has
    /// no entry for it.
    #[test]
    fn an_entryless_own_row_is_listed_under_the_local_machine_name() {
        let out = text(&view(vec![own_row()]));
        assert!(out.lines().any(|l| l.starts_with("laptop ")), "{out}");
        assert!(out.contains("single-machine fleet") && out.contains("darkmux machine add"), "{out}");
    }

    /// The promise: an older peer is "card unavailable", not an error and not
    /// a blank; each reason reads differently; a version from presence says so.
    #[test]
    fn a_peer_with_no_card_reads_card_unavailable_with_its_reason_and_whose_version() {
        let un = |why, v: Option<&str>, src| CardOutcome::Unavailable {
            why,
            peer_version: v.map(str::to_string),
            peer_version_source: src,
        };
        let out = text(&view(vec![
            machine("old", Liveness::Live, un(UnavailableWhy::NoCardRoute, Some("4.9.1"), Some(VersionSource::Presence))),
            machine("older", Liveness::Unknown, un(UnavailableWhy::NoCardRoute, None, None)),
            machine("newer", Liveness::Live, un(UnavailableWhy::OtherSchemaMajor, Some("9.9.9"), Some(VersionSource::Peer))),
            machine("broken", Liveness::Live, un(UnavailableWhy::Unparseable, Some("5.0.0"), Some(VersionSource::Peer))),
        ]));
        assert!(out.contains("card unavailable (peer 4.9.1, from presence; no card route)"), "{out}");
        assert!(out.contains("card unavailable (peer version unknown; no card route)"), "{out}");
        assert!(out.contains("card unavailable (peer 9.9.9; unreadable card schema)"), "{out}");
        assert!(out.contains("card unavailable (peer 5.0.0; its card did not parse)"), "{out}");
        assert!(!out.contains("unreachable"), "an old peer is not unreachable: {out}");
        assert!(out.contains("older darkmux"), "the no-route rows are followed by the remedy: {out}");
    }

    /// No beat is a display column, not a verdict: the machine was asked and
    /// its card is shown.
    #[test]
    fn a_machine_with_no_beat_is_shown_with_its_card_and_is_not_called_gone() {
        let out = text(&view(vec![machine("studio", Liveness::NoBeat, read(card("5.0.0")))]));
        let row = out.lines().find(|l| l.starts_with("studio")).unwrap();
        assert!(row.contains("none") && row.contains("darkmux:qwen"), "{row}");
        assert!(!out.contains("gone") && !out.contains("not asked"), "{out}");
    }

    #[test]
    fn an_unreachable_machine_says_why_with_the_peers_own_words() {
        let out = text(&view(vec![
            unreachable("down", UnreachableReason::ListenerOff, Some("ConnectionFailed")),
            unreachable("auth", UnreachableReason::AuthRequired, None),
            unreachable("odd", UnreachableReason::PinMismatch, None),
            unreachable("noplace", UnreachableReason::RefusedByPeer, Some("studio cannot place you")),
            unreachable("busy", UnreachableReason::ListenerUnavailable, None),
            unreachable("weird", UnreachableReason::Unknown, None),
        ]));
        assert!(out.contains("its fleet listener did not answer (Connection"), "{out}");
        assert!(out.contains("its fleet listener refused this machine (studi"), "{out}");
        assert!(out.contains("its fleet listener cannot serve a card now"), "{out}");
        assert!(out.contains("a reason this darkmux does not know"), "{out}");
        assert!(out.contains("enable `fleet.listener`"), "the listener-off fix is named: {out}");
        assert!(out.contains("DARKMUX_SERVE_TOKEN") && out.contains("pinned tailnet node"), "the fixes are named: {out}");
    }

    /// Each reason a card could not be asked for has its own remedy: a DNS
    /// failure is not a down identity tool is not an address that is not a node.
    #[test]
    fn every_unverified_reason_names_its_own_remedy() {
        let cases = [
            (UnreachableReason::BadAddress, "not a usable address"),
            (UnreachableReason::DnsFailed, "did not resolve"),
            (UnreachableReason::IdentityUnavailable, "identity tool could not verify"),
            (UnreachableReason::NotOnOverlay, "not a node on the network"),
            (UnreachableReason::PinMismatch, "not their pinned tailnet node"),
        ];
        for (reason, want) in cases {
            let out = text(&view(vec![unreachable("m", reason, None)]));
            assert!(out.contains(want), "{reason:?}: {out}");
            for (other, other_want) in cases.iter().filter(|(r, _)| *r != reason) {
                assert!(!out.contains(other_want), "{reason:?} printed {other:?}'s remedy: {out}");
            }
        }
    }

    #[test]
    fn a_card_of_another_machine_is_flagged_and_shows_none_of_its_contents() {
        let out = text(&view(vec![machine("studio", Liveness::Live, CardOutcome::Mismatch { answered_as: Some("mini".into()) })]));
        assert!(out.contains("answered as mini; not this machine's card, not used"), "{out}");
        assert!(out.contains("another machine holds that address"), "{out}");
        assert!(!out.contains("profiles:"), "nothing of its card is shown: {out}");
    }

    /// The three answers about `accepts` read differently, and none reads as
    /// another.
    #[test]
    fn what_a_peer_accepts_reads_as_yes_no_or_not_known() {
        let grant = darkmux_serve::machine_card::CardAccepts {
            peer_name: "laptop".into(),
            profiles: vec!["deep".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        };
        let row = |accepts| view(vec![with_accepts(machine("studio", Liveness::Live, read(card("5.0.0"))), accepts)]);
        let yes = text(&row(AcceptsState::Granted { accepts: grant }));
        assert!(yes.contains("accepts from this machine: profiles deep; roles radio-host"), "{yes}");
        let no = text(&row(AcceptsState::NotListed));
        assert!(no.contains("does not list this machine: it takes no work from it"), "{no}");
        assert!(no.contains("profiles: deep"), "a machine that lists nobody is still shown: {no}");
        let unknown = text(&row(AcceptsState::Unknown));
        assert!(unknown.contains("accepts from this machine: unknown"), "{unknown}");
        assert!(!unknown.contains("does not list"), "unknown is not 'not listed': {unknown}");
    }

    /// A view a CLI gathered itself says what it did not observe.
    #[test]
    fn a_view_gathered_without_a_daemon_says_this_machines_readings_are_not_observed() {
        let mut v = view(vec![machine("studio", Liveness::Live, CardOutcome::Unavailable {
            why: UnavailableWhy::NoCardRoute,
            peer_version: None,
            peer_version_source: None,
        })]);
        assert!(!text(&v).contains("not observed"));
        v.gathered_by = GatheredBy::CliProcess;
        let out = text(&v);
        assert!(out.contains("not observed: no daemon running"), "{out}");
        assert!(out.contains("seats, thermal state and battery"), "{out}");
    }
}
