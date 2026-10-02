//! `darkmux machine` roster-facing command handlers (#1426 — the retired
//! `fleet add`/`remove`/`status` folded into the machine family: `machine
//! add`/`machine remove`/`machine list`). The `MachineCmd` arg surface lives
//! in `cli.rs`; `cmd_machine` in `main.rs` routes the roster sub-verbs here.
//! The "fleet" concept survives (roster, `fleet.mode`, flow records) — only
//! the CLI family moved under `machine`.

use anyhow::Result;

use crate::cli_json;
use crate::fleet;
use crate::flow;

/// `darkmux machine add` — register (or update) a roster entry.
///
/// (#2924) **The entry's id is the machine's `machine_id`.** That one name is
/// what flow records carry, what presence beats carry as `display_name`, and
/// what #2916's `profile@machine` addresses will resolve. So an entry is
/// THIS machine's own entry exactly when `id` equals this machine's resolved
/// machine_id, and only then is this host's hardware uid (#2768) stored on
/// it. That used to be decided by the address instead (a loopback address
/// meant "self"), which is what put `127.0.0.1:8765` in the Studio's roster.
///
/// (#2924) **A loopback address is refused** unless `allow_loopback`. A
/// roster entry is read by other machines — the daemon serves the roster to
/// every viewer on the tailnet, and fleet routing dials it — and a loopback
/// address reaches whichever machine reads it, never the one the entry
/// describes. `--allow-loopback` exists for several daemons on one host (a
/// same-host test fleet), where loopback really does reach the peer.
pub(crate) fn cmd_machine_add(
    id: &str,
    address: &str,
    description: Option<&str>,
    allow_loopback: bool,
) -> Result<i32> {
    if let Some(msg) = loopback_refusal(id, address, allow_loopback) {
        eprintln!("{msg}");
        return Ok(2);
    }
    // (#2947 review C1) `machine add` pins the new entry through the
    // identity provider; a bad `fleet.identity.provider` refuses before the
    // roster is written, not after.
    darkmux_types::config_enum::preflight(darkmux_types::config_enum::Scope::FleetSubmission)?;
    // (#2916 re-review C7) Machine names are case-insensitive: an entry that
    // differs only in case is the SAME machine, updated under its existing
    // spelling rather than added twice.
    let requested = id;
    let existing_key = fleet::find_machine_key(&fleet::load_roster()?, requested)?;
    let id_owned = existing_key.clone().unwrap_or_else(|| requested.to_string());
    let id = id_owned.as_str();
    if id != requested {
        println!("machine: `{requested}` is the existing entry `{id}` (machine names are case-insensitive)");
    }
    let local_id = flow::resolve_machine_id();
    let is_self_entry = local_id.as_deref().is_some_and(|l| fleet::same_machine(l, id));
    // A peer's hardware cannot be probed from here — `machine add` performs no
    // network call — so a non-self entry passes `None`, which `add_machine`
    // treats as "keep whatever the entry already had" (see its own doc).
    let uid = if is_self_entry {
        darkmux_hardware::machine_uid()
    } else {
        None
    };
    let loopback_intended = allow_loopback && fleet::address_host_is_loopback(address);
    let was_present = fleet::mutate_roster(|roster| {
        let was_present = roster.machines.contains_key(id);
        fleet::add_machine(roster, id, address, description, uid)?;
        if let Some(entry) = roster.machines.get_mut(id) {
            entry.loopback_intended = loopback_intended;
        }
        Ok(was_present)
    })?;
    let verb = if was_present { "updated" } else { "added" };
    println!("machine: {verb} {id} (address={address})");
    // (#2916 review C1) Pin the node at the address, so work submission can
    // check it before sending the fleet token; warn when the address is not
    // a node on the overlay at all.
    if !loopback_intended {
        let pin = match fleet::configured_provider() {
            Ok(p) => pin_address(p.as_ref(), address),
            Err(e) => PinOutcome::Unverified(format!("{e:#}")),
        };
        match pin {
            PinOutcome::Pinned { node_id, shown } => {
                fleet::mutate_roster(|r| {
                    if let Some(e) = r.machines.get_mut(id) {
                        e.node_id = Some(node_id.clone());
                    }
                    Ok(())
                })?;
                println!("  pinned to the network node `{shown}`; work sent to {id} checks it every time");
            }
            PinOutcome::NotANode => eprintln!(
                "{}",
                darkmux_types::style::warn(&format!(
                    "machine: `{address}` is not a node on the tailnet. Fleet work refuses to go there \
                     (the fleet token would cross a network nobody vouches for); use {id}'s tailnet DNS name."
                ))
            ),
            PinOutcome::Unverified(why) => println!(
                "  not pinned yet ({why}); the first work sent to {id} pins its node"
            ),
        }
    }
    if let Some(d) = description {
        println!("  description: {d}");
    }
    if is_self_entry {
        let recorded = if uid.is_some() { "; hardware identity recorded" } else { "" };
        println!("  this machine (its machine_id is `{id}`){recorded}");
    }
    println!("  roster: {}", fleet::roster_path().display());
    Ok(0)
}

/// (#2916 review C1) What `machine add` learned about the node at an
/// address.
#[derive(Debug, PartialEq, Eq)]
enum PinOutcome {
    Pinned { node_id: String, shown: String },
    NotANode,
    Unverified(String),
}

fn pin_address(provider: &dyn fleet::IdentityProvider, address: &str) -> PinOutcome {
    let Some(ip) = fleet::resolve_host_addrs(address).first().copied() else {
        return PinOutcome::Unverified("the address does not resolve from here".into());
    };
    match provider.identify(ip) {
        Ok(Some(n)) => PinOutcome::Pinned { shown: n.dns_name.clone().unwrap_or(n.name.clone()), node_id: n.node_id },
        Ok(None) => PinOutcome::NotANode,
        Err(e) => PinOutcome::Unverified(format!("{} could not answer: {e:#}", provider.provider_name())),
    }
}

/// (#2924) The refusal `machine add` prints for a loopback address, or `None`
/// when the address is acceptable. Pure so the rule is testable apart from
/// the verb; the verb's own test proves the call site honors it.
fn loopback_refusal(id: &str, address: &str, allow_loopback: bool) -> Option<String> {
    if allow_loopback || !fleet::address_host_is_loopback(address) {
        return None;
    }
    Some(format!(
        "machine: refusing loopback address `{address}` for `{id}`. A roster entry is read by other \
         machines (the daemon serves the roster to every viewer, and fleet routing dials it), and a \
         loopback address reaches whichever machine reads it, never `{id}`.\n  \
         Use the machine's tailnet DNS name: `darkmux machine add {id} --address <tailnet-dns-name>` \
         (`tailscale status` on that machine prints it).\n  \
         Several daemons on ONE host (a same-host test fleet)? Pass --allow-loopback."
    ))
}

/// (#2924) The fleet-roster rows `darkmux doctor` appends: `roster
/// addresses` (a loopback address no peer can use) and `roster identity`
/// (an entry not named by its machine's machine_id). Nothing when there is
/// no roster — a single machine has no fleet to check.
///
/// Gathers what this machine knows about fleet identity from three sources,
/// in rising precedence for "what does uid X go by now": local flow history
/// (latest name wins), live presence beats, and this machine's own
/// resolution. Presence is read only when Redis is configured, with the
/// same bounded connect every presence read uses.
pub(crate) fn roster_doctor_checks() -> Vec<crate::doctor::Check> {
    let roster = match fleet::load_roster() {
        Ok(r) => r,
        Err(e) => {
            return vec![crate::doctor::Check {
                name: "roster identity".into(),
                status: crate::doctor::Status::Warn,
                message: format!("the fleet roster could not be read: {e:#}"),
                hint: Some("Fix the JSON in the roster file named above; `darkmux machine list` reads the same file.".into()),
            }];
        }
    };
    if roster.machines.is_empty() {
        return Vec::new();
    }
    let (beats, presence) = match darkmux_flow::redis_url() {
        None => (Vec::new(), crate::doctor::PresenceState::NotConfigured),
        Some(url) => match redis::Client::open(url.expose_for_probe())
            .ok()
            .and_then(|client| darkmux_flow::presence::read_live(&client).ok())
        {
            Some(beats) => (
                beats.into_iter().map(|b| (b.machine_uid, b.display_name)).collect(),
                crate::doctor::PresenceState::Read,
            ),
            None => (Vec::new(), crate::doctor::PresenceState::Unreadable),
        },
    };
    let resolved = darkmux_flow::resolve_machine_id_with_source();
    let live = LiveIdentity {
        beats,
        local_uid: darkmux_hardware::machine_uid().map(str::to_string),
        local_name_from_env: matches!(resolved, Some((_, darkmux_flow::MachineIdSource::Env))),
        local_name: resolved.map(|(id, _)| id),
        presence,
    };
    // Flow history is read only when this machine and the live beats cannot
    // settle every entry on their own — the normal, healthy fleet never pays
    // for the scan.
    let live_only = gather_identity_knowledge(None, &live);
    let known = if roster_needs_history(&roster, &live_only) {
        gather_identity_knowledge(Some(&darkmux_types::config_access::flows_dir()), &live)
    } else {
        live_only
    };
    roster_checks(&roster, &known)
}

// (#2916 stage 2) `LiveIdentity`, `record_identity`, `gather_identity_knowledge`,
// `roster_needs_history` and `ROSTER_HISTORY_FILES` moved to
// `darkmux_fleet::identity_knowledge`, re-exported at the crate root.
use fleet::{gather_identity_knowledge, roster_needs_history, LiveIdentity};
#[cfg(test)]
use fleet::{record_identity, ROSTER_HISTORY_FILES};

/// The pure half of [`roster_doctor_checks`]: roster + knowledge in, rows out.
fn roster_checks(
    roster: &fleet::FleetRoster,
    known: &crate::doctor::FleetIdentityKnowledge,
) -> Vec<crate::doctor::Check> {
    let views: Vec<crate::doctor::RosterEntryView> = roster
        .machines
        .values()
        .map(|m| crate::doctor::RosterEntryView {
            id: m.id.clone(),
            machine_uid: m.machine_uid.clone(),
            current_name: m.current_name.clone(),
            address: m.address.clone(),
            address_is_loopback: fleet::address_host_is_loopback(&m.address),
            loopback_intended: m.loopback_intended,
        })
        .collect();
    vec![
        crate::doctor::check_roster_addresses(&views, known),
        crate::doctor::check_roster_identity(&views, known),
    ]
}

pub(crate) fn cmd_machine_remove(id: &str) -> Result<i32> {
    let removed = fleet::mutate_roster(|roster| {
        let key = fleet::find_machine_key(roster, id)?.unwrap_or_else(|| id.to_string());
        Ok(fleet::remove_machine(roster, &key))
    })?;
    match removed {
        Some(entry) => {
            println!("machine: removed {id} (address was {})", entry.address);
            println!("  roster: {}", fleet::roster_path().display());
            Ok(0)
        }
        None => {
            eprintln!("machine: no machine `{id}` in roster — nothing to remove");
            Ok(2)
        }
    }
}

/// What a peer's body says about its own versions: the `schema_version` and
/// `darkmux_version` strings it carries, when it carries them. Read from the
/// raw body, so it works when the body does not parse into a type this darkmux
/// knows.
fn peer_versions(body: &serde_json::Value) -> Option<String> {
    let named: Vec<String> = ["schema_version", "darkmux_version"]
        .into_iter()
        .filter_map(|key| body.get(key).and_then(serde_json::Value::as_str).map(|v| format!("{key} {v}")))
        .collect();
    (!named.is_empty()).then(|| named.join(", "))
}

/// The peer's versions as a sentence fragment, for a message that names them.
pub(crate) fn peer_reports(body: &serde_json::Value) -> String {
    peer_versions(body)
        .map_or_else(|| "the peer reports no schema_version or darkmux_version".to_string(), |v| format!("the peer reports {v}"))
}

/// Why a peer's `/machine/resources` body did not read: what the peer says it
/// is, what this darkmux reads, and the parse error. It claims no version
/// mismatch it did not observe: a body that names the schema this darkmux
/// reads and still fails is reported as malformed, not as another version.
fn unreadable_resources_reason(body: &serde_json::Value, err: &serde_json::Error) -> String {
    let local = darkmux_profiles::model_ledger::LEDGER_SCHEMA_VERSION;
    let peer = peer_reports(body);
    let same_schema = body.get("schema_version").and_then(serde_json::Value::as_str) == Some(local);
    let verdict = if same_schema {
        format!("the schema matches, so the body is malformed: {err}")
    } else {
        format!("this darkmux reads ledger schema {local}: {err}")
    };
    format!("{peer}; {verdict}")
}

/// What `machine resources <peer>` prints for the peer's `/machine/resources`
/// body. It is read as the daemon's own `MachineResourcesResponse`, so the
/// recorded cadence (`cache_ttl_ms`) and the sampler's `load` reach the
/// output. A body this darkmux cannot read is a refusal under `--json` (a
/// script needs the failure) and a note in text mode (an operator needs to
/// know which peer and which version, not a bare error).
pub(crate) fn peer_resources_view(id: &str, body: serde_json::Value, json: bool) -> Result<String> {
    match serde_json::from_value::<darkmux_serve::wire::MachineResourcesResponse>(body.clone()) {
        Ok(resp) if json => cli_json::render(&resp),
        Ok(resp) => Ok(darkmux_profiles::model_ledger::render_human(&resp.ledger)),
        Err(e) => {
            let reason = unreadable_resources_reason(&body, &e);
            if json {
                anyhow::bail!("machine `{id}` answered with a resource ledger this darkmux does not read ({reason})")
            }
            Ok(format!("machine `{id}`: resources unreadable. Nothing from the peer's ledger could be shown ({reason}).\n"))
        }
    }
}

/// Resolve a roster `id` and GET `path` from its daemon with the shared
/// fleet token (#1426, #881). Used by `machine status [id]` / `machine
/// resources [id]`. (#2916 re-review MUST 3) The request goes through
/// `darkmux_fleet::peer_target` + `fleet_get`, the one place the token is
/// attached: a peer's address must resolve to its pinned tailnet node
/// (this machine's own entry and loopback entries excepted), and it is
/// dialed at that verified address. Every string in the answer is
/// sanitized before anything prints it (MUST 4). Reads only.
pub(crate) fn fetch_peer_json(id: &str, path: &str) -> Result<serde_json::Value> {
    let roster = fleet::load_roster()?;
    let entry = fleet::find_machine(&roster, id)?.cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "no machine `{id}` in roster — add it with `darkmux machine add {id} --address <dns-name>`, \
             or omit the id to read this host"
        )
    })?;
    let local_id = flow::resolve_machine_id();
    let dialed = dial_address(&entry, local_id.as_deref(), &darkmux_types::config_access::serve_client_addr());
    let target = peer_target_for(&entry, local_id.as_deref())?;
    let url = format!("{}{path}", target.base());
    match fleet::fleet_get(&target, path, std::time::Duration::from_millis(2000), &[]) {
        Ok(resp) => {
            let body = resp
                .into_string()
                .map_err(|e| anyhow::anyhow!("reading response from `{id}` ({url}): {e}"))?;
            let mut v: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| anyhow::anyhow!("parsing JSON from `{id}` ({url}): {e}"))?;
            // (#2916 round 3 MUST) Its fields are rendered as table cells and
            // one-line fields: single-line and bounded.
            fleet::sanitize_remote_json_lines(&mut v, PEER_FIELD_MAX_CHARS);
            Ok(v)
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => anyhow::bail!(
            "peer `{id}` requires a bearer token this machine isn't sending. Set DARKMUX_SERVE_TOKEN \
             (or the darkmux-serve-token Keychain item) to the shared fleet token."
        ),
        // A 404 means the peer IS reachable — its daemon just doesn't serve
        // this route. "Could not reach" would be the wrong vocabulary.
        Err(ureq::Error::Status(404, _)) => {
            anyhow::bail!(route_missing_message(id, path, &dialed))
        }
        Err(e) => anyhow::bail!("could not reach `{id}` ({url}): {e}"),
    }
}

/// The longest single field a peer's payload may carry into a render.
const PEER_FIELD_MAX_CHARS: usize = 80;

/// (#2916 re-review MUST 3) Where a token-bearing read of roster entry
/// `entry` may go: this machine's own daemon (loopback) for its own entry,
/// a loopback entry as written, else the verified, pinned tailnet node.
/// A first-contact pin is persisted.
fn peer_target_for(entry: &fleet::MachineEntry, local_id: Option<&str>) -> Result<fleet::SettledTarget> {
    let provider = fleet::configured_provider_or_unavailable();
    peer_target_with(entry, local_id, provider.as_ref())
}

/// [`peer_target_for`] with the identity provider supplied by the caller.
fn peer_target_with(
    entry: &fleet::MachineEntry,
    local_id: Option<&str>,
    provider: &dyn fleet::IdentityProvider,
) -> Result<fleet::SettledTarget> {
    let is_self = local_id.is_some_and(|l| fleet::same_machine(l, &entry.id)) && !fleet::address_host_is_loopback(&entry.address);
    let local_addr = is_self.then(darkmux_types::config_access::serve_client_addr);
    let target = fleet::peer_target(
        &entry.id,
        entry,
        local_addr.as_deref(),
        crate::serve::DEFAULT_DAEMON_PORT,
        true,
        provider,
    )?;
    fleet::pin_on_first_contact(target, entry, provider)
}

/// (#2924 MF-3) The address to dial for a roster entry. This machine's own
/// entry (its id is this machine's machine_id) dials the local daemon at
/// `local_addr` (`serve_client_addr()`); every other entry dials its roster
/// address.
///
/// The roster address is the PEER-facing name: in the hub guide's default
/// topology the daemon binds `127.0.0.1:8765` and reaches the tailnet only
/// through `tailscale serve` on :443, so the hub's own tailnet DNS name
/// normalizes to `:8765`, where nothing listens on the tailnet. Dialing it
/// from the hub itself would show the hub unreachable from the one machine
/// that can always reach it.
///
/// An explicit loopback roster address (a same-host `--allow-loopback`
/// fleet) is kept as written: it already names this host, with the port
/// that node's daemon listens on, which the CLI's own serve config need not
/// know.
fn dial_address(entry: &fleet::MachineEntry, local_id: Option<&str>, local_addr: &str) -> String {
    if local_id.is_some_and(|l| fleet::same_machine(l, &entry.id)) && !fleet::address_host_is_loopback(&entry.address) {
        local_addr.to_string()
    } else {
        entry.address.clone()
    }
}

/// Build the message for a peer that answered but has no `path` route
/// (#1849). Two mutually exclusive possible causes, so the sentence picks
/// one remedy rather than naming both and leaving the operator to guess:
///
/// - The request went to a bare, non-loopback IP: a peer behind
///   `tailscale serve` routes by Host header and answers a bare IP with
///   Tailscale's own 404, not the daemon's — the remedy is re-adding the
///   peer by its DNS name, not upgrading darkmux.
/// - Otherwise, the 404 is darkmux's own (an older binary without this
///   route) — the remedy is upgrading darkmux on the peer.
///
/// This is a statement about the request darkmux just made and the
/// response it got back, never a claim about how the operator's peer is
/// actually set up (darkmux describes, never adjudicates).
fn route_missing_message(id: &str, path: &str, address: &str) -> String {
    if fleet::address_host_is_bare_ip(address) {
        format!(
            "peer `{id}` answered but has no `{path}` route. This request went to a bare IP — \
             a peer behind `tailscale serve` answers on its DNS name, not its IP, and would 404 \
             exactly like this. If `{id}` sits behind `tailscale serve`, re-add it by DNS name \
             (`darkmux machine add {id} --address <dns-name>:8765`); otherwise it may be running \
             an older darkmux (route not found) — upgrade darkmux on `{id}` and retry."
        )
    } else {
        format!(
            "peer `{id}` answered but has no `{path}` route — it may be running an older \
             darkmux (route not found). Upgrade darkmux on `{id}` and retry."
        )
    }
}

/// `darkmux machine list`: print the fleet view. This machine's own daemon
/// serves it at `GET /fleet/view` with this machine's seats and governor
/// readings; with no daemon running, the view is gathered here and says so
/// (`gathered_by`).
pub(crate) fn cmd_machine_list(emit_json: bool) -> Result<i32> {
    let view = crate::machine_list::local_fleet_view();
    if emit_json {
        // (#776) Machine-readable output stays byte-clean: force color off so
        // any accidental downstream style call can't leak ANSI into the JSON.
        darkmux_types::style::set_colorize_override(Some(false));
        cli_json::emit(&view)?;
        return Ok(0);
    }
    print!("{}", crate::machine_list::render_text(&view, &fleet::roster_path().display().to_string()));
    Ok(0)
}

// ─── Fleet work submission: the receiver's allow-list (#2916) ──────────

/// Whether `profile` may be granted to a peer: it must exist in this
/// machine's registry and resolve to a WORK model (a profile whose only model
/// is the machine's utility model is utility work, never addressable from
/// another machine, #2914). `Err` says why not.
fn grantable_profile(registry: &darkmux_types::ProfileRegistry, profile: &str) -> std::result::Result<(), String> {
    let Some(p) = registry.profiles.get(profile) else {
        return Err(format!("profile `{profile}` is not defined in this machine's registry"));
    };
    let utility = registry.utility_model_id();
    if utility.is_some() && !p.models.is_empty() && p.models.iter().all(|m| Some(m.id.as_str()) == utility) {
        return Err(format!(
            "profile `{profile}` lists only this machine's utility model; utility work is never taken \
             from another machine (#2914)"
        ));
    }
    Ok(())
}

/// Find the one node `name` refers to. The lookup names, in order: `--node`
/// (only that), else the host of `name`'s roster address, then `name`.
fn resolve_trust_node(
    provider: &dyn fleet::IdentityProvider,
    name: &str,
    node_hint: Option<&str>,
    roster_host: Option<&str>,
) -> Result<fleet::NodeIdentity> {
    let nodes = provider.nodes().map_err(|e| {
        anyhow::anyhow!(
            "the identity provider `{}` could not list the network's nodes: {e:#}",
            provider.provider_name()
        )
    })?;
    let queries: Vec<&str> = match node_hint {
        Some(n) => vec![n],
        None => roster_host.into_iter().chain(std::iter::once(name)).collect(),
    };
    for q in &queries {
        let hits: Vec<&fleet::NodeIdentity> = nodes.iter().filter(|n| n.answers_to(q)).collect();
        match hits.as_slice() {
            [] => continue,
            [one] => return Ok((*one).clone()),
            many => anyhow::bail!(
                "`{q}` names {} nodes on the {} network ({}); pass `--node <name>` with the one you mean",
                many.len(),
                provider.provider_name(),
                many.iter().map(|n| n.dns_name.clone().unwrap_or_else(|| n.name.clone())).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    let mut seen: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
    seen.sort();
    anyhow::bail!(
        "no node on the {} network answers to {} (nodes it reports: {}). Pass `--node <name>` with \
         the peer's name on the network.",
        provider.provider_name(),
        queries.iter().map(|q| format!("`{q}`")).collect::<Vec<_>>().join(" or "),
        if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
    )
}

/// What `machine trust` was asked to grant.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrustRequest<'a> {
    pub name: &'a str,
    pub node_hint: Option<&'a str>,
    pub profiles: &'a [String],
    pub roles: &'a [String],
    pub images: Option<&'a [String]>,
    pub workspace: Option<bool>,
}

/// The allow-list key already naming `name` (machine names are ASCII
/// case-insensitive), else `name` itself.
fn existing_key(root: &serde_json::Value, name: &str) -> String {
    root.get("fleet")
        .and_then(|f| f.get("accept_work"))
        .and_then(|a| a.as_object())
        .and_then(|a| a.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned())
        .unwrap_or_else(|| name.to_string())
}

fn string_list(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    v.and_then(|p| serde_json::from_value::<Vec<String>>(p.clone()).ok()).filter(|p| !p.is_empty())
}

fn clean_list(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in v.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if !out.iter().any(|o| o == x) {
            out.push(x.to_string());
        }
    }
    out
}

/// The pure-ish core of `machine trust`: resolve the node, check the scope,
/// write `fleet.accept_work.<name>` into the config.json at `config_path`.
/// Returns the confirmation text. Touches nothing but that one key.
/// `known_roles` is this machine's role library as (id, is_utility).
pub(crate) fn trust_at(
    config_path: &std::path::Path,
    req: &TrustRequest<'_>,
    provider: &dyn fleet::IdentityProvider,
    registry: &darkmux_types::ProfileRegistry,
    known_roles: &[(String, bool)],
    roster_host: Option<&str>,
) -> Result<String> {
    fleet::validate_machine_name("machine name", req.name)?;
    let mut root = crate::config_cmd::load_object(config_path)?;
    let key = existing_key(&root, req.name);
    let existing = root.get("fleet").and_then(|f| f.get("accept_work")).and_then(|a| a.get(&key)).cloned();
    let name = key.as_str();
    let from_existing = |field: &str| string_list(existing.as_ref().and_then(|e| e.get(field)));
    let profiles = if req.profiles.is_empty() { from_existing("profiles") } else { Some(clean_list(req.profiles)) }
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "name the profiles `{name}` may run on this machine: `darkmux machine trust {name} \
                 --profiles <profile>[,...] --roles <role>[,...]` (`darkmux profile list` shows them)"
            )
        })?;
    let roles = if req.roles.is_empty() { from_existing("roles") } else { Some(clean_list(req.roles)) }
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "name the roles `{name}` may dispatch on this machine: `--roles <role>[,...]` \
                 (`darkmux role list` shows them). There is no \"any role\": a role is a tool palette."
            )
        })?;
    let images = match req.images {
        Some(i) => clean_list(i),
        None => from_existing("images").unwrap_or_default(),
    };
    let mut problems: Vec<String> = profiles.iter().filter_map(|p| grantable_profile(registry, p).err()).collect();
    for r in &roles {
        match known_roles.iter().find(|(id, _)| id == r) {
            None => problems.push(format!("role `{r}` is not defined on this machine")),
            Some((_, true)) => problems.push(format!(
                "role `{r}` is a utility role; utility work is never taken from another machine (#2914)"
            )),
            Some(_) => {}
        }
    }
    for i in &images {
        if let Err(e) = fleet::validate_image_ref(i) {
            problems.push(format!("image `{i}`: {e:#}"));
        }
    }
    if !problems.is_empty() {
        anyhow::bail!("not trusting `{name}`: {}", problems.join("; "));
    }
    let node = resolve_trust_node(provider, name, req.node_hint, roster_host)?;
    // A machine never trusts its own node: it would let a local process
    // pass as a peer (#2916 review C4).
    if let Ok(me) = provider.local_node() {
        if me.node_id == node.node_id {
            anyhow::bail!(
                "`{}` is THIS machine's own node; a machine does not take fleet work from itself",
                node.dns_name.as_deref().unwrap_or(&node.name)
            );
        }
    }
    let workspace = req
        .workspace
        .or_else(|| existing.as_ref().and_then(|e| e.get("workspace")).and_then(|w| w.as_bool()))
        .unwrap_or(false);

    // Keep any field on the entry this binary does not know.
    let mut entry = existing.and_then(|e| e.as_object().cloned()).unwrap_or_default();
    entry.insert("node_id".into(), serde_json::Value::String(node.node_id.clone()));
    entry.insert("profiles".into(), serde_json::json!(profiles));
    entry.insert("roles".into(), serde_json::json!(roles));
    if images.is_empty() {
        entry.remove("images");
    } else {
        entry.insert("images".into(), serde_json::json!(images));
    }
    entry.insert("workspace".into(), serde_json::Value::Bool(workspace));
    {
        let obj = root.as_object_mut().expect("load_object returns an object");
        let fleet_v = obj.entry("fleet").or_insert_with(|| serde_json::json!({}));
        if !fleet_v.is_object() {
            *fleet_v = serde_json::json!({});
        }
        let aw = fleet_v.as_object_mut().unwrap().entry("accept_work").or_insert_with(|| serde_json::json!({}));
        if !aw.is_object() {
            *aw = serde_json::json!({});
        }
        aw.as_object_mut().unwrap().insert(name.to_string(), serde_json::Value::Object(entry));
    }
    serde_json::from_value::<darkmux_types::config::DarkmuxConfig>(root.clone())
        .map_err(|e| anyhow::anyhow!("the resulting config.json would not parse ({e}); nothing written"))?;
    std::fs::write(config_path, serde_json::to_string_pretty(&root)? + "\n")
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", config_path.display()))?;
    let state = match node.online {
        Some(true) => "online",
        Some(false) => "offline",
        None => "online state unknown",
    };
    Ok(format!(
        "machine: this machine now accepts work from `{name}`: the {} node `{}` ({state}; owner: {})\n  \
         may run profiles: {}\n  roles: {}\n  images: {}\n  workspace: {}\n  \
         written to {} (fleet.accept_work.{name}); the daemon reads it per request, no restart",
        provider.provider_name(),
        node.dns_name.as_deref().unwrap_or(&node.name),
        node.owner.as_deref().unwrap_or("not reported"),
        profiles.join(", "),
        roles.join(", "),
        if images.is_empty() { "darkmux's own runtime only".to_string() } else { images.join(", ") },
        if workspace {
            "yes (may mount any directory under this machine's worktrees base read-write)"
        } else {
            "no"
        },
        config_path.display(),
    ))
}

/// The core of `machine untrust`: remove `fleet.accept_work.<name>` and
/// nothing else. `Ok(false)` when there was no such entry.
pub(crate) fn untrust_at(config_path: &std::path::Path, name: &str) -> Result<bool> {
    let mut root = crate::config_cmd::load_object(config_path)?;
    let key = existing_key(&root, name);
    let removed = root
        .get_mut("fleet")
        .and_then(|f| f.get_mut("accept_work"))
        .and_then(|a| a.as_object_mut())
        .and_then(|a| a.remove(&key))
        .is_some();
    if removed {
        std::fs::write(config_path, serde_json::to_string_pretty(&root)? + "\n")
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", config_path.display()))?;
    }
    Ok(removed)
}

fn user_config_path() -> std::path::PathBuf {
    darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config
}

/// (#2916) The fleet work-submission rows `darkmux doctor` appends: fleet
/// token, identity provider, listener, allow-list, and the retired Redis
/// queue if it is still there. Gathers the facts; `darkmux-doctor`
/// evaluates them. No row prints a node id or a token.
pub(crate) fn fleet_submission_doctor_checks() -> Vec<crate::doctor::Check> {
    use crate::doctor::{FleetSubmissionFacts, ProviderReport, TrustView};
    let listener_enabled = darkmux_types::config_access::fleet_listener_enabled();
    let port = darkmux_types::config_access::fleet_listener_port();
    // (#2947) The raw value on a bad one, so the Unknown row names what was
    // written; the generic enum-settings row carries the valid values.
    let value = match darkmux_types::config_access::fleet_identity_provider() {
        Ok(p) => p.as_str().to_string(),
        Err(bad) => bad.raw,
    };
    let (provider_report, nodes, local_addr) = match fleet::configured_provider() {
        Err(_) => (ProviderReport::Unknown { value: value.clone() }, None, None),
        Ok(p) => match p.local_node() {
            Err(e) => (ProviderReport::Down { value: value.clone(), detail: format!("{e:#}") }, None, None),
            Ok(local) => {
                let addr = local.addresses.iter().find(|a| a.is_ipv4()).or(local.addresses.first()).copied();
                (
                    ProviderReport::Up {
                        value: value.clone(),
                        local_name: local.name.clone(),
                        local_addr: addr.map(|a| a.to_string()),
                    },
                    p.nodes().ok(),
                    addr,
                )
            }
        },
    };
    let listener_bound = if listener_enabled {
        local_addr.map(|ip| {
            std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::new(ip, port),
                std::time::Duration::from_millis(300),
            )
            .is_ok()
        })
    } else {
        None
    };
    let registry = darkmux_profiles::profiles::load_registry(None).ok().map(|l| l.registry);
    let known_roles: Option<Vec<(String, bool)>> = crate::crew::loader::load_roles().ok().map(|rs| {
        rs.into_iter()
            .map(|r| {
                let utility = !r.is_specialist();
                (r.id, utility)
            })
            .collect()
    });
    let trusted = fleet::read_user_allow_list().map(|allow| {
        allow
            .into_iter()
            .map(|(name, e)| {
                let node_id = e.node_id.clone().filter(|id| !id.is_empty());
                let node = node_id
                    .as_deref()
                    .and_then(|id| nodes.as_ref().and_then(|ns| ns.iter().find(|n| n.node_id == id)));
                let profiles = e.profiles.clone().unwrap_or_default();
                let roles = e.roles.clone().unwrap_or_default();
                let mut profile_problems: Vec<String> = match &registry {
                    Some(r) => profiles.iter().filter_map(|p| grantable_profile(r, p).err()).collect(),
                    None => vec!["this machine's profile registry could not be read".to_string()],
                };
                if let Some(known) = &known_roles {
                    for r in &roles {
                        match known.iter().find(|(id, _)| id == r) {
                            None => profile_problems.push(format!("role `{r}` is not defined here")),
                            Some((_, true)) => profile_problems.push(format!("role `{r}` is a utility role")),
                            Some(_) => {}
                        }
                    }
                }
                TrustView {
                    name,
                    has_node_id: node_id.is_some(),
                    network_name: node.map(|n| n.name.clone()),
                    online: node.and_then(|n| n.online),
                    // Only claim "gone" when the provider actually listed nodes.
                    node_on_network: nodes.is_none() || node.is_some(),
                    profiles,
                    roles,
                    images: e.images.clone().unwrap_or_default(),
                    profile_problems,
                    workspace: e.workspace.unwrap_or(false),
                }
            })
            .collect()
    });
    let retired = retired_queue_streams();
    let facts = FleetSubmissionFacts {
        listener_enabled,
        port,
        token_present: darkmux_flow::serve_token_present(),
        provider: provider_report,
        listener_bound,
        trusted,
        retired_streams: retired.0,
        queue_consumers: retired.1,
        daemon_token_set: daemon_token_set(),
        daemon_hub_link: daemon_hub_link(),
        daemon_listener_state: if listener_enabled && listener_bound != Some(true) {
            daemon_listener_state()
        } else {
            None
        },
        busy: crate::doctor::BusyFacts {
            running: daemon_health().as_ref().and_then(running_busy_settings),
            configured: darkmux_types::config_access::fleet_busy_policy().ok().map(|policy| {
                crate::doctor::BusySettings {
                    policy,
                    hosted_cap: darkmux_types::config_access::remote_concurrent_cap(),
                }
            }),
        },
        local_machine: darkmux_flow::resolve_machine_id(),
        clock_skews: fleet_clock_skews(),
    };
    crate::doctor::fleet_submission_checks(&facts)
}

/// (#3017) Each fleet machine's clock against the hub's, from the same view
/// `machine list` prints: only machines with a beat and a readable hub clock.
fn fleet_clock_skews() -> Vec<(String, i64)> {
    let view = crate::machine_list::local_fleet_view();
    view.machines.iter().filter_map(|m| Some((crate::machine_list::row_name(&view, m), m.clock_skew_ms?))).collect()
}

/// (#2916 review C8) What the local daemon says about its fleet listener
/// (`/health`'s `fleet_listener`), when it answers within 500 ms.
fn daemon_listener_state() -> Option<String> {
    daemon_health()?.get("fleet_listener").and_then(|s| s.as_str()).map(str::to_string)
}

/// Whether the local daemon reports a fleet token in its own environment
/// (`/health`'s `fleet_token_set`), when it answers within 500 ms.
fn daemon_token_set() -> Option<bool> {
    daemon_health()?.get("fleet_token_set")?.as_bool()
}

/// The local daemon's link to the fleet hub (`/health`'s `hub_link`), when it
/// answers within 500 ms and a hub is configured.
fn daemon_hub_link() -> Option<darkmux_flow::HubLink> {
    serde_json::from_value(daemon_health()?.get("hub_link")?.clone()).ok()
}

/// The local daemon's `/health`, when it answers within 500 ms.
fn daemon_health() -> Option<serde_json::Value> {
    let addr = darkmux_types::config_access::serve_client_addr();
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .get(&format!("http://{addr}/health"))
        .call()
        .ok()?
        .into_string()
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// (#2916 stage 2 review C5) The busy settings the running listener
/// reports in `/health`'s `fleet_busy` (parsed here, at the boundary).
fn running_busy_settings(health: &serde_json::Value) -> Option<crate::doctor::BusySettings> {
    use darkmux_types::config_enum::ConfigEnum;
    let busy = health.get("fleet_busy")?;
    Some(crate::doctor::BusySettings {
        policy: darkmux_types::config::BusyPolicy::parse(busy.get("policy")?.as_str()?)?,
        hosted_cap: u32::try_from(busy.get("hosted_cap")?.as_u64()?).ok()?,
    })
}

/// The retired work-queue streams (`darkmux:work`, `darkmux:work:<tier>`)
/// still in Redis, and the consumers still registered on them (name, idle
/// ms), when Redis is configured and answers within the usual bounded
/// connect. Empty otherwise: these rows only ever add a cleanup hint or
/// name a 3.x daemon still claiming queue work (#2916 review C6).
fn retired_queue_streams() -> (Vec<String>, Vec<(String, u64)>) {
    let Some(url) = darkmux_flow::redis_url() else { return (Vec::new(), Vec::new()) };
    let Ok(client) = redis::Client::open(url.expose_for_probe()) else { return (Vec::new(), Vec::new()) };
    let Ok(mut conn) = darkmux_flow::open_redis_connection_bounded(&client, darkmux_flow::REDIS_CONNECT_TIMEOUT) else {
        return (Vec::new(), Vec::new());
    };
    darkmux_flow::bound_redis_response(&conn);
    let mut found = Vec::new();
    let mut cursor: u64 = 0;
    for _ in 0..50 {
        let Ok((next, keys)): redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("darkmux:work*")
            .arg("COUNT")
            .arg(1000)
            .query(&mut conn)
        else {
            break;
        };
        found.extend(keys.into_iter().filter(|k| k == "darkmux:work" || k.starts_with("darkmux:work:")));
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    found.sort();
    found.dedup();
    let mut consumers = Vec::new();
    for stream in &found {
        let groups: Vec<std::collections::HashMap<String, redis::Value>> =
            redis::cmd("XINFO").arg("GROUPS").arg(stream).query(&mut conn).unwrap_or_default();
        for g in groups {
            let Some(group) = g.get("name").and_then(|v| redis::from_redis_value::<String>(v).ok()) else {
                continue;
            };
            let cs: Vec<std::collections::HashMap<String, redis::Value>> = redis::cmd("XINFO")
                .arg("CONSUMERS")
                .arg(stream)
                .arg(&group)
                .query(&mut conn)
                .unwrap_or_default();
            for c in cs {
                let name = c.get("name").and_then(|v| redis::from_redis_value::<String>(v).ok());
                let idle = c.get("idle").and_then(|v| redis::from_redis_value::<u64>(v).ok());
                if let (Some(n), Some(i)) = (name, idle) {
                    consumers.push((n, i));
                }
            }
        }
    }
    (found, consumers)
}

/// `darkmux machine trust <name>` (#2916).
pub(crate) fn cmd_machine_trust(
    name: &str,
    node: Option<&str>,
    profiles: &[String],
    roles: &[String],
    images: Option<&[String]>,
    workspace: Option<bool>,
) -> Result<i32> {
    let provider = fleet::configured_provider()?;
    let loaded = darkmux_profiles::profiles::load_registry(None)?;
    let known_roles: Vec<(String, bool)> = crate::crew::loader::load_roles()?
        .into_iter()
        .map(|r| {
            let utility = !r.is_specialist();
            (r.id, utility)
        })
        .collect();
    let roster_host = fleet::load_roster()
        .ok()
        .and_then(|r| fleet::find_machine(&r, name).ok().flatten().and_then(|e| fleet::address_host(&e.address)));
    let req = TrustRequest { name, node_hint: node, profiles, roles, images, workspace };
    let msg = trust_at(
        &user_config_path(),
        &req,
        provider.as_ref(),
        &loaded.registry,
        &known_roles,
        roster_host.as_deref(),
    )?;
    println!("{msg}");
    if !darkmux_types::config_access::fleet_listener_enabled() {
        println!(
            "  note: this machine's fleet listener is off, so it takes no work yet: \
             `darkmux config set fleet.listener.enabled true`, then restart `darkmux serve`"
        );
    }
    Ok(0)
}

/// `darkmux machine untrust <name>` (#2916).
pub(crate) fn cmd_machine_untrust(name: &str) -> Result<i32> {
    let path = user_config_path();
    if untrust_at(&path, name)? {
        println!(
            "machine: this machine no longer accepts work from `{name}` (removed fleet.accept_work.{name} \
             from {}); effective on the next request",
            path.display()
        );
        Ok(0)
    } else {
        eprintln!("machine: `{name}` is not on this machine's allow-list (fleet.accept_work); nothing changed");
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── machine add: loopback refusal + self by machine_id (#2924) ──────
    //
    // `cmd_machine_add` is the one writer of operator roster entries, so the
    // guards are exercised through it, not only through the pure helper.

    /// Pin the roster file and the machine_id for one test; returns the
    /// TempDir guard. Env-mutating, so every caller is `#[serial]`.
    fn isolated_add_env(machine_id: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", tmp.path().join("fleet.json"));
            std::env::set_var("DARKMUX_MACHINE_ID", machine_id);
        }
        tmp
    }

    fn clear_add_env() {
        unsafe {
            std::env::remove_var("DARKMUX_FLEET_FILE");
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
    }

    #[test]
    fn loopback_refusal_names_the_fix_and_the_escape_hatch() {
        for addr in ["127.0.0.1:8765", "127.0.0.1", "[::1]:8765", "http://127.0.0.1:8765"] {
            let msg = loopback_refusal("studio", addr, false)
                .unwrap_or_else(|| panic!("{addr} must be refused"));
            assert!(msg.contains("darkmux machine add studio --address <tailnet-dns-name>"), "{msg}");
            assert!(msg.contains("--allow-loopback"), "{msg}");
        }
        assert_eq!(loopback_refusal("studio", "127.0.0.1:8765", true), None);
        assert_eq!(loopback_refusal("studio", "studio.tailnet.example", false), None);
        assert_eq!(loopback_refusal("studio", "100.64.0.2:8765", false), None);
    }

    /// The Studio's defect, through the real verb: the documented
    /// self-registration command no longer writes a loopback entry.
    #[serial_test::serial]
    #[test]
    fn cmd_machine_add_refuses_a_loopback_address_and_writes_nothing() {
        let _tmp = isolated_add_env("studio");
        let code = cmd_machine_add("studio", "127.0.0.1:8765", None, false).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        assert_eq!(code, 2, "a refused add exits 2");
        assert!(roster.machines.is_empty(), "nothing may be written: {:?}", roster.machines.keys());
    }

    #[serial_test::serial]
    #[test]
    fn cmd_machine_add_writes_loopback_when_explicitly_allowed() {
        let _tmp = isolated_add_env("viewer");
        let code = cmd_machine_add("peer-a", "127.0.0.1:18765", None, true).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        assert_eq!(code, 0);
        let entry = roster.machines.get("peer-a").unwrap();
        assert_eq!(entry.address, "127.0.0.1:18765");
        assert!(entry.loopback_intended, "the entry records that loopback was asked for (#2924 C-6)");
        // Loopback no longer means "self": a same-host peer under another
        // name must not be stamped with this host's identity.
        assert_eq!(entry.machine_uid, None);
    }

    #[serial_test::serial]
    #[test]
    fn cmd_machine_add_remote_address_never_resolves_a_uid() {
        // A peer, named by something other than this machine's machine_id.
        let _tmp = isolated_add_env("laptop");
        cmd_machine_add("peer1", "100.64.0.2:8765", None, false).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        assert_eq!(roster.machines.get("peer1").unwrap().machine_uid, None);
    }

    /// (#2924) Self is recognized by NAME — the entry's id equals this
    /// machine's machine_id — at a peer-usable address. Asserted against
    /// `darkmux_hardware::machine_uid()`'s own live answer (None off macOS is
    /// correct there too); on macOS it must actually resolve, so the equality
    /// cannot pass vacuously on None == None.
    #[serial_test::serial]
    #[test]
    fn cmd_machine_add_self_entry_is_recognized_by_machine_id_and_records_the_uid() {
        let _tmp = isolated_add_env("studio");
        cmd_machine_add("studio", "studio.tailnet.example", None, false).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        let uid = roster.machines.get("studio").unwrap().machine_uid.clone();
        assert_eq!(uid.as_deref(), darkmux_hardware::machine_uid());
        #[cfg(target_os = "macos")]
        assert!(uid.is_some(), "a self entry on macOS must carry this host's uid");
    }

    #[serial_test::serial]
    #[test]
    fn cmd_machine_add_re_add_of_the_self_entry_keeps_its_uid() {
        let _tmp = isolated_add_env("studio");
        cmd_machine_add("studio", "studio.tailnet.example", None, false).unwrap();
        cmd_machine_add("studio", "studio.tailnet.example", Some("updated"), false).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        let entry = roster.machines.get("studio").unwrap();
        assert_eq!(entry.machine_uid.as_deref(), darkmux_hardware::machine_uid());
        assert_eq!(entry.description.as_deref(), Some("updated"));
    }

    // ── doctor roster rows (#2924) ──────────────────────────────────────

    fn write_flow(dir: &std::path::Path, file: &str, lines: &[&str]) {
        std::fs::write(dir.join(file), lines.join("\n") + "\n").unwrap();
    }

    fn live(beats: &[(&str, &str)], local_uid: Option<&str>, local_name: Option<&str>) -> LiveIdentity {
        LiveIdentity {
            beats: beats.iter().map(|(u, n)| (u.to_string(), n.to_string())).collect(),
            local_uid: local_uid.map(str::to_string),
            local_name: local_name.map(str::to_string),
            local_name_from_env: false,
            presence: crate::doctor::PresenceState::Read,
        }
    }

    #[serial_test::serial]
    #[test]
    fn a_later_add_without_allow_loopback_clears_the_intent() {
        let _tmp = isolated_add_env("viewer");
        cmd_machine_add("peer-a", "127.0.0.1:18765", None, true).unwrap();
        cmd_machine_add("peer-a", "peer-a.tailnet.example", None, false).unwrap();
        let roster = fleet::load_roster().unwrap();
        clear_add_env();
        assert!(!roster.machines.get("peer-a").unwrap().loopback_intended);
    }

    #[test]
    fn gather_identity_knowledge_takes_each_uids_latest_name_and_keeps_every_uid_per_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_flow(tmp.path(), "2026-06-01.jsonl", &[
            r#"{"ts":"2026-06-01T00:00:00Z","machine_id":"laptop","machine_uid":"UID-A"}"#,
            r#"{"ts":"2026-06-01T00:00:01Z","machine_id":"old-box"}"#,
            r#"{"ts":"2026-06-01T00:00:02Z","machine_id":"MacBook-Pro-shared","machine_uid":"UID-B"}"#,
            "not json at all",
        ]);
        write_flow(tmp.path(), "2026-09-01.jsonl", &[
            r#"{"ts":"2026-09-01T00:00:00Z","machine_id":"MacBook-Pro-shared","machine_uid":"UID-A"}"#,
            r#"{"ts":"2026-09-01T00:00:01Z","machine_id":"MacBook-Pro","machine_uid":"UID-A"}"#,
        ]);
        std::fs::write(tmp.path().join("notes.txt"), r#"{"machine_id":"ignored","machine_uid":"UID-Z"}"#).unwrap();
        let k = gather_identity_knowledge(Some(tmp.path()), &live(&[], None, None));
        assert_eq!(k.current_name_by_uid.get("UID-A").map(String::as_str), Some("MacBook-Pro"));
        assert_eq!(k.uids_by_name.get("laptop").map(|s| s.len()), Some(1));
        assert_eq!(k.uids_by_name.get("MacBook-Pro-shared").map(|s| s.len()), Some(2), "a shared name keeps both uids");
        assert!(k.uidless_names.contains("old-box"));
        assert!(!k.current_name_by_uid.contains_key("UID-Z"), "only .jsonl flow files are read");
    }

    /// C-5: only the most recent `ROSTER_HISTORY_FILES` day files are read.
    #[test]
    fn gather_identity_knowledge_reads_a_bounded_window_of_day_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_flow(tmp.path(), "2000-01-01.jsonl", &[r#"{"machine_id":"ancient","machine_uid":"UID-OLD"}"#]);
        for i in 0..ROSTER_HISTORY_FILES {
            write_flow(tmp.path(), &format!("2026-{:02}-{:02}.jsonl", 1 + i / 28, 1 + i % 28), &[
                r#"{"machine_id":"recent","machine_uid":"UID-R"}"#,
            ]);
        }
        let k = gather_identity_knowledge(Some(tmp.path()), &live(&[], None, None));
        assert!(k.uids_by_name.contains_key("recent"));
        assert!(!k.uids_by_name.contains_key("ancient"), "a file older than the window is not read");
        assert_eq!(k.history_truncated_to, Some(ROSTER_HISTORY_FILES), "C-e: the row can say so");
        std::fs::remove_file(tmp.path().join("2000-01-01.jsonl")).unwrap();
        let k = gather_identity_knowledge(Some(tmp.path()), &live(&[], None, None));
        assert_eq!(k.history_truncated_to, None, "nothing was skipped");
    }

    /// Presence outranks history, and this machine's own resolution outranks
    /// presence; this machine's name counts even with no readable uid (C-3).
    #[test]
    fn gather_identity_knowledge_lets_presence_and_self_override_history() {
        let tmp = tempfile::tempdir().unwrap();
        write_flow(tmp.path(), "2026-09-01.jsonl", &[
            r#"{"machine_id":"laptop","machine_uid":"UID-A"}"#,
            r#"{"machine_id":"studio-old","machine_uid":"UID-B"}"#,
        ]);
        let k = gather_identity_knowledge(Some(tmp.path()), &live(&[("UID-A", "MacBook-Pro")], Some("UID-B"), Some("studio")));
        assert_eq!(k.current_name_by_uid.get("UID-A").map(String::as_str), Some("MacBook-Pro"));
        assert_eq!(k.current_name_by_uid.get("UID-B").map(String::as_str), Some("studio"));
        assert_eq!(k.local_name.as_deref(), Some("studio"));
        let no_uid = gather_identity_knowledge(None, &live(&[], None, Some("studio")));
        assert_eq!(no_uid.local_name.as_deref(), Some("studio"), "C-3: the name without a uid");
    }

    /// Which uids have a LIVE current name, and this machine's uid.
    #[test]
    fn gather_identity_knowledge_marks_live_uids_and_the_local_uid() {
        let k = gather_identity_knowledge(None, &live(&[("UID-A", "MacBook-Pro")], Some("UID-B"), Some("studio")));
        assert!(k.live_uids.contains("UID-A") && k.live_uids.contains("UID-B"));
        assert_eq!(k.local_uid.as_deref(), Some("UID-B"));
        assert!(!k.local_name_from_env);
    }

    /// C-b: a `DARKMUX_MACHINE_ID` session override is not this machine's
    /// current name: it is not overlaid onto the local uid, and the knowledge
    /// says where the local name came from.
    #[test]
    fn gather_identity_knowledge_does_not_overlay_a_session_override() {
        let mut l = live(&[], Some("UID-B"), Some("review-scratch"));
        l.local_name_from_env = true;
        let tmp = tempfile::tempdir().unwrap();
        write_flow(tmp.path(), "2026-09-01.jsonl", &[r#"{"machine_id":"studio","machine_uid":"UID-B"}"#]);
        let k = gather_identity_knowledge(Some(tmp.path()), &l);
        assert_eq!(k.current_name_by_uid.get("UID-B").map(String::as_str), Some("studio"));
        assert!(k.local_name_from_env);
        assert!(!k.live_uids.contains("UID-B"));
    }

    /// C-d: pin the Unreadable classification at its call site: Redis is
    /// configured but nothing answers.
    #[serial_test::serial]
    #[test]
    fn roster_doctor_checks_say_when_redis_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", tmp.path().join("fleet.json"));
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path().join("flows"));
            std::env::set_var("DARKMUX_REDIS_URL", format!("redis://127.0.0.1:{port}"));
            std::env::set_var("DARKMUX_MACHINE_ID", "this-test-host");
        }
        fleet::mutate_roster(|r| fleet::add_machine(r, "studio", "studio.tailnet.example", None, None)).unwrap();
        let checks = roster_doctor_checks();
        unsafe {
            for k in ["DARKMUX_FLEET_FILE", "DARKMUX_FLOWS_DIR", "DARKMUX_REDIS_URL", "DARKMUX_MACHINE_ID"] {
                std::env::remove_var(k);
            }
        }
        let ident = checks.iter().find(|c| c.name == "roster identity").unwrap();
        assert!(ident.message.contains("Redis unreachable"), "{}", ident.message);
    }

    /// C-b at the call site: an env-tier machine_id reaches the knowledge as
    /// a session override.
    #[serial_test::serial]
    #[test]
    fn roster_doctor_checks_name_an_env_tier_machine_id() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", tmp.path().join("fleet.json"));
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path().join("flows"));
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_MACHINE_ID", "session-name");
        }
        fleet::mutate_roster(|r| fleet::add_machine(r, "studio", "studio.tailnet.example", None, None)).unwrap();
        let checks = roster_doctor_checks();
        unsafe {
            for k in ["DARKMUX_FLEET_FILE", "DARKMUX_FLOWS_DIR", "DARKMUX_MACHINE_ID"] {
                std::env::remove_var(k);
            }
        }
        let ident = checks.iter().find(|c| c.name == "roster identity").unwrap();
        assert!(ident.message.contains("comes from DARKMUX_MACHINE_ID"), "{}", ident.message);
    }

    /// The live fleet from #2924 through the row builder: the Studio's self
    /// entry at loopback, the laptop rostered under a name it no longer uses.
    #[test]
    fn roster_checks_report_the_studio_loopback_and_the_laptop_rename() {
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "studio", "127.0.0.1:8765", None, Some("UID-B")).unwrap();
        fleet::add_machine(&mut roster, "laptop", "laptop.tailnet.example", None, None).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_flow(tmp.path(), "2026-06-10.jsonl", &[r#"{"machine_id":"laptop","machine_uid":"UID-A"}"#]);
        let known = gather_identity_knowledge(Some(tmp.path()), &live(&[("UID-A", "MacBook-Pro")], Some("UID-B"), Some("studio")));
        let checks = roster_checks(&roster, &known);
        let by_name = |n: &str| checks.iter().find(|c| c.name == n).unwrap();
        let addr = by_name("roster addresses");
        assert_eq!(addr.status, crate::doctor::Status::Warn, "{}", addr.message);
        assert!(addr.message.contains("`studio` at 127.0.0.1:8765"), "{}", addr.message);
        let ident = by_name("roster identity");
        assert_eq!(ident.status, crate::doctor::Status::Warn, "{}", ident.message);
        assert!(ident.message.contains("`laptop` was last used"), "{}", ident.message);
        assert!(ident.message.contains("now called `MacBook-Pro`"), "{}", ident.message);
    }

    /// An entry added with --allow-loopback reaches the row as intentional.
    #[test]
    fn roster_checks_carry_the_loopback_intent() {
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "peer-a", "127.0.0.1:18765", None, None).unwrap();
        roster.machines.get_mut("peer-a").unwrap().loopback_intended = true;
        let checks = roster_checks(&roster, &crate::doctor::FleetIdentityKnowledge::default());
        let addr = checks.iter().find(|c| c.name == "roster addresses").unwrap();
        assert_eq!(addr.status, crate::doctor::Status::Pass, "{}", addr.message);
    }


    #[test]
    fn record_identity_reads_top_level_fields_and_falls_back_on_anything_else() {
        let id = |l: &str| record_identity(l);
        assert_eq!(
            id(r#"{"ts":"t","machine_id":"studio","machine_uid":"U1","data":{"x":1}}"#),
            Some(("studio".into(), Some("U1".into())))
        );
        assert_eq!(id(r#"{"ts":"t","machine_id":"studio"}"#), Some(("studio".into(), None)));
        // A machine_id inside a nested object is not the record's own: the
        // fast path must not take it, and the real parse finds the top-level one.
        assert_eq!(
            id(r#"{"data":{"machine_id":"nested","machine_uid":"UN"},"machine_id":"top","machine_uid":"UT"}"#),
            Some(("top".into(), Some("UT".into())))
        );
        assert_eq!(id(r#"{"data":{"machine_id":"nested"}}"#), None);
        // null / escaped values go through the real parse.
        assert_eq!(id(r#"{"machine_id":"a","machine_uid":null}"#), Some(("a".into(), None)));
        assert_eq!(id(r#"{"machine_id":"a\"b"}"#), Some(("a\"b".into(), None)));
        assert_eq!(id(r#"{"ts":"t"}"#), None);
        assert_eq!(id(r#"{"machine_id":""}"#), None);
        assert_eq!(id("not json"), None);
    }

    /// The scan is skipped only when live knowledge settles every entry.

    #[test]
    fn roster_needs_history_only_for_entries_live_knowledge_cannot_settle() {
        let live = gather_identity_knowledge(None, &live(&[("UID-A", "MacBook-Pro")], Some("UID-B"), Some("studio")));
        let mut healthy = fleet::FleetRoster::default();
        fleet::add_machine(&mut healthy, "studio", "studio.tailnet.example", None, Some("UID-B")).unwrap();
        fleet::add_machine(&mut healthy, "MacBook-Pro", "mbp.tailnet.example", None, None).unwrap();
        assert!(!roster_needs_history(&healthy, &live));

        let mut stale = healthy.clone();
        fleet::add_machine(&mut stale, "laptop", "laptop.tailnet.example", None, None).unwrap();
        assert!(roster_needs_history(&stale, &live), "a uid-less non-current name needs history");

        let mut offline = healthy.clone();
        fleet::add_machine(&mut offline, "mini-1", "mini.tailnet.example", None, Some("UID-C")).unwrap();
        assert!(roster_needs_history(&offline, &live), "a uid nobody live answers to needs history");
    }

    /// (#2916 stage 2 review C5) `/health`'s `fleet_busy` is read into typed
    /// settings; anything malformed is "no report", never a guess.
    #[test]
    fn the_running_busy_settings_are_read_from_health() {
        let got = running_busy_settings(&serde_json::json!({"fleet_busy": {"policy": "queue", "hosted_cap": 3}}));
        assert_eq!(
            got,
            Some(crate::doctor::BusySettings { policy: darkmux_types::config::BusyPolicy::Queue, hosted_cap: 3 })
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"fleet_busy": null}),
            serde_json::json!({"fleet_busy": {"policy": "sometimes", "hosted_cap": 1}}),
            serde_json::json!({"fleet_busy": {"policy": "queue", "hosted_cap": -1}}),
        ] {
            assert_eq!(running_busy_settings(&bad), None, "{bad}");
        }
    }

    /// A config whose `fleet.identity.provider` is an unregistered value.
    fn bad_provider_config() -> darkmux_types::config::DarkmuxConfig {
        darkmux_types::config::DarkmuxConfig {
            fleet: Some(darkmux_types::config::FleetConfig {
                identity: Some(darkmux_types::config::FleetIdentityConfig {
                    provider: Some("zz-bad-provider".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// (#2947 review C1) `machine add` refuses a bad identity provider
    /// before it writes the roster.
    #[serial_test::serial]
    #[test]
    fn machine_add_refuses_a_bad_identity_provider_before_writing_the_roster() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet_file = tmp.path().join("fleet.json");
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", &fleet_file) };
        let r = {
            let _g = darkmux_types::config_access::set_config_for_test(bad_provider_config());
            cmd_machine_add("peer-x", "peer-x.tailnet.example:8765", None, false)
        };
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("fleet work submission: refusing to start") && msg.contains("`zz-bad-provider`"), "{msg}");
        assert!(!fleet_file.exists(), "the roster was written before the refusal");
    }

    /// C-2: pin the history gate at its real call site. The laptop shape (a
    /// uid-less entry known only from history) must read as renamed through
    /// `roster_doctor_checks` itself, with no Redis configured.
    #[serial_test::serial]
    #[test]
    fn roster_doctor_checks_read_history_for_the_laptop_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let flows = tmp.path().join("flows");
        std::fs::create_dir_all(&flows).unwrap();
        write_flow(&flows, "2026-06-10.jsonl", &[r#"{"machine_id":"laptop","machine_uid":"UID-HIST-A"}"#]);
        write_flow(&flows, "2026-09-20.jsonl", &[r#"{"machine_id":"renamed-now","machine_uid":"UID-HIST-A"}"#]);
        let fleet_file = tmp.path().join("fleet.json");
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", &fleet_file);
            std::env::set_var("DARKMUX_FLOWS_DIR", &flows);
            std::env::set_var("DARKMUX_MACHINE_ID", "this-test-host");
            std::env::remove_var("DARKMUX_REDIS_URL");
        }
        fleet::mutate_roster(|r| fleet::add_machine(r, "laptop", "laptop.tailnet.example", None, None)).unwrap();
        let checks = roster_doctor_checks();
        unsafe {
            std::env::remove_var("DARKMUX_FLEET_FILE");
            std::env::remove_var("DARKMUX_FLOWS_DIR");
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
        let ident = checks.iter().find(|c| c.name == "roster identity").unwrap();
        assert_eq!(ident.status, crate::doctor::Status::Warn, "{}", ident.message);
        assert!(ident.message.contains("now called `renamed-now`"), "{}", ident.message);
        assert!(ident.message.contains("no Redis configured"), "{}", ident.message);
    }

    #[serial_test::serial]
    #[test]
    fn roster_doctor_checks_are_silent_without_a_roster() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", tmp.path().join("fleet.json")) };
        let checks = roster_doctor_checks();
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
        assert!(checks.is_empty(), "{:?}", checks.iter().map(|c| &c.name).collect::<Vec<_>>());
    }

    // ── this machine's own entry dials the local daemon (#2924 MF-3) ────

    #[test]
    fn dial_address_uses_the_local_daemon_for_this_machines_entry_only() {
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "studio", "studio.tailnet.example", None, None).unwrap();
        fleet::add_machine(&mut roster, "laptop", "laptop.tailnet.example", None, None).unwrap();
        let studio = roster.machines.get("studio").unwrap();
        let laptop = roster.machines.get("laptop").unwrap();
        assert_eq!(dial_address(studio, Some("studio"), "127.0.0.1:8799"), "127.0.0.1:8799");
        assert_eq!(dial_address(laptop, Some("studio"), "127.0.0.1:8799"), "laptop.tailnet.example");
        assert_eq!(dial_address(studio, None, "127.0.0.1:8799"), "studio.tailnet.example");
    }

    /// (#2924 re-review MUST 1) A same-host `--allow-loopback` fleet: the
    /// entry names an explicit loopback address with its own port, which is
    /// exactly where that node's daemon is. Substituting the CLI's
    /// `serve_client_addr()` (8765 with no serve config) broke it.
    #[test]
    fn dial_address_keeps_an_explicit_loopback_self_entry() {
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "node-a", "127.0.0.1:18881", None, None).unwrap();
        let node_a = roster.machines.get("node-a").unwrap();
        assert_eq!(dial_address(node_a, Some("node-a"), "127.0.0.1:8765"), "127.0.0.1:18881");
    }

    /// Through the real `machine status <id>` path: this machine's entry
    /// names an address nothing answers on (the hub guide's default topology,
    /// where the tailnet name only serves :443), yet the read reaches the
    /// local daemon.
    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_for_this_machines_entry_reaches_the_local_daemon() {
        let addr = one_shot_http("200 OK", r#"{"ok":true}"#);
        let port = addr.rsplit_once(':').unwrap().1.to_string();
        let _tmp = isolated_roster(&[("self-host", "self-host.invalid")]);
        unsafe {
            std::env::set_var("DARKMUX_MACHINE_ID", "self-host");
            std::env::set_var("DARKMUX_SERVE_PORT", &port);
            std::env::set_var("DARKMUX_SERVE_BIND", "127.0.0.1");
        }
        let got = fetch_peer_json("self-host", "/machine/status");
        unsafe {
            std::env::remove_var("DARKMUX_MACHINE_ID");
            std::env::remove_var("DARKMUX_SERVE_PORT");
            std::env::remove_var("DARKMUX_SERVE_BIND");
            std::env::remove_var("DARKMUX_FLEET_FILE");
        }
        assert_eq!(got.unwrap()["ok"], true);
    }

    // ── fetch_peer_json error shapes (#1426) ────────────────────────────
    //
    // Each test isolates the roster via DARKMUX_FLEET_FILE (read live per
    // access by config_access) and, where a peer is needed, serves canned
    // HTTP from a one-shot std TcpListener on a loopback ephemeral port.
    // Env-mutating, so #[serial_test::serial].

    /// Point the roster at a fresh tempfile and register `entries`.
    /// Returns the TempDir guard (dropping it removes the roster).
    fn isolated_roster(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("fleet.json");
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", &file) };
        for (id, addr) in entries {
            fleet::mutate_roster(|roster| {
                fleet::add_machine(roster, id, addr, None, None)?;
                Ok(())
            })
            .unwrap();
        }
        tmp
    }

    /// One-shot HTTP responder: accepts a single connection on an ephemeral
    /// loopback port and answers with `status_line` + `body`. Returns the
    /// bound address.
    fn one_shot_http(status_line: &'static str, body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf); // consume request
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        addr
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_unknown_roster_id_names_machine_add() {
        let _tmp = isolated_roster(&[]);
        let err = fetch_peer_json("no-such-machine", "/machine/status").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no machine `no-such-machine` in roster"), "{msg}");
        assert!(msg.contains("darkmux machine add"), "hint names the fix: {msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_401_names_the_shared_fleet_token() {
        let addr = one_shot_http("401 Unauthorized", "{}");
        let _tmp = isolated_roster(&[("peer1", addr.as_str())]);
        let err = fetch_peer_json("peer1", "/machine/status").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bearer token"), "{msg}");
        assert!(msg.contains("DARKMUX_SERVE_TOKEN"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_404_says_older_darkmux_not_unreachable() {
        let addr = one_shot_http("404 Not Found", "{}");
        let _tmp = isolated_roster(&[("peer1", addr.as_str())]);
        let err = fetch_peer_json("peer1", "/machine/resources").unwrap_err();
        let msg = err.to_string();
        // The peer answered — "could not reach" is the wrong vocabulary.
        assert!(msg.contains("older"), "names the likely cause: {msg}");
        assert!(msg.contains("route not found"), "{msg}");
        assert!(!msg.contains("could not reach"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_404_from_loopback_ip_gets_no_tailscale_serve_hint() {
        // (#1849 MUST FIX 1, red-prove: loopback direction) `one_shot_http`
        // binds loopback, so the roster address IS a bare IP
        // (`127.0.0.1:<port>`) by shape — but loopback traffic never
        // traverses `tailscale serve` (a same-host `--allow-loopback` entry
        // is exactly this shape), so the hint must NOT fire here. This is the
        // opposite of what this test asserted before #1849's loopback
        // exclusion — it used to assert the hint DID fire, which was only
        // true because the guard didn't yet know loopback isn't the
        // tailscale-serve shape.
        let addr = one_shot_http("404 Not Found", "{}");
        let _tmp = isolated_roster(&[("peer1", addr.as_str())]);
        let err = fetch_peer_json("peer1", "/machine/resources").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("older"), "base message still present: {msg}");
        assert!(!msg.contains("tailscale serve"), "{msg}");
        assert!(!msg.contains("bare IP"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_404_from_dns_name_gets_no_tailscale_serve_hint() {
        // Inverted case (#1849 red-prove requirement): the same 404, but
        // reached via a DNS name (`localhost`, which resolves to the same
        // loopback listener) rather than a bare IP. The hint must NOT
        // appear — a DNS-addressed peer's 404 is not the bare-IP failure
        // mode this hint exists to name.
        let addr = one_shot_http("404 Not Found", "{}");
        let port = addr.rsplit_once(':').expect("host:port").1;
        let dns_addr = format!("localhost:{port}");
        let _tmp = isolated_roster(&[("peer1", dns_addr.as_str())]);
        let err = fetch_peer_json("peer1", "/machine/resources").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("older"), "base message still present: {msg}");
        assert!(!msg.contains("tailscale serve"), "{msg}");
        assert!(!msg.contains("bare IP"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_unreachable_peer_says_could_not_reach() {
        // Port 1 on loopback has no listener (and connecting is refused fast).
        let _tmp = isolated_roster(&[("ghost", "127.0.0.1:1")]);
        let err = fetch_peer_json("ghost", "/machine/status").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("could not reach `ghost`"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    #[serial_test::serial]
    #[test]
    fn fetch_peer_json_non_json_200_reports_a_parse_error() {
        let addr = one_shot_http("200 OK", "this is not json");
        let _tmp = isolated_roster(&[("peer1", addr.as_str())]);
        let err = fetch_peer_json("peer1", "/machine/status").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("parsing JSON from `peer1`"), "{msg}");
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
    }

    // ── route_missing_message (#1849) — pure, no network ────────────────
    //
    // `address_host_is_bare_ip` is pure string parsing, so the
    // hint-selection logic is testable directly without a TCP round trip.
    // The live-network tests above (loopback, DNS name) already prove
    // `fetch_peer_json` threads `entry.address` into this function; these
    // tests prove the function's own branching, including the direction a
    // network fixture can't cheaply cover: a real (non-loopback) bare IP.

    #[test]
    fn route_missing_message_hints_tailscale_serve_for_a_non_loopback_bare_ip() {
        // (#1849 MUST FIX 1, red-prove: non-loopback direction) A
        // tailnet-shaped address — the documented CGNAT example range,
        // never a real tailnet IP — still gets the hint after the
        // loopback exclusion below.
        let msg = route_missing_message("peer1", "/machine/resources", "100.64.0.2:8765");
        assert!(msg.contains("tailscale serve"), "{msg}");
        assert!(msg.contains("DNS name, not its IP"), "{msg}");
    }

    #[test]
    fn route_missing_message_no_hint_for_a_loopback_bare_ip() {
        // (#1849 MUST FIX 1, red-prove: loopback direction)
        let msg = route_missing_message("peer1", "/machine/resources", "127.0.0.1:8765");
        assert!(msg.contains("older"), "{msg}");
        assert!(!msg.contains("tailscale serve"), "{msg}");
    }

    #[test]
    fn route_missing_message_no_hint_for_a_dns_name() {
        let msg = route_missing_message("peer1", "/machine/resources", "studio.tailnet:8765");
        assert!(msg.contains("older"), "{msg}");
        assert!(!msg.contains("tailscale serve"), "{msg}");
    }

    fn studio_entry() -> fleet::MachineEntry {
        fleet::MachineEntry {
            id: "studio".into(),
            address: "100.64.0.2".into(),
            description: None,
            added_unix_ms: 1,
            machine_uid: None,
            loopback_intended: false,
            node_id: None,
            current_name: None,
            extras: Default::default(),
        }
    }

    fn overlay_provider() -> fleet::StaticIdentityProvider {
        fleet::StaticIdentityProvider {
            local: fleet::test_node("nLAPTOP", "laptop", "100.64.0.7"),
            peers: vec![fleet::test_node("nSTUDIO", "studio", "100.64.0.2")],
            down: None,
        }
    }

    /// The promise: `machine status <id>` / `resources <id>` send the token
    /// only after the peer's first-contact pin is in the roster.
    #[test]
    #[serial_test::serial]
    fn a_first_contact_read_pins_the_peer_in_the_roster() {
        let tmp = isolated_add_env("laptop");
        fleet::mutate_roster(|r| {
            r.machines.insert("studio".into(), studio_entry());
            Ok(())
        })
        .unwrap();
        let target = peer_target_with(&studio_entry(), Some("laptop"), &overlay_provider()).map_err(|e| format!("{e:#}"));
        let pinned = fleet::load_roster().unwrap().machines["studio"].node_id.clone();
        clear_add_env();
        drop(tmp);
        assert_eq!(target.unwrap().node_id(), Some("nSTUDIO"));
        assert_eq!(pinned.as_deref(), Some("nSTUDIO"), "the pin was written before the target was handed out");
    }

    /// Fail closed: a roster that cannot be written yields no target, so no
    /// token can be attached.
    #[test]
    #[serial_test::serial]
    fn an_unwritable_roster_yields_no_token_bearing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", blocker.join("fleet.json")) };
        let out = peer_target_with(&studio_entry(), Some("laptop"), &overlay_provider());
        clear_add_env();
        assert!(out.is_err(), "an unpinnable peer must not become a target");
    }

    /// Recovery and inverse: a peer the saved roster already pins needs no
    /// write and is confirmed from the saved entry; one the saved roster no
    /// longer holds is refused, whatever the caller's snapshot said.
    #[test]
    #[serial_test::serial]
    fn an_already_pinned_peer_is_confirmed_from_the_saved_roster() {
        let tmp = isolated_add_env("laptop");
        let mut e = studio_entry();
        e.node_id = Some("nSTUDIO".into());
        let absent = peer_target_with(&e, Some("laptop"), &overlay_provider()).map_err(|e| format!("{e:#}"));
        fleet::mutate_roster(|r| {
            r.machines.insert("studio".into(), e.clone());
            Ok(())
        })
        .unwrap();
        let present = peer_target_with(&e, Some("laptop"), &overlay_provider()).map_err(|e| format!("{e:#}"));
        clear_add_env();
        drop(tmp);
        assert!(absent.is_err(), "a snapshot cannot vouch for an entry the roster no longer holds");
        assert_eq!(present.unwrap().node_id(), Some("nSTUDIO"));
    }

}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use darkmux_fleet::{test_node, StaticIdentityProvider};

    fn provider() -> StaticIdentityProvider {
        StaticIdentityProvider {
            local: test_node("nSTUDIO", "studio", "100.64.0.2"),
            peers: vec![test_node("nLAPTOP", "laptop", "100.64.0.7"), test_node("nPHONE", "peer", "100.64.0.9")],
            down: None,
        }
    }

    fn registry() -> darkmux_types::ProfileRegistry {
        serde_json::from_str(
            r#"{"profiles":{"host":{"models":[{"id":"big","n_ctx":32000}]},
                "coder-studio":{"models":[{"id":"big","n_ctx":64000}]},
                "utility":{"models":[{"id":"small","n_ctx":8000}]}},
              "internal":{"utility":{"id":"small"}}}"#,
        )
        .unwrap()
    }

    fn roles() -> Vec<(String, bool)> {
        vec![("radio-host".into(), false), ("coder".into(), false), ("radio-router".into(), true)]
    }

    #[test]
    fn machine_add_pins_an_overlay_node_and_flags_a_lan_address() {
        assert_eq!(
            pin_address(&provider(), "100.64.0.7:8765"),
            PinOutcome::Pinned { node_id: "nLAPTOP".into(), shown: "laptop.tailnet-example.ts.net".into() }
        );
        assert_eq!(pin_address(&provider(), "192.168.1.20"), PinOutcome::NotANode);
        let mut down = provider();
        down.down = Some("x".into());
        assert!(matches!(pin_address(&down, "100.64.0.7"), PinOutcome::Unverified(_)));
    }

    /// `trust_at` with the common test scope: role `radio-host`, no images.
    fn ta(
        p: &std::path::Path,
        name: &str,
        node: Option<&str>,
        profiles: &[String],
        workspace: Option<bool>,
        provider: &dyn fleet::IdentityProvider,
        roster_host: Option<&str>,
    ) -> Result<String> {
        let roles_arg = vec!["radio-host".to_string()];
        let req = TrustRequest { name, node_hint: node, profiles, roles: &roles_arg, images: None, workspace };
        trust_at(p, &req, provider, &registry(), &roles(), roster_host)
    }

    fn cfg(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("config.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn entry(p: &std::path::Path, name: &str) -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        v["fleet"]["accept_work"][name].clone()
    }

    #[test]
    fn trust_resolves_the_node_through_the_provider_and_touches_only_its_key() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, r#"{"machine_id":"studio","fleet":{"mode":"hub","accept_work":{"mini":{"node_id":"nMINI","profiles":["host"]}}},"redis":{"enabled":true}}"#);
        let before: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        // The roster names the laptop `laptop` at its tailnet DNS name; the
        // node is found by that host, never typed.
        let out = ta(&p, "workbook", None, &["host".into()], None, &provider(), Some("laptop.tailnet-example.ts.net")).unwrap();
        assert!(out.contains("accepts work from `workbook`"), "{out}");
        assert!(!out.contains("nLAPTOP"), "the node id is never printed: {out}");
        let e = entry(&p, "workbook");
        assert_eq!(e["node_id"], "nLAPTOP");
        assert_eq!(e["profiles"], serde_json::json!(["host"]));
        assert_eq!(e["workspace"], false);
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let mut after_minus = after.clone();
        after_minus["fleet"]["accept_work"].as_object_mut().unwrap().remove("workbook");
        assert_eq!(after_minus, before, "nothing but fleet.accept_work.workbook changed");
    }

    /// The receiver's refusal remedy, fed back to the real `machine trust`
    /// parser and core, admits the sender under the entry the allow-list and
    /// roster already key it by. Following it must not add a second key for
    /// the same node.
    #[test]
    fn the_refusal_remedy_targets_the_existing_allow_list_key() {
        use clap::Parser;
        let node = provider().peers[0].clone();
        let roster: fleet::FleetRoster = serde_json::from_value(serde_json::json!({
            "machines": { "Laptop-Mac": { "id": "Laptop-Mac", "address": "laptop.tailnet-example.ts.net:8765", "added_unix_ms": 1 } }
        }))
        .unwrap();
        let job = serde_json::json!({ "job": { "role_id": "radio-host", "published_by_machine": "laptop" } });
        let ask = fleet::TrustAsk::for_sender(&node, &roster, &serde_json::to_vec(&job).unwrap());
        let said = fleet::Refusal::NotAllowed { node_name: node.name.clone(), ask }.reason("studio");
        let command = said.split('`').nth(1).expect("the remedy is one backticked command");

        let argv: Vec<String> = command.replace("<profile>", "host").split_whitespace().map(str::to_string).collect();
        let crate::cli::Cmd::Machine { sub: Some(crate::cli::MachineCmd::Trust { name, node: node_hint, profiles, roles: asked_roles, .. }) } =
            crate::cli::Cli::try_parse_from(&argv).expect("the printed command parses").command
        else {
            panic!("`{command}` did not parse as `machine trust`");
        };

        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, r#"{"fleet":{"accept_work":{"Laptop-Mac":{"node_id":"nOLD","profiles":["coder-studio"],"roles":["coder"]}}}}"#);
        let roster_host = fleet::find_machine(&roster, &name).unwrap().and_then(|e| fleet::address_host(&e.address));
        let req = TrustRequest { name: &name, node_hint: node_hint.as_deref(), profiles: &profiles, roles: &asked_roles, ..Default::default() };
        trust_at(&p, &req, &provider(), &registry(), &roles(), roster_host.as_deref()).unwrap();

        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let keys: Vec<&String> = after["fleet"]["accept_work"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["Laptop-Mac"], "the remedy `{command}` must update the existing key, not add one");
        assert_eq!(entry(&p, "Laptop-Mac")["node_id"], "nLAPTOP");
    }

    #[test]
    fn trust_refuses_utility_and_unknown_profiles_and_requires_a_scope() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, "{}");
        let err = ta(&p, "laptop", None, &["utility".into()], None, &provider(), None).unwrap_err();
        assert!(err.to_string().contains("utility model"), "{err}");
        let err = ta(&p, "laptop", None, &["nope".into()], None, &provider(), None).unwrap_err();
        assert!(err.to_string().contains("not defined"), "{err}");
        let err = ta(&p, "laptop", None, &[], None, &provider(), None).unwrap_err();
        assert!(err.to_string().contains("name the profiles"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{}", "a refused trust writes nothing");
    }

    /// (#2916 review C4/C5/M2) Self is refused; the OS host name never
    /// resolves a node; utility and unknown roles are refused; the
    /// confirmation shows online state and owner; names are
    /// case-insensitive; images are validated and listed.
    #[test]
    fn trust_refuses_self_utility_roles_and_host_names_and_shows_owner() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, "{}");
        let err = ta(&p, "studio", None, &["host".into()], None, &provider(), None).unwrap_err();
        assert!(err.to_string().contains("THIS machine's own node"), "{err}");
        let mut hostnamed = provider();
        hostnamed.peers[0].host_name = Some("Evil Host".into());
        let err = ta(&p, "evil", Some("Evil Host"), &["host".into()], None, &hostnamed, None).unwrap_err();
        assert!(err.to_string().contains("no node"), "the OS host name must not resolve a node: {err}");
        let profiles = vec!["host".to_string()];
        for (r, want) in [("radio-router", "utility role"), ("nope", "not defined")] {
            let roles_arg = vec![r.to_string()];
            let req = TrustRequest { name: "laptop", profiles: &profiles, roles: &roles_arg, ..Default::default() };
            let err = trust_at(&p, &req, &provider(), &registry(), &roles(), None).unwrap_err();
            assert!(err.to_string().contains(want), "{err}");
        }
        let no_roles: Vec<String> = vec![];
        let req = TrustRequest { name: "laptop", profiles: &profiles, roles: &no_roles, ..Default::default() };
        let err = trust_at(&p, &req, &provider(), &registry(), &roles(), None).unwrap_err();
        assert!(err.to_string().contains("name the roles"), "{err}");
        let coder = vec!["coder".to_string()];
        let bad_images = vec!["-v /:/x".to_string()];
        let req = TrustRequest { name: "laptop", profiles: &profiles, roles: &coder, images: Some(&bad_images), ..Default::default() };
        assert!(trust_at(&p, &req, &provider(), &registry(), &roles(), None).is_err(), "an invalid image reference is refused");
        let images = vec!["rust:slim".to_string()];
        let req = TrustRequest { name: "laptop", profiles: &profiles, roles: &coder, images: Some(&images), ..Default::default() };
        let out = trust_at(&p, &req, &provider(), &registry(), &roles(), None).unwrap();
        assert!(out.contains("(online; owner: operator)"), "{out}");
        assert!(out.contains("roles: coder") && out.contains("images: rust:slim"), "{out}");
        assert_eq!(entry(&p, "laptop")["images"], serde_json::json!(["rust:slim"]));
        // Re-trust by another spelling updates the same entry.
        ta(&p, "LAPTOP", Some("laptop"), &["coder-studio".into()], None, &provider(), None).unwrap();
        assert_eq!(entry(&p, "laptop")["profiles"], serde_json::json!(["coder-studio"]));
        assert!(entry(&p, "LAPTOP").is_null());
        assert!(untrust_at(&p, "Laptop").unwrap(), "untrust is case-insensitive too");
    }

    #[test]
    fn trust_refuses_a_name_the_network_does_not_know_and_names_what_it_does() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, "{}");
        let err = ta(&p, "ghost", None, &["host".into()], None, &provider(), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no node") && msg.contains("laptop") && msg.contains("peer"), "{msg}");
        // --node overrides the name.
        ta(&p, "ghost", Some("peer"), &["host".into()], None, &provider(), None).unwrap();
        assert_eq!(entry(&p, "ghost")["node_id"], "nPHONE");
    }

    #[test]
    fn trust_refuses_when_the_provider_is_down() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, "{}");
        let mut down = provider();
        down.down = Some("daemon not running".into());
        let err = ta(&p, "laptop", None, &["host".into()], None, &down, None).unwrap_err();
        assert!(err.to_string().contains("could not list"), "{err}");
    }

    #[test]
    fn retrust_keeps_the_scope_unless_given_and_untrust_removes_only_that_entry() {
        let d = tempfile::TempDir::new().unwrap();
        let p = cfg(&d, "{}");
        ta(&p, "laptop", None, &["host".into(), "coder-studio".into()], Some(true), &provider(), None).unwrap();
        ta(&p, "peer", None, &["host".into()], None, &provider(), None).unwrap();
        ta(&p, "laptop", None, &[], None, &provider(), None).unwrap();
        let e = entry(&p, "laptop");
        assert_eq!(e["profiles"], serde_json::json!(["host", "coder-studio"]));
        assert_eq!(e["workspace"], true);
        assert!(untrust_at(&p, "laptop").unwrap());
        assert!(entry(&p, "laptop").is_null());
        assert_eq!(entry(&p, "peer")["node_id"], "nPHONE", "the other entry stays");
        assert!(!untrust_at(&p, "laptop").unwrap(), "a second untrust changes nothing");
    }

    /// The doctor gatherer end to end, with a fake provider tool first on
    /// PATH and an isolated home: the rows name who is trusted, flag a
    /// profile that cannot run here, and print no node id.
    #[serial_test::serial]
    #[test]
    fn doctor_rows_report_the_allow_list_through_the_provider() {
        let d = tempfile::TempDir::new().unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("tailscale");
        std::fs::write(
            &tool,
            "#!/bin/sh\n[ \"$1\" = status ] || exit 2\necho '{\"BackendState\":\"Running\",\"Self\":{\"ID\":\"nSTUDIO\",\"DNSName\":\"studio.tailnet-example.ts.net.\",\"TailscaleIPs\":[\"100.64.0.2\"]},\"Peer\":{\"k\":{\"ID\":\"nLAPTOP\",\"DNSName\":\"laptop.tailnet-example.ts.net.\",\"TailscaleIPs\":[\"100.64.0.7\"],\"Online\":true}}}'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let home = d.path().join("dm");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.json"),
            r#"{"fleet":{"accept_work":{"workbook":{"node_id":"nLAPTOP","profiles":["host","nope"],"roles":["radio-host"]},"gone":{"node_id":"nGONE","profiles":["host"]}}}}"#,
        )
        .unwrap();
        let profiles = d.path().join("profiles.json");
        std::fs::write(&profiles, r#"{"profiles":{"host":{"models":[{"id":"big","n_ctx":1000}]}}}"#).unwrap();
        let keys = ["PATH", "DARKMUX_HOME", "DARKMUX_PROFILES", "DARKMUX_REDIS_URL", "DARKMUX_SERVE_TOKEN", "DARKMUX_FLEET_LISTENER_ENABLED"];
        let prev: Vec<(&str, Option<String>)> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
            std::env::set_var("DARKMUX_HOME", &home);
            std::env::set_var("DARKMUX_PROFILES", &profiles);
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_SERVE_TOKEN", "t");
            std::env::remove_var("DARKMUX_FLEET_LISTENER_ENABLED");
        }
        let rows = fleet_submission_doctor_checks();
        unsafe {
            for (k, v) in prev {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let find = |n: &str| rows.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("no {n} in {rows:?}"));
        assert_eq!(find("fleet token").status, crate::doctor::Status::Pass);
        assert!(find("fleet identity").message.contains("this machine is `studio` at 100.64.0.2"), "{rows:?}");
        assert!(find("fleet listener").message.starts_with("off"), "{rows:?}");
        let t = find("fleet trust");
        assert_eq!(t.status, crate::doctor::Status::Warn);
        assert!(t.message.contains("workbook may run host, nope (roles: radio-host; images: runtime only; workspace: no)"), "{}", t.message);
        assert!(t.message.contains("node `laptop`, online"), "{}", t.message);
        assert!(t.message.contains("`nope` is not defined"), "{}", t.message);
        assert!(t.message.contains("gone may run host") && t.message.contains("no longer on the network"), "{}", t.message);
        for c in &rows {
            assert!(!c.message.contains("nLAPTOP") && !c.message.contains("nGONE"), "{}", c.message);
        }
    }

    // ── machine resources <peer>: the daemon's whole response, or an honest note ──

    /// A peer's `/machine/resources` body: a real ledger plus the two fields
    /// the daemon adds to it.
    fn peer_resources_body() -> serde_json::Value {
        let ledger = darkmux_profiles::model_ledger::gather_with_bin("/nonexistent-lms");
        let mut body = serde_json::to_value(ledger).unwrap();
        body["cache_ttl_ms"] = serde_json::json!(2000);
        body["load"] = serde_json::json!({
            "battery_health": null,
            "now": { "sampled_at_ms": 5, "sampler_cost_ms": 1 },
            "window": {
                "samples": 3, "span_ms": 4000, "interval_ms": 2000,
                "cpu_pct": { "mean": 10.0, "p95": 20, "max": 30 },
                "gpu_pct": { "mean": null, "p95": null, "max": null },
                "mem_pct": { "mean": 50.0, "p95": 51, "max": 52 },
                "power_mw": null, "thermal": null, "energy_mwh": null
            }
        });
        body
    }

    #[test]
    fn a_peers_cadence_and_load_reach_the_json_output() {
        let out = peer_resources_view("studio", peer_resources_body(), true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cache_ttl_ms"], 2000, "the recorded cadence knob is dropped: {out}");
        assert_eq!(v["load"]["window"]["samples"], 3, "the sampler's load is dropped: {out}");
    }

    #[test]
    fn a_peer_without_a_sampler_prints_no_load_key() {
        let mut body = peer_resources_body();
        body.as_object_mut().unwrap().remove("load");
        let v: serde_json::Value = serde_json::from_str(&peer_resources_view("studio", body, true).unwrap()).unwrap();
        assert!(v.get("load").is_none());
        assert_eq!(v["cache_ttl_ms"], 2000);
    }

    #[test]
    fn a_peer_body_this_build_cannot_read_is_a_note_in_text_naming_its_version() {
        let mut body = peer_resources_body();
        body["limit_source"] = serde_json::json!(12);
        body["darkmux_version"] = serde_json::json!("9.9.9");
        let out = peer_resources_view("studio", body, false).unwrap();
        assert!(out.contains("resources unreadable"), "{out}");
        assert!(out.contains("schema_version 2.1") && out.contains("darkmux_version 9.9.9"), "{out}");
        assert!(out.contains("integer `12`"), "what could not be read is named: {out}");
    }

    #[test]
    fn a_refusal_under_json_names_the_peer_version_and_claims_no_mismatch_it_did_not_see() {
        let mut body = peer_resources_body();
        body["limit_source"] = serde_json::json!(12);
        let err = peer_resources_view("studio", body.clone(), true).unwrap_err().to_string();
        assert!(err.contains("schema_version 2.1") && err.contains("the schema matches"), "{err}");
        assert!(!err.contains("differs"), "a version mismatch nobody observed is claimed: {err}");
        body["schema_version"] = serde_json::json!("3.0");
        let err = peer_resources_view("studio", body, true).unwrap_err().to_string();
        assert!(err.contains("schema_version 3.0") && err.contains("reads ledger schema 2.1"), "{err}");
    }

    #[test]
    fn a_peer_body_with_no_versions_says_so() {
        let err = peer_resources_view("studio", serde_json::json!({"x": 1}), true).unwrap_err().to_string();
        assert!(err.contains("reports no schema_version or darkmux_version"), "{err}");
    }

}

