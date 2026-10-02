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
use darkmux_serve::machine_card::{CardBusyPolicy, CardEndpointKind, CardProfile, CardSeats, MachineCard};
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
        UnreachableReason::PinNotSaved => "its node could not be pinned in the roster; nothing sent",
        UnreachableReason::ListenerOff => "its fleet listener did not answer",
        UnreachableReason::AuthRequired => "did not accept this machine's fleet token",
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
        other => {
            let reason = card_unreadable_reason(other).unwrap_or_default();
            [dash(), dash(), dash(), reason]
        }
    }
}

/// Why no card could be read from a peer, in one clause; `None` for a card
/// that was read. The one place these phrases are worded: the table's last
/// column and `profile list --machine` both print it.
pub(crate) fn card_unreadable_reason(outcome: &CardOutcome) -> Option<String> {
    Some(match outcome {
        CardOutcome::Available { .. } => return None,
        CardOutcome::Unavailable { why, peer_version, peer_version_source } => {
            unavailable_phrase(*why, &peer_words(peer_version.as_deref(), *peer_version_source))
        }
        CardOutcome::Mismatch { answered_as } => {
            format!("answered as {}; not this machine's card, not used", answered_as.as_deref().unwrap_or("no name"))
        }
        CardOutcome::Unreachable { reason, detail } => unreachable_phrase(*reason, detail.as_deref()),
        CardOutcome::Unknown => "an answer this darkmux does not know".to_string(),
    })
}

/// Who is reading a line. The CLI's reader is on the machine that ran the
/// verb, so "this machine" is exact. Radio's grounding goes to a model that
/// may run on ANOTHER machine, where "this machine" would name the wrong one:
/// it says "the user's machine" (where the question was asked) instead, and
/// says which model each profile runs, because a small model reads a profile
/// name as a loaded model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Voice {
    Cli,
    Grounding,
}

impl Voice {
    /// The machine the request comes from, in this reader's words.
    fn requester(self) -> &'static str {
        match self {
            Voice::Cli => "this machine",
            Voice::Grounding => "the user's machine",
        }
    }
}

/// What a peer lets the requesting machine do, in a line's words.
fn accepts_phrase(accepts: &AcceptsState, voice: Voice) -> Option<String> {
    let who = voice.requester();
    Some(match accepts {
        AcceptsState::Granted { accepts } => format!(
            "accepts from {who}: profiles {}; roles {}",
            list_or_none(&accepts.profiles),
            list_or_none(&accepts.roles)
        ),
        AcceptsState::NotListed => format!("does not list {who}: it takes no work from it"),
        AcceptsState::ThisMachine => return None,
        AcceptsState::Unknown => format!("accepts from {who}: unknown"),
    })
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn profiles_line(card: &MachineCard, voice: Voice) -> String {
    let profiles = &card.profiles;
    if let Some(why) = &card.profiles_error {
        return format!("profiles: unreadable ({why})");
    }
    if profiles.is_empty() {
        return "profiles: none".to_string();
    }
    let each: Vec<String> = profiles.iter().map(|p| profile_phrase(p, voice)).collect();
    format!("profiles: {}", each.join(", "))
}

/// One profile: `name (kind)`, and in the grounding voice `name (kind: model,
/// model)` so the model ids the profile runs sit beside its name.
fn profile_phrase(p: &CardProfile, voice: Voice) -> String {
    let kind = kind_word(p.endpoint_kind);
    if voice == Voice::Cli || p.models.is_empty() {
        return format!("{} ({kind})", p.name);
    }
    let ids: Vec<String> = p.models.iter().map(|m| m.id.clone()).collect();
    format!("{} ({kind}: {})", p.name, capped_list(&ids))
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
    if let Some(phrase) = accepts_phrase(accepts, Voice::Cli) {
        lines.push(format!("  {phrase}"));
    }
    lines
}

/// The first dim line: profiles, seats, thermal.
fn detail_line(card: &MachineCard) -> String {
    let mut parts = vec![profiles_line(card, Voice::Cli)];
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
pub(crate) fn row_name(view: &FleetView, m: &FleetMachine) -> String {
    match &m.entry {
        Some(e) => e.id.clone(),
        None => view.local_machine_id.clone().unwrap_or_else(|| "<this machine>".to_string()),
    }
}

/// The reasons a peer is not asked at all, in the order their remedies print.
const UNVERIFIED_REASONS: [UnreachableReason; 6] = [
    UnreachableReason::PinMismatch,
    UnreachableReason::PinNotSaved,
    UnreachableReason::NotOnOverlay,
    UnreachableReason::DnsFailed,
    UnreachableReason::IdentityUnavailable,
    UnreachableReason::BadAddress,
];

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
    for r in UNVERIFIED_REASONS {
        if let Some(fault) = r.target_fault() {
            note(reason(r), "not asked", &format!("the fleet token was not sent. {}", fault.remedy("<id>")));
        }
    }
    note(
        reason(UnreachableReason::ListenerOff),
        "did not answer on their fleet listener",
        "the listener is off or not reachable from here. A machine is visible to the fleet only while its listener runs: enable `fleet.listener` on each (`darkmux doctor` there says why it is not up).",
    );
    note(
        reason(UnreachableReason::AuthRequired),
        "did not accept this machine's fleet token",
        "the token is missing here or is not theirs. Set DARKMUX_SERVE_TOKEN (or the darkmux-serve-token Keychain item) to the shared fleet token, the same value on every machine.",
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

/// The view every fleet reader here uses: the one this machine's own daemon
/// gathered when it answers, else one gathered in this process. The single
/// place that choice is made.
pub(crate) fn local_fleet_view() -> FleetView {
    darkmux_serve::fleet_view::fetch_local_daemon_view(&darkmux_types::config_access::serve_client_addr())
        .unwrap_or_else(darkmux_serve::fleet_view::gather_fleet_view_now)
}

/// The most a machine's profiles, loaded models or granted lists show in the
/// grounding block before "and N more".
const GROUNDING_LIST_MAX: usize = 12;

fn capped_list(items: &[String]) -> String {
    match items.len() {
        0 => "none".to_string(),
        n if n <= GROUNDING_LIST_MAX => items.join(", "),
        n => format!("{}, and {} more", items[..GROUNDING_LIST_MAX].join(", "), n - GROUNDING_LIST_MAX),
    }
}

fn liveness_words(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "live",
        Liveness::NoBeat => "no beat seen",
        Liveness::Unknown => "unknown",
    }
}

/// What the card outcome says, in one clause a model can quote.
fn card_words(outcome: &CardOutcome) -> String {
    match outcome {
        CardOutcome::Available { .. } => "card read".to_string(),
        CardOutcome::Unavailable { why, peer_version, peer_version_source } => {
            unavailable_phrase(*why, &peer_words(peer_version.as_deref(), *peer_version_source))
        }
        CardOutcome::Mismatch { .. } => "answered as another machine; its card was not used".to_string(),
        CardOutcome::Unreachable { reason, .. } => unreachable_phrase(*reason, None),
        CardOutcome::Unknown => "an answer this darkmux does not know".to_string(),
    }
}

/// One machine's lines in the grounding block. Never a machine uid, a node
/// name, an address or a token: only what the card states and what the user
/// can act on.
fn grounding_lines(view: &FleetView, m: &FleetMachine) -> Vec<String> {
    let name = row_name(view, m);
    let here = if m.is_this_machine { " (the user's machine, where the question was asked)" } else { "" };
    let mut lines = vec![format!("- {name}{here}: liveness {}; {}", liveness_words(m.liveness), card_words(&m.card))];
    if let CardOutcome::Available { card, .. } = &m.card {
        let loaded: Vec<String> = card.specs.loaded_models.iter().map(|x| x.identifier.clone()).collect();
        lines.push(format!("  loaded models: {}", capped_list(&loaded)));
        lines.push(format!("  {}", grounding_profiles_line(card, &m.accepts)));
    }
    if let Some(phrase) = accepts_phrase(&m.accepts, Voice::Grounding) {
        lines.push(format!("  {phrase}"));
    }
    lines
}

/// A machine with more profiles than this lists only the ones a question is
/// likely to be about (loaded now, or accepted from the user's machine) plus a
/// count of the rest. It is a readability bound, not a policy knob: at six
/// entries a profiles line stays under about 400 characters, short enough that
/// a small model reads the whole line instead of skimming it. (A 27-profile
/// machine measured about 1,500 characters and crowded out the loaded models.)
const GROUNDING_PROFILES_MAX: usize = 6;

/// The namespace darkmux puts on a model it loaded, dropped for readability.
/// The rest of the id is left exact.
fn bare_model_id(identifier: &str) -> &str {
    identifier.strip_prefix("darkmux:").unwrap_or(identifier)
}

/// The profiles line in the grounding voice. Under [`GROUNDING_PROFILES_MAX`]
/// it is the full list; above it, the profiles running a loaded model or named
/// in the accepts block, then "and N more".
fn grounding_profiles_line(card: &MachineCard, accepts: &AcceptsState) -> String {
    let full = profiles_line(card, Voice::Grounding);
    let profiles = &card.profiles;
    if card.profiles_error.is_some() || profiles.len() <= GROUNDING_PROFILES_MAX {
        return full;
    }
    let loaded: Vec<&str> = card.specs.loaded_models.iter().map(|x| bare_model_id(&x.identifier)).collect();
    let accepted: &[String] = match accepts {
        AcceptsState::Granted { accepts } => &accepts.profiles,
        _ => &[],
    };
    let shown: Vec<String> = profiles
        .iter()
        .filter(|p| accepted.contains(&p.name) || p.models.iter().any(|m| loaded.contains(&m.id.as_str())))
        .map(|p| profile_phrase(p, Voice::Grounding))
        .collect();
    let rest = profiles.len() - shown.len();
    if shown.is_empty() {
        return format!("profiles: {rest} (none runs a model loaded now; names omitted)");
    }
    if rest == 0 {
        return format!("profiles: {}", shown.join(", "));
    }
    format!("profiles (those running a loaded model or accepted from the user's machine): {}, and {rest} more", shown.join(", "))
}

/// The fleet-wide "loaded now" block: one line per model id, naming every
/// machine that has it loaded. Read from the cards' loaded models only, never
/// from profiles, so a small model answers "where is X loaded" from one line.
fn loaded_now_lines(view: &FleetView) -> Vec<String> {
    let mut by_model: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    for m in &view.machines {
        let CardOutcome::Available { card, .. } = &m.card else { continue };
        let name = if m.is_this_machine { format!("{} (the user's machine)", row_name(view, m)) } else { row_name(view, m) };
        for x in &card.specs.loaded_models {
            let hosts = by_model.entry(bare_model_id(&x.identifier)).or_default();
            if !hosts.contains(&name) {
                hosts.push(name.clone());
            }
        }
    }
    let mut lines = vec!["loaded now (model → machines that have it loaded):".to_string()];
    if by_model.is_empty() {
        lines.push("- none".to_string());
    }
    lines.extend(by_model.iter().map(|(model, hosts)| format!("- {model} → {}", hosts.join(", "))));
    lines
}

/// The "fleet" section of radio's grounding: every machine the view lists,
/// from the same [`FleetView`] the daemon serves. Each darkmux term is
/// defined once, before its first use, for a model with no darkmux history.
pub(crate) fn render_grounding(view: &FleetView) -> String {
    let mut out = String::from(
        "A fleet is the user's own machines that run darkmux together. Each line below is one machine, \
         from the card that machine states about itself. \"liveness\" is whether the machine sent a recent \
         presence beat (\"no beat seen\" is not proof it is down). A profile is a named model setup; \
         \"managed\" means the machine loads and serves the model itself, \"unmanaged\" means it only sends \
         requests to a hosted endpoint; the model ids after the colon are the models that profile runs. A profile is not a \
         loaded model. The \"loaded now\" block is the answer to where a model is loaded: it lists, per model, the \
         machines that have it loaded right now; a model that appears only in a profile is not loaded. \"the user's machine\" is the \
         machine where the user asked the question; \"accepts from the user's machine\" is what that machine lets \
         the user's work started there run on it.\n",
    );
    for line in loaded_now_lines(view) {
        out.push_str(&line);
        out.push('\n');
    }
    for m in &view.machines {
        for line in grounding_lines(view, m) {
            out.push_str(&line);
            out.push('\n');
        }
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
        // (#3017) A clock the hub disagrees with, said here and in `doctor`
        // only, past `CLOCK_SKEW_THRESHOLD_MS`; never on a fleet card.
        if let Some(words) = m.clock_skew_ms.and_then(darkmux_flow::presence::clock_skew_words) {
            out.push_str(&format!("{}\n", style::warn(&format!("  {}: {words}", row_name(view, m)))));
        }
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
pub(crate) mod tests {
    use super::*;
    use darkmux_serve::fleet_view::CardSource;

    pub(crate) fn card_json(version: &str) -> serde_json::Value {
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
            "fleet_mode": "peer", "hosts_fleet_redis": false,
            "generated_at_ms": 1, "gather_ms": 2, "cache_ttl_ms": 2000
        })
    }

    pub(crate) fn card(version: &str) -> Box<MachineCard> {
        Box::new(serde_json::from_value(card_json(version)).expect("a card of this build's shape"))
    }

    pub(crate) fn read(card: Box<MachineCard>) -> CardOutcome {
        CardOutcome::Available { card, source: CardSource::Listener }
    }

    pub(crate) fn machine(id: &str, liveness: Liveness, outcome: CardOutcome) -> FleetMachine {
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
            clock_skew_ms: None,
            received_at_ms: None,
            fetch_ms: Some(1),
            card: outcome,
            accepts: AcceptsState::Unknown,
        }
    }

    pub(crate) fn with_accepts(mut m: FleetMachine, accepts: AcceptsState) -> FleetMachine {
        m.accepts = accepts;
        m
    }

    pub(crate) fn unreachable(id: &str, reason: UnreachableReason, detail: Option<&str>) -> FleetMachine {
        machine(id, Liveness::Unknown, CardOutcome::Unreachable { reason, detail: detail.map(str::to_string) })
    }

    pub(crate) fn view(machines: Vec<FleetMachine>) -> FleetView {
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

    pub(crate) fn own_row() -> FleetMachine {
        let mut m = with_accepts(machine("x", Liveness::Live, CardOutcome::Available { card: card("5.0.0"), source: CardSource::Local }), AcceptsState::ThisMachine);
        m.entry = None;
        m.is_this_machine = true;
        m
    }

    /// The grounding block carries what a question about the fleet needs (who
    /// is live, what is loaded, profiles with their endpoint kind, what this
    /// machine may run there) and none of what must never reach a model:
    /// uids, addresses, transport detail.
    #[test]
    fn the_grounding_block_answers_fleet_questions_and_leaks_no_identity() {
        let granted = AcceptsState::Granted {
            accepts: darkmux_serve::machine_card::CardAccepts {
                peer_name: "laptop".into(),
                profiles: vec!["deep".into()],
                roles: vec!["diff-review".into()],
                images: vec![],
                workspace: false,
            },
        };
        let mut studio = with_accepts(machine("studio", Liveness::Live, read(card("5.0.0"))), granted);
        studio.machine_uid = Some("00000000-0000-4000-8000-ABCDEF000001".into());
        let mini = unreachable("mini", UnreachableReason::ListenerOff, Some("connect to 100.64.0.9:8765 refused"));
        let out = render_grounding(&view(vec![own_row(), studio, mini]));

        for want in [
            "- studio: liveness live; card read",
            "loaded models: darkmux:qwen",
            "profiles: deep (managed: qwen), cloud (unmanaged: gpt)",
            "accepts from the user's machine: profiles deep; roles diff-review",
            "- mini: liveness unknown; unreachable: its fleet listener did not answer",
            "(the user's machine, where the question was asked)",
            "\"managed\" means the machine loads and serves the model itself",
            "A profile is not a loaded model",
        ] {
            assert!(out.contains(want), "{want} missing:\n{out}");
        }
        // The answering model may run on another machine: "this machine" is
        // ambiguous there, so the grounding never uses it.
        assert!(!out.contains("this machine"), "ambiguous wording in the grounding:\n{out}");
        for banned in ["ABCDEF000001", "example.invalid", "100.64.0.9", "8765", "token", "Bearer"] {
            assert!(!out.contains(banned), "{banned} leaked:\n{out}");
        }
    }

    /// (#3017) `machine list` says a machine's clock differs from the hub's
    /// only past the threshold, in `doctor`'s wording, and nothing under it.
    #[test]
    fn machine_list_states_a_clock_skew_only_past_the_threshold() {
        let skewed = |ms: i64| {
            let mut m = machine("studio", Liveness::Live, read(card("5.0.0")));
            m.clock_skew_ms = Some(ms);
            text(&view(vec![m]))
        };
        let past = skewed(-10 * 60 * 1000);
        assert!(past.contains("studio: clock 10m behind the hub"), "{past}");
        for under in [0, -5_000, darkmux_flow::presence::CLOCK_SKEW_THRESHOLD_MS as i64] {
            let out = skewed(under);
            assert!(!out.contains("the hub"), "a skew of {under}ms is under the threshold:\n{out}");
        }
    }

    #[test]
    fn a_read_card_fills_the_row_and_the_detail_line() {
        let out = text(&view(vec![machine("studio", Liveness::Live, read(card("5.0.0")))]));
        let row = out.lines().find(|l| l.starts_with("studio")).unwrap();
        for want in ["live", "64 GB", "macos aarch64", "5.0.0", "darkmux:qwen"] {
            assert!(row.contains(want), "{want} missing: {row}");
        }
        assert!(out.contains("profiles: deep (managed), cloud (unmanaged)"), "{out}");
        assert!(!out.contains("managed: qwen"), "the text table keeps its profile line: {out}");
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
        assert!(out.contains("DARKMUX_SERVE_TOKEN") && out.contains("Set DARKMUX_SERVE_TOKEN"), "the fixes are named: {out}");
    }

    /// Each reason a card could not be asked for has its own remedy: a DNS
    /// failure is not a down identity tool is not an address that is not a node.
    #[test]
    fn every_unverified_reason_names_its_own_remedy() {
        let remedy = |r: UnreachableReason| r.target_fault().unwrap().remedy("<id>");
        for reason in UNVERIFIED_REASONS {
            let out = text(&view(vec![unreachable("m", reason, None)]));
            assert!(out.contains(&remedy(reason)), "{reason:?}: {out}");
            for other in UNVERIFIED_REASONS.iter().filter(|r| **r != reason) {
                assert!(!out.contains(&remedy(*other)), "{reason:?} printed {other:?}'s remedy: {out}");
            }
        }
    }

    /// A 401 means the peer did not accept the token this machine sent: it
    /// may be missing or wrong, so the row and its remedy say neither
    /// "not sending" nor blame only one of the two.
    #[test]
    fn a_refused_token_is_not_described_as_a_missing_one() {
        let out = text(&view(vec![unreachable("m", UnreachableReason::AuthRequired, None)]));
        assert!(!out.contains("isn't sending") && !out.contains("is not sending"), "{out}");
        assert!(out.contains("did not accept this machine's fleet token"), "{out}");
        assert!(out.contains("the same value on every machine"), "{out}");
    }

    /// The remedy for a reason that is a `TargetError` is the one
    /// `darkmux-fleet` words for it: the CLI restates none of it.
    #[test]
    fn the_remedy_for_an_unverified_address_is_the_one_the_fleet_crate_words() {
        let cases = [
            (UnreachableReason::BadAddress, darkmux_fleet::TargetFault::BadAddress),
            (UnreachableReason::DnsFailed, darkmux_fleet::TargetFault::DoesNotResolve),
            (UnreachableReason::IdentityUnavailable, darkmux_fleet::TargetFault::IdentityUnavailable),
            (UnreachableReason::NotOnOverlay, darkmux_fleet::TargetFault::NotOnOverlay),
            (UnreachableReason::PinMismatch, darkmux_fleet::TargetFault::PinMismatch),
            (UnreachableReason::PinNotSaved, darkmux_fleet::TargetFault::PinNotSaved),
        ];
        for (reason, fault) in cases {
            assert_eq!(reason.target_fault(), Some(fault));
            let out = text(&view(vec![unreachable("m", reason, None)]));
            assert!(out.contains(&fault.remedy("<id>")), "{reason:?}: {out}");
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

    /// A card with the given loaded model identifiers and (name, model id)
    /// managed profiles.
    pub(crate) fn card_with(loaded: &[&str], profiles: &[(String, &str)]) -> Box<MachineCard> {
        let mut j = card_json("5.0.0");
        j["specs"]["loaded_models"] = loaded
            .iter()
            .map(|id| serde_json::json!({"identifier": id, "model": id, "status": "idle", "size": "1 GB", "context": 4096}))
            .collect();
        j["profiles"] = profiles
            .iter()
            .map(|(name, model)| {
                serde_json::json!({"name": name, "description": null, "is_default": false, "endpoint_kind": "managed",
                    "models": [{"id": model, "n_ctx": 4096, "endpoint_kind": "managed"}]})
            })
            .collect();
        Box::new(serde_json::from_value(j).expect("a card of this build's shape"))
    }

    fn many_profiles(n: usize) -> Vec<(String, &'static str)> {
        (0..n).map(|i| (format!("profile-{i:02}"), "qwen/other-model")).collect()
    }

    pub(crate) fn granted(profiles: &[&str]) -> AcceptsState {
        AcceptsState::Granted {
            accepts: darkmux_serve::machine_card::CardAccepts {
                peer_name: "laptop".into(),
                profiles: profiles.iter().map(|p| p.to_string()).collect(),
                roles: vec![],
                images: vec![],
                workspace: false,
            },
        }
    }

    /// The shape that misled two small models: the Studio has devstral loaded,
    /// the MacBook has it only in a profile (among many).
    fn two_machine_view() -> FleetView {
        let mut own = own_row();
        let mut profiles = many_profiles(26);
        profiles.push(("diff-review".into(), "mistralai/devstral-small-2-2512"));
        own.card = read(card_with(&["darkmux:qwen3-4b-instruct-2507"], &profiles));
        let studio = with_accepts(
            machine(
                "m1-max-32gb-studio",
                Liveness::Unknown,
                read(card_with(
                    &["darkmux:mistralai/devstral-small-2-2512", "darkmux:qwen/qwen3-4b-2507"],
                    &[("diff-review".into(), "mistralai/devstral-small-2-2512")],
                )),
            ),
            granted(&["diff-review"]),
        );
        view(vec![own, studio])
    }

    /// The loaded-now block: its header and the lines under it, up to the
    /// first per-machine line.
    fn loaded_block(out: &str) -> String {
        let lines = out.lines().skip_while(|l| !l.starts_with("loaded now (")).take_while(|l| !l.contains(": liveness"));
        lines.collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn loaded_now_names_only_the_machine_that_has_the_model_loaded() {
        let out = render_grounding(&two_machine_view());
        let block = loaded_block(&out);
        let devstral: Vec<&str> = block.lines().filter(|l| l.contains("devstral")).collect();
        assert_eq!(devstral, ["- mistralai/devstral-small-2-2512 → m1-max-32gb-studio"], "{block}");
        assert!(block.contains("- qwen3-4b-instruct-2507 → laptop (the user's machine)"), "{block}");
        assert!(!block.contains("darkmux:"), "namespace prefix not stripped:\n{block}");
    }

    #[test]
    fn loaded_now_says_none_when_nothing_is_loaded() {
        let mut own = own_row();
        own.card = read(card_with(&[], &[]));
        let out = render_grounding(&view(vec![own]));
        assert!(out.contains("loaded now (model → machines that have it loaded):\n- none\n"), "{out}");
    }

    #[test]
    fn a_long_profile_list_is_shortened_but_never_drops_an_accepted_profile() {
        let out = render_grounding(&two_machine_view());
        // Own row: 27 profiles and none runs a model loaded there.
        assert!(out.contains("  profiles: 27 (none runs a model loaded now; names omitted)"), "{out}");
        assert!(!out.contains("profile-00"), "{out}");

        // One profile runs a loaded model: it is listed, the rest counted.
        let mut ps = many_profiles(12);
        ps.push(("live".into(), "qwen/loaded-one"));
        let m = machine("peer", Liveness::Live, read(card_with(&["darkmux:qwen/loaded-one"], &ps)));
        let out = render_grounding(&view(vec![m]));
        assert!(out.contains("live (managed: qwen/loaded-one), and 12 more"), "{out}");

        // A profile named in accepts is listed even when it runs nothing loaded.
        let mut m = with_accepts(machine("peer", Liveness::Live, read(card_with(&[], &many_profiles(10)))), granted(&["profile-07"]));
        m.entry = None;
        let out = render_grounding(&view(vec![m]));
        assert!(out.contains("profile-07 (managed: qwen/other-model), and 9 more"), "{out}");
    }

    #[test]
    fn a_short_profile_list_and_the_cli_table_are_unchanged() {
        let out = render_grounding(&view(vec![with_accepts(machine("s", Liveness::Live, read(card("5.0.0"))), granted(&[]))]));
        assert!(out.contains("  profiles: deep (managed: qwen), cloud (unmanaged: gpt)"), "{out}");
        let long = card_with(&[], &many_profiles(30));
        let cli = profiles_line(&long, Voice::Cli);
        assert_eq!(cli.matches("(managed)").count(), 30, "{cli}");
        assert!(!cli.contains("more"), "{cli}");
    }

    #[test]
    fn the_loaded_now_block_survives_the_fleet_cap_ahead_of_per_machine_detail() {
        // Twenty peers, each accepting ten profiles: per-machine detail alone
        // is far past the cap.
        let names: Vec<String> = (0..10).map(|i| format!("profile-{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut machines = vec![own_row()];
        machines[0].card = read(card_with(&["darkmux:qwen3-4b-instruct-2507"], &many_profiles(10)));
        machines.push(machine("studio", Liveness::Live, read(card_with(&["darkmux:mistralai/devstral-small-2-2512"], &many_profiles(10)))));
        for i in 0..20 {
            machines.push(with_accepts(machine(&format!("peer-{i:02}"), Liveness::Live, read(card_with(&[], &many_profiles(10)))), granted(&names)));
        }
        let full = render_grounding(&view(machines));
        let out = crate::radio_answer::truncate_chars(&full, crate::radio_answer::FLEET_CAP_CHARS);
        assert!(out.contains("- mistralai/devstral-small-2-2512 → studio\n"), "{out}");
        assert!(full.chars().count() > crate::radio_answer::FLEET_CAP_CHARS, "the fixture must exceed the cap to prove anything");
        assert!(out.contains("- qwen3-4b-instruct-2507 → laptop (the user's machine)"), "{out}");
    }

    /// Writes the rendered section for the two-machine shape so it can be
    /// replayed across models; a no-op unless the path variable is set.
    #[test]
    fn emit_two_machine_fleet_section() {
        if let Ok(path) = std::env::var("DARKMUX_TEST_FLEET_SECTION_OUT") {
            std::fs::write(path, render_grounding(&two_machine_view())).expect("write section");
        }
    }
}
