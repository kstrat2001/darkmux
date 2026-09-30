//! `darkmux machine list`: the fleet view, printed.
//!
//! The verb gathers the same view every daemon serves at `GET /fleet/view`
//! (`darkmux_serve::fleet_view`): one row per roster machine, each with the
//! card that machine states about itself, fetched in parallel over the
//! verified peer path. Under `--json` it prints the `FleetView` itself.
//!
//! This machine's row is built locally with no HTTP, so its seat block is
//! absent (the seats are the running daemon's; see `MachineCard::seats`).

use darkmux_serve::fleet_view::{CardOutcome, FleetMachine, FleetView, Liveness, UnreachableReason};
use darkmux_serve::machine_card::{CardEndpointKind, CardSeats, MachineCard};
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
        Liveness::Gone => "gone",
        Liveness::Unknown => "?",
    }
}

fn kind_word(k: CardEndpointKind) -> &'static str {
    match k {
        CardEndpointKind::Managed => "managed",
        CardEndpointKind::Unmanaged => "unmanaged",
        CardEndpointKind::Mixed => "mixed",
        CardEndpointKind::Unresolved => "unresolved",
    }
}

fn unreachable_phrase(reason: UnreachableReason, detail: Option<&str>) -> String {
    let base = match reason {
        UnreachableReason::PresenceGone => "presence reports it gone; not asked",
        UnreachableReason::Unverified => "address is not its pinned node; nothing sent",
        UnreachableReason::AuthRequired => "needs a fleet token this machine is not sending",
        UnreachableReason::ConnectFailed => "could not connect",
        UnreachableReason::BadAnswer => "answered with something that is not a card",
    };
    match detail {
        Some(d) => format!("unreachable: {base} ({d})"),
        None => format!("unreachable: {base}"),
    }
}

/// The table cells of one machine: headroom, OS, version, and the last
/// column (loaded models, or why there is no card).
fn cells(m: &FleetMachine) -> [String; 4] {
    let dash = || "-".to_string();
    match &m.card {
        CardOutcome::Available { card } => {
            let s = &card.specs;
            let models = s.loaded_models.iter().map(|x| x.identifier.as_str()).collect::<Vec<_>>().join(", ");
            [
                s.ram_free_for_ai_bytes.map(human_gb).unwrap_or_else(dash),
                s.os.clone(),
                s.darkmux_version.clone(),
                if models.is_empty() { dash() } else { models },
            ]
        }
        CardOutcome::Unavailable { peer_version } => [
            dash(),
            dash(),
            peer_version.clone().unwrap_or_else(dash),
            format!("card unavailable (peer {})", peer_version.as_deref().unwrap_or("version unknown")),
        ],
        CardOutcome::Unreachable { reason, detail } => {
            [dash(), dash(), dash(), unreachable_phrase(*reason, detail.as_deref())]
        }
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

fn seats_line(seats: &CardSeats) -> String {
    let hosted = match seats.hosted.cap {
        Some(cap) => format!("{}/{cap}", seats.hosted.held),
        None => format!("{}/unbounded", seats.hosted.held),
    };
    format!(
        "seats: {} held, hosted {hosted}, {} waiting, busy policy {}",
        seats.local.held_models.len(),
        seats.waiting,
        darkmux_types::config_enum::ConfigEnum::token(seats.busy_policy)
    )
}

/// The dim line under a machine whose card was read: profiles, seats, thermal.
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

fn remedies(view: &FleetView) -> Vec<String> {
    let ids = |want: fn(&CardOutcome) -> bool| -> Vec<&str> {
        view.machines.iter().filter(|m| want(&m.card)).map(|m| m.entry.id.as_str()).collect()
    };
    let unverified = ids(|c| matches!(c, CardOutcome::Unreachable { reason: UnreachableReason::Unverified, .. }));
    let auth = ids(|c| matches!(c, CardOutcome::Unreachable { reason: UnreachableReason::AuthRequired, .. }));
    let old = ids(|c| matches!(c, CardOutcome::Unavailable { .. }));
    let mut out = Vec::new();
    if !unverified.is_empty() {
        out.push(format!(
            "! {} machine(s) not asked ({}): the address is not their pinned tailnet node, so the fleet token \
             was not sent. Re-add each by its tailnet DNS name (`darkmux machine add <id> --address <dns-name>`).",
            unverified.len(),
            unverified.join(", ")
        ));
    }
    if !auth.is_empty() {
        out.push(format!(
            "! {} machine(s) require a fleet token this machine isn't sending ({}). Set DARKMUX_SERVE_TOKEN \
             (or the darkmux-serve-token Keychain item) to the shared fleet token.",
            auth.len(),
            auth.join(", ")
        ));
    }
    if !old.is_empty() {
        out.push(format!(
            "! {} machine(s) answered with no readable card ({}): they may run an older darkmux. Upgrade \
             darkmux there to see their profiles, seats and state.",
            old.len(),
            old.join(", ")
        ));
    }
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
        "  local machine_id: {}\n\n",
        style::dim(view.local_machine_id.as_deref().unwrap_or("<unknown>"))
    ));
    if view.machines.is_empty() {
        out.push_str("(no peers in roster: single-machine fleet)\n\n");
        out.push_str("Add a peer: darkmux machine add <id> --address <dns-name>\n");
        return out;
    }
    out.push_str(&format!(
        "{}\n",
        style::dim(&format!("{:<14} {:<5} {:<11} {:<13} {:<12} LOADED", "MACHINE", "LIVE", "AI-HEADROOM", "OS", "VERSION"))
    ));
    for m in &view.machines {
        let [ram, os, version, last] = cells(m);
        let row = format!(
            "{:<14} {:<5} {:<11} {:<13} {:<12} {}",
            darkmux_fleet::truncate_chars(&m.entry.id, 14),
            liveness_cell(m.liveness),
            darkmux_fleet::truncate_chars(&ram, 11),
            darkmux_fleet::truncate_chars(&os, 13),
            darkmux_fleet::truncate_chars(&version, 12),
            darkmux_fleet::truncate_chars(&last, MODELS_COL_CHARS)
        );
        let reachable = matches!(m.card, CardOutcome::Available { .. } | CardOutcome::Unavailable { .. });
        out.push_str(&format!("{}\n", if reachable { row } else { style::dim(&row) }));
        if let CardOutcome::Available { card } = &m.card {
            out.push_str(&format!("{}\n", style::dim(&detail_line(card))));
        }
    }
    for line in remedies(view) {
        out.push_str(&format!("{}\n", style::warn(&format!("  {line}"))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card_json(version: &str) -> serde_json::Value {
        serde_json::json!({
            "card_schema_version": "1.0",
            "work_job_schema_version": "8",
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
            "seats": {"busy_policy": "queue", "local": {"held_models": ["qwen"], "free_models": []},
                      "hosted": {"held": 1, "cap": 3, "free": 2}, "waiting": 0},
            "governor": {"thermal": {"state": "nominal", "cpu_speed_limit_pct": 100}, "battery": null,
                         "battery_gate": {"floor_pct": 50, "refuse_start_below_min": true,
                                          "pause_running_below_min": true, "refusing_start": false}},
            "generated_at_ms": 1, "gather_ms": 2
        })
    }

    fn card(version: &str) -> Box<MachineCard> {
        Box::new(serde_json::from_value(card_json(version)).expect("a card of this build's shape"))
    }

    fn machine(id: &str, liveness: Liveness, outcome: CardOutcome) -> FleetMachine {
        FleetMachine {
            entry: darkmux_serve::wire::RosterMachineEntry {
                id: id.to_string(),
                address: format!("{id}.example.invalid"),
                description: None,
                added_unix_ms: 1,
                machine_uid: None,
                loopback_intended: None,
            },
            is_this_machine: false,
            liveness,
            last_beat_ms: None,
            card: outcome,
        }
    }

    fn view(machines: Vec<FleetMachine>) -> FleetView {
        FleetView {
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

    #[test]
    fn a_read_card_fills_the_row_and_the_detail_line() {
        let out = text(&view(vec![machine("studio", Liveness::Live, CardOutcome::Available { card: card("5.0.0") })]));
        let row = out.lines().find(|l| l.starts_with("studio")).unwrap();
        for want in ["live", "64 GB", "macos aarch64", "5.0.0", "darkmux:qwen"] {
            assert!(row.contains(want), "{want} missing: {row}");
        }
        assert!(out.contains("profiles: deep (managed), cloud (unmanaged)"), "{out}");
        assert!(out.contains("hosted 1/3") && out.contains("busy policy queue") && out.contains("thermal: nominal"), "{out}");
    }

    /// The promise: an older peer is "card unavailable (peer <version>)", not
    /// an error and not a blank.
    #[test]
    fn a_peer_with_no_card_reads_card_unavailable_naming_its_version() {
        let out = text(&view(vec![
            machine("old", Liveness::Live, CardOutcome::Unavailable { peer_version: Some("4.9.1".into()) }),
            machine("older", Liveness::Unknown, CardOutcome::Unavailable { peer_version: None }),
        ]));
        assert!(out.contains("card unavailable (peer 4.9.1)"), "{out}");
        assert!(out.contains("card unavailable (peer version unknown)"), "{out}");
        assert!(!out.contains("unreachable"), "an old peer is not unreachable: {out}");
        assert!(out.contains("older darkmux"), "the row is followed by the remedy: {out}");
    }

    #[test]
    fn an_unreachable_machine_says_why_and_a_gone_one_says_it_was_not_asked() {
        let out = text(&view(vec![
            machine("gone", Liveness::Gone, CardOutcome::Unreachable { reason: UnreachableReason::PresenceGone, detail: None }),
            machine(
                "down",
                Liveness::Unknown,
                CardOutcome::Unreachable { reason: UnreachableReason::ConnectFailed, detail: Some("ConnectionFailed".into()) },
            ),
            machine("auth", Liveness::Live, CardOutcome::Unreachable { reason: UnreachableReason::AuthRequired, detail: None }),
            machine("odd", Liveness::Live, CardOutcome::Unreachable { reason: UnreachableReason::Unverified, detail: None }),
        ]));
        assert!(out.contains("presence reports it gone; not asked"), "{out}");
        assert!(out.contains("could not connect (ConnectionFailed)"), "{out}");
        assert!(out.contains("DARKMUX_SERVE_TOKEN") && out.contains("pinned tailnet node"), "the fixes are named: {out}");
    }

    #[test]
    fn an_empty_roster_says_it_is_a_single_machine_fleet() {
        let out = text(&view(vec![]));
        assert!(out.contains("single-machine fleet") && out.contains("darkmux machine add"), "{out}");
    }
}
