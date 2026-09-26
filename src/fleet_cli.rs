//! `darkmux machine` roster-facing command handlers (#1426 — the retired
//! `fleet add`/`remove`/`status` folded into the machine family: `machine
//! add`/`machine remove`/`machine list`). The `MachineCmd` arg surface lives
//! in `cli.rs`; `cmd_machine` in `main.rs` routes the roster sub-verbs here.
//! The "fleet" concept survives (roster, `fleet.mode`, flow records) — only
//! the CLI family moved under `machine`.

use anyhow::Result;

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
    let local_id = flow::resolve_machine_id();
    let is_self_entry = local_id.as_deref() == Some(id);
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

/// What this doctor run learned without reading flow history.
struct LiveIdentity {
    /// Presence beats as `(uid, display_name)`.
    beats: Vec<(String, String)>,
    /// This machine's hardware uid, when readable.
    local_uid: Option<String>,
    /// This machine's resolved machine_id.
    local_name: Option<String>,
    /// True when `local_name` came from the `DARKMUX_MACHINE_ID` env tier.
    local_name_from_env: bool,
    presence: crate::doctor::PresenceState,
}

/// (#2924 C-5) How many of the most recent flow FILES the roster identity
/// check reads (one file per day in practice, but it counts files: a day
/// with no records has none). Bounds doctor's cost as history is retained without
/// limit (the laptop holds ~130 days, ~300 MB). 120 covers the live rename
/// this check was written for; a rename older than the window is reported as
/// a note ("matches no machine_id this machine can see"), never a warning.
const ROSTER_HISTORY_FILES: usize = 120;

/// True when some roster entry cannot be settled from live knowledge alone:
/// it declares no uid and is not a current name, or declares a uid nobody
/// live answers to. History can only change the verdict for those.
fn roster_needs_history(roster: &fleet::FleetRoster, live: &crate::doctor::FleetIdentityKnowledge) -> bool {
    roster.machines.values().any(|m| match &m.machine_uid {
        Some(uid) => !live.current_name_by_uid.contains_key(uid),
        None => !live.is_current_name(&m.id),
    })
}

/// A flow record's top-level `(machine_id, machine_uid)`, or `None` when it
/// carries no machine_id.
///
/// Measured cost is why this is not a plain `serde_json` parse: a full
/// history on the laptop is ~300 MB, and parsing every line added ~4 s to a
/// debug `darkmux doctor`. darkmux writes both fields as flat strings among
/// the record's leading scalar fields, so the fast path reads them by key,
/// accepting a match only when no `{` precedes it (i.e. it is not inside a
/// nested object). Any other shape falls back to a real parse.
fn record_identity(line: &str) -> Option<(String, Option<String>)> {
    fn flat_field<'a>(line: &'a str, key: &str) -> Option<Option<&'a str>> {
        let pat = format!("\"{key}\":\"");
        let Some(at) = line.find(&pat) else {
            return if line.contains(&format!("\"{key}\"")) { None } else { Some(None) };
        };
        if line.get(1..at).is_some_and(|pre| pre.contains('{')) {
            return None;
        }
        let rest = &line[at + pat.len()..];
        let end = rest.find('"')?;
        let v = &rest[..end];
        if v.contains('\\') {
            return None;
        }
        Some(Some(v))
    }
    let fast = flat_field(line, "machine_id").zip(flat_field(line, "machine_uid"));
    let (id, uid) = match fast {
        Some((id, uid)) => (id.map(str::to_string), uid.map(str::to_string)),
        None => {
            #[derive(serde::Deserialize)]
            struct Ids {
                machine_id: Option<String>,
                machine_uid: Option<String>,
            }
            let ids: Ids = serde_json::from_str(line).ok()?;
            (ids.machine_id, ids.machine_uid)
        }
    };
    let id = id.filter(|n| !n.is_empty())?;
    Some((id, uid.filter(|u| !u.is_empty())))
}

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

/// Build [`crate::doctor::FleetIdentityKnowledge`] from the last
/// [`ROSTER_HISTORY_FILES`] flow day-files, presence beats, and this
/// machine's own resolution. Later sources override earlier ones for a uid's
/// CURRENT name. Every uid a name was ever seen under is kept (a set, never
/// last-writer-wins), because one machine collects throwaway session names
/// and two machines can once have shared a hostname-derived id.
///
/// Flow files are read in name (date) order so the last name a uid wrote is
/// its current one by history.
fn gather_identity_knowledge(
    flows_dir: Option<&std::path::Path>,
    live: &LiveIdentity,
) -> crate::doctor::FleetIdentityKnowledge {
    use std::io::BufRead;
    let mut known = crate::doctor::FleetIdentityKnowledge::default();
    let mut files: Vec<std::path::PathBuf> = flows_dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let skip = files.len().saturating_sub(ROSTER_HISTORY_FILES);
    if skip > 0 {
        known.history_truncated_to = Some(ROSTER_HISTORY_FILES);
    }
    for path in files.into_iter().skip(skip) {
        let Ok(file) = std::fs::File::open(&path) else { continue };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Some((name, uid)) = record_identity(&line) else { continue };
            match uid {
                Some(uid) => {
                    known.uids_by_name.entry(name.clone()).or_default().insert(uid.clone());
                    known.current_name_by_uid.insert(uid, name);
                }
                None => {
                    known.uidless_names.insert(name);
                }
            }
        }
    }
    // (#2924 C-b) A `DARKMUX_MACHINE_ID` override is a per-shell name, not
    // this machine's name for the fleet, so it is not overlaid onto the local
    // uid; that uid keeps whatever presence or history says.
    let local_overlay = if live.local_name_from_env {
        None
    } else {
        live.local_uid.as_deref().zip(live.local_name.as_deref())
    };
    let overlays = live
        .beats
        .iter()
        .map(|(u, n)| (u.as_str(), n.as_str()))
        .chain(local_overlay);
    for (uid, name) in overlays {
        known.uids_by_name.entry(name.to_string()).or_default().insert(uid.to_string());
        known.current_name_by_uid.insert(uid.to_string(), name.to_string());
        known.live_uids.insert(uid.to_string());
    }
    known.local_name = live.local_name.clone();
    known.local_name_from_env = live.local_name_from_env;
    known.local_uid = live.local_uid.clone();
    known.presence = live.presence;
    known
}

pub(crate) fn cmd_machine_remove(id: &str) -> Result<i32> {
    let removed = fleet::mutate_roster(|roster| Ok(fleet::remove_machine(roster, id)))?;
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

/// Resolve a roster `id` to its normalized daemon base URL, then GET `path`
/// with the shared fleet bearer token (#1426, #881). Used by `machine status
/// [id]` / `machine resources [id]` to read a peer over its serve daemon —
/// the same shared-token mechanism `machine list --deep` uses. Reads only;
/// mutations never target a peer.
pub(crate) fn fetch_peer_json(id: &str, path: &str) -> Result<serde_json::Value> {
    let roster = fleet::load_roster()?;
    let entry = roster.machines.get(id).ok_or_else(|| {
        anyhow::anyhow!(
            "no machine `{id}` in roster — add it with `darkmux machine add {id} --address <dns-name>`, \
             or omit the id to read this host"
        )
    })?;
    let local_id = flow::resolve_machine_id();
    let dialed = dial_address(entry, local_id.as_deref(), &darkmux_types::config_access::serve_client_addr());
    let base = normalize_daemon_base(&dialed);
    let url = format!("{base}{path}");
    let token = darkmux_flow::serve_token();
    let token_str = token.as_ref().map(|t| t.expose_for_compare());
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(2000))
        .build();
    let mut req = agent.get(&url);
    if let Some(tok) = token_str {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => {
            let body = resp
                .into_string()
                .map_err(|e| anyhow::anyhow!("reading response from `{id}` ({url}): {e}"))?;
            serde_json::from_str(&body)
                .map_err(|e| anyhow::anyhow!("parsing JSON from `{id}` ({url}): {e}"))
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
    if local_id == Some(entry.id.as_str()) && !fleet::address_host_is_loopback(&entry.address) {
        local_addr.to_string()
    } else {
        entry.address.clone()
    }
}

/// Reachability probe for every roster entry, dialing each at
/// [`dial_address`]. Returns `(entry, dialed address, probe)`.
fn list_probes(
    roster: &fleet::FleetRoster,
    local_id: Option<&str>,
) -> Vec<(fleet::MachineEntry, String, fleet::ReachabilityResult)> {
    let local_addr = darkmux_types::config_access::serve_client_addr();
    roster
        .machines
        .values()
        .map(|m| {
            let dialed = dial_address(m, local_id, &local_addr);
            let probe = fleet::probe_reachability(&dialed);
            (m.clone(), dialed, probe)
        })
        .collect()
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

/// Normalize a roster address into an `http://host:port` daemon base URL,
/// mirroring `fetch_machine_specs`' normalization (IPv6 / port-less forms).
fn normalize_daemon_base(address: &str) -> String {
    if address.contains("://") {
        address.trim_end_matches('/').to_string()
    } else if address.contains(':') {
        format!("http://{address}")
    } else {
        format!("http://{address}:{}", crate::serve::DEFAULT_DAEMON_PORT)
    }
}

pub(crate) fn cmd_machine_list(emit_json: bool, deep: bool) -> Result<i32> {
    let roster = fleet::load_roster()?;

    // Probe each machine's reachability (TCP connect to its daemon port).
    // Done sequentially — the roster is small and the budget per probe
    // is 300ms; total wall is bounded. This machine's own entry is dialed at
    // the local daemon (#2924, `dial_address`).
    let local_id = flow::resolve_machine_id();
    let probes = list_probes(&roster, local_id.as_deref());

    // When --deep, fetch /machine/specs from each reachable peer. One
    // HTTP GET per peer; ~1s budget each. Failures are surfaced per-row
    // (Some(None) in the resolved vector) — they MUST NOT fail the
    // whole command. (#275 PR-B)
    // (#881) Resolve THIS machine's serve token once and send it to peers — a
    // single shared fleet token. Track peers that answered 401/403 so a missing
    // token surfaces a real "auth?" signal instead of looking like a timeout.
    let token = darkmux_flow::serve_token();
    let token_str = token.as_ref().map(|t| t.expose_for_compare());
    let mut auth_required: Vec<String> = Vec::new();
    // (#1849) Peers that answered with a 404 on `/machine/specs` — reachable,
    // but no route, distinct from a generic probe failure.
    let mut route_missing: Vec<String> = Vec::new();
    let specs_by_id: std::collections::BTreeMap<String, Option<serde_json::Value>> = if deep {
        probes
            .iter()
            .map(|(m, dialed, p)| {
                let value = if p.reachable {
                    match fetch_machine_specs(dialed, token_str) {
                        SpecsProbe::Ok(v) => Some(v),
                        SpecsProbe::AuthRequired => {
                            auth_required.push(m.id.clone());
                            None
                        }
                        SpecsProbe::RouteMissing => {
                            route_missing.push(m.id.clone());
                            None
                        }
                        SpecsProbe::Unavailable => None,
                    }
                } else {
                    None
                };
                (m.id.clone(), value)
            })
            .collect()
    } else {
        std::collections::BTreeMap::new()
    };

    if emit_json {
        // (#776) Machine-readable output stays byte-clean: force color off so
        // any accidental downstream style call can't leak ANSI into the JSON.
        darkmux_types::style::set_colorize_override(Some(false));
        let local_id = flow::resolve_machine_id();
        let payload = serde_json::json!({
            "roster_path": fleet::roster_path().display().to_string(),
            "roster_version": roster.version,
            "local_machine_id": local_id,
            "machines": probes
                .iter()
                .map(|(m, dialed, p)| serde_json::json!({
                    "id": m.id,
                    "address": m.address,
                    // (#2924) Where the probe went: the roster address, or the
                    // local daemon for this machine's own entry.
                    "dialed_address": dialed,
                    "is_this_machine": local_id.as_deref() == Some(m.id.as_str()),
                    "description": m.description,
                    "added_unix_ms": m.added_unix_ms,
                    // (#2768) `null` for a remote peer or a pre-#2768 entry —
                    // "unknown identity", never "same machine as another
                    // null entry". See `MachineEntry::machine_uid`'s own doc.
                    "machine_uid": m.machine_uid,
                    "reachable": p.reachable,
                    "resolved_address": p.resolved_address,
                    "probe_ms": p.elapsed_ms,
                    "probe_error": p.error,
                    // Only present when --deep was passed; null when
                    // --deep was passed but the fetch failed.
                    "specs": specs_by_id.get(&m.id).cloned().flatten().unwrap_or(serde_json::Value::Null),
                    // (#881) Distinguish a null `specs` caused by a 401/403
                    // (this machine isn't sending the shared fleet token) from a
                    // timeout/other failure, so a consumer (viewer/script) gets
                    // the same signal the text table's `auth?` column carries.
                    "specs_auth_required": auth_required.contains(&m.id),
                    // (#1849) Distinguish a null `specs` caused by a 404 (the
                    // peer answered but has no `/machine/specs` route) from a
                    // generic probe failure — the same signal the text
                    // table's `no-route?` column carries.
                    "specs_route_missing": route_missing.contains(&m.id),
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }

    // Human-readable table.
    use darkmux_types::style;
    println!("{}", style::header("darkmux machine list"));
    println!(
        "  roster:           {}",
        style::dim(&fleet::roster_path().display().to_string())
    );
    println!(
        "  local machine_id: {}",
        style::dim(&flow::resolve_machine_id().unwrap_or_else(|| "<unknown>".into()))
    );
    println!();
    if probes.is_empty() {
        println!("(no peers in roster — single-machine fleet)");
        println!();
        println!("Add a peer: darkmux machine add <id> --address <dns-name>");
        return Ok(0);
    }
    // Column-header row dimmed as secondary structure. Styling wraps the
    // WHOLE line (color codes at the line edges), so column alignment — which
    // counts visible chars inside the format — is preserved.
    if deep {
        println!(
            "{}",
            style::dim(&format!(
                "{:<14} {:<22} {:<10} {:<11} {:<10} VERSION  MODELS",
                "MACHINE", "ADDRESS", "PROBE", "AI-HEADROOM", "OS"
            ))
        );
    } else {
        println!(
            "{}",
            style::dim(&format!(
                "{:<14} {:<26} {:<10} DESCRIPTION",
                "MACHINE", "ADDRESS", "PROBE"
            ))
        );
    }
    if let Some((m, dialed, _)) = probes.iter().find(|(m, _, _)| local_id.as_deref() == Some(m.id.as_str())) {
        println!(
            "{}",
            style::dim(&format!(
                "  `{}` is this machine: probed at its local daemon {dialed}; the address below is what peers dial.",
                m.id
            ))
        );
    }
    for (m, _dialed, p) in &probes {
        let status = if p.reachable {
            format!("✓ {}ms", p.elapsed_ms)
        } else {
            format!("✗ {}ms", p.elapsed_ms)
        };
        if deep {
            let specs = specs_by_id.get(&m.id).cloned().unwrap_or(None);
            let (ram_free, os_str, version, models_summary) = match &specs {
                Some(s) => {
                    let ram = s
                        .get("ram_free_for_ai_bytes")
                        .and_then(|v| v.as_u64())
                        .map(human_gb)
                        .unwrap_or_else(|| "—".into());
                    let os = s
                        .get("os")
                        .and_then(|v| v.as_str())
                        .unwrap_or("—")
                        .to_string();
                    let v = s
                        .get("darkmux_version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("—")
                        .to_string();
                    let models = s
                        .get("loaded_models")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m.get("identifier").and_then(|i| i.as_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_else(|| "—".into());
                    (
                        ram,
                        os,
                        v,
                        if models.is_empty() {
                            "—".into()
                        } else {
                            models
                        },
                    )
                }
                // (#881) Distinguish a 401/403 (peer requires a token we didn't
                // send) from a generic specs failure, so it doesn't read as a
                // timeout.
                None if auth_required.contains(&m.id) => {
                    ("auth?".into(), "—".into(), "—".into(), "—".into())
                }
                // (#1849) Distinguish a 404 (peer reachable, no
                // `/machine/specs` route) from a generic specs failure —
                // this is the exact shape a bare-IP peer behind `tailscale
                // serve` produces, and it must not render identically to
                // an unreachable/timed-out peer.
                None if route_missing.contains(&m.id) => {
                    ("no-route?".into(), "—".into(), "—".into(), "—".into())
                }
                None => ("specs?".into(), "—".into(), "—".into(), "—".into()),
            };
            let row = format!(
                "{:<14} {:<22} {:<10} {:<11} {:<10} {:<8} {}",
                m.id, m.address, status, ram_free, os_str, version, models_summary
            );
            // Fade unreachable peers (whole-line dim — alignment-safe).
            println!("{}", if p.reachable { row } else { style::dim(&row) });
        } else {
            let desc = m.description.as_deref().unwrap_or("");
            let row = format!(
                "{:<14} {:<26} {:<10} {}",
                m.id, m.address, status, desc
            );
            println!("{}", if p.reachable { row } else { style::dim(&row) });
        }
        if let Some(err) = &p.error {
            println!("{}", style::error(&format!("               error: {err}")));
        }
    }
    // (#881) If any peer returned 401/403, the local machine is missing the
    // shared fleet token — surface the fix rather than leaving a silent "auth?".
    if !auth_required.is_empty() {
        println!(
            "{}",
            style::warn(&format!(
                "  ! {} peer(s) require a bearer token this machine isn't sending ({}). \
Set DARKMUX_SERVE_TOKEN (or the darkmux-serve-token Keychain item) to the shared fleet token.",
                auth_required.len(),
                auth_required.join(", ")
            ))
        );
    }
    // (#1849) If any peer answered but has no `/machine/specs` route,
    // surface the fix rather than leaving a silent "no-route?" — and branch
    // by cause the same way `route_missing_message` does for a single peer.
    if !route_missing.is_empty() {
        println!(
            "{}",
            style::warn(&format!(
                "  ! {} peer(s) answered but have no `/machine/specs` route ({}) — it may be \
running an older darkmux (route not found). Upgrade darkmux on those peer(s) and retry.",
                route_missing.len(),
                route_missing.join(", ")
            ))
        );
        let bare_ip_peers: Vec<&str> = probes
            .iter()
            .filter(|(m, dialed, _)| {
                route_missing.contains(&m.id) && fleet::address_host_is_bare_ip(dialed)
            })
            .map(|(m, _, _)| m.id.as_str())
            .collect();
        if !bare_ip_peers.is_empty() {
            println!(
                "{}",
                style::warn(&format!(
                    "    {} of those ({}) are addressed by bare IP — a peer behind `tailscale \
serve` answers a bare IP with its own 404 too. If that's the shape, re-add it by DNS name \
(`darkmux machine add <id> --address <dns-name>:8765`) instead.",
                    bare_ip_peers.len(),
                    bare_ip_peers.join(", ")
                ))
            );
        }
    }
    Ok(0)
}

/// Outcome of probing a peer's `/machine/specs` (#881). `AuthRequired`
/// (401/403) is distinguished from `Unavailable` (timeout, refused, other
/// non-2xx, bad JSON) so a missing shared fleet token reads as `auth?`, not a
/// silent `specs?`. `RouteMissing` (404, #1849) is likewise distinguished —
/// the peer IS reachable and answered, its daemon just doesn't serve this
/// route (an older darkmux, or a bare-IP request landing on `tailscale
/// serve`'s own 404 instead of the daemon's) — a generic `specs?` would
/// read exactly like the unreachable-peer case this command already tells
/// apart via the PROBE column, hiding the #1849 shape from the one surface
/// (`machine list --deep`) that showcases it in the docs.
enum SpecsProbe {
    Ok(serde_json::Value),
    AuthRequired,
    RouteMissing,
    Unavailable,
}

/// Fetch `/machine/specs` from a peer's daemon at `address`, sending the shared
/// fleet bearer `token` if one is configured (#881). Bounded at 1s total — the
/// operator gets a row per peer even when one is slow or wedged. (#275 PR-B)
fn fetch_machine_specs(address: &str, token: Option<&str>) -> SpecsProbe {
    let normalized = if address.contains("://") {
        address.to_string()
    } else if address.contains(':') {
        format!("http://{address}")
    } else {
        // (#907) Use the typed port const — string-splitting the addr is
        // wrong for IPv6 / port-less forms.
        format!("http://{address}:{}", crate::serve::DEFAULT_DAEMON_PORT)
    };
    let url = format!("{normalized}/machine/specs");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(1000))
        .build();
    let mut req = agent.get(&url);
    if let Some(tok) = token {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => match resp.into_string() {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(v) => SpecsProbe::Ok(v),
                Err(_) => SpecsProbe::Unavailable,
            },
            Err(_) => SpecsProbe::Unavailable,
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            SpecsProbe::AuthRequired
        }
        Err(ureq::Error::Status(404, _)) => SpecsProbe::RouteMissing,
        Err(_) => SpecsProbe::Unavailable,
    }
}

/// Format a byte count as a human-friendly "N GB" string for the
/// `fleet status --deep` table. Round to whole GB — the precision the
/// `AI-HEADROOM` column wants. (#275 PR-B)
fn human_gb(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{:.0} GB", gb.round())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_daemon_base (#1426) — the three roster address forms ──

    #[test]
    fn normalize_daemon_base_passes_through_full_urls_sans_trailing_slash() {
        assert_eq!(
            normalize_daemon_base("http://studio.tailnet:9000/"),
            "http://studio.tailnet:9000"
        );
        assert_eq!(
            normalize_daemon_base("https://hub.example:8765"),
            "https://hub.example:8765"
        );
    }

    #[test]
    fn normalize_daemon_base_prefixes_host_port_forms() {
        assert_eq!(
            normalize_daemon_base("100.64.0.2:8765"),
            "http://100.64.0.2:8765"
        );
    }

    #[test]
    fn normalize_daemon_base_appends_default_port_to_bare_hosts() {
        assert_eq!(
            normalize_daemon_base("100.64.0.2"),
            format!("http://100.64.0.2:{}", crate::serve::DEFAULT_DAEMON_PORT)
        );
    }

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

    /// The e2e harness shape through `list_probes`: two fake daemons on
    /// loopback, the CLI running as node-a with no serve port configured.
    #[serial_test::serial]
    #[test]
    fn list_probes_dial_each_same_host_node_at_its_own_loopback_port() {
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (pa, pb) = (a.local_addr().unwrap().port(), b.local_addr().unwrap().port());
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "node-a", &format!("127.0.0.1:{pa}"), None, None).unwrap();
        fleet::add_machine(&mut roster, "node-b", &format!("127.0.0.1:{pb}"), None, None).unwrap();
        unsafe {
            std::env::remove_var("DARKMUX_SERVE_PORT");
            std::env::remove_var("DARKMUX_SERVE_BIND");
        }
        let probes = list_probes(&roster, Some("node-a"));
        for (m, dialed, probe) in &probes {
            assert_eq!(dialed, &m.address, "{} must be dialed at its own address", m.id);
            assert!(probe.reachable, "{}: {:?}", m.id, probe.error);
        }
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

    /// `machine list` probes this machine's entry at the local daemon.
    #[serial_test::serial]
    #[test]
    fn list_probes_probe_this_machines_entry_at_the_local_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut roster = fleet::FleetRoster::default();
        fleet::add_machine(&mut roster, "self-host", "self-host.invalid", None, None).unwrap();
        unsafe {
            std::env::set_var("DARKMUX_SERVE_PORT", port.to_string());
            std::env::set_var("DARKMUX_SERVE_BIND", "127.0.0.1");
        }
        let probes = list_probes(&roster, Some("self-host"));
        unsafe {
            std::env::remove_var("DARKMUX_SERVE_PORT");
            std::env::remove_var("DARKMUX_SERVE_BIND");
        }
        let (_, dialed, probe) = &probes[0];
        assert_eq!(dialed, &format!("127.0.0.1:{port}"));
        assert!(probe.reachable, "{:?}", probe.error);
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

    // ── fetch_machine_specs / SpecsProbe::RouteMissing (#1849 MUST FIX 2) ──

    #[test]
    fn fetch_machine_specs_404_is_route_missing_not_unavailable() {
        // Red-prove: a 404 on `/machine/specs` must render as a distinct
        // outcome from a generic probe failure — `machine list --deep`
        // renders `RouteMissing` as `no-route?`, never the same `specs?`
        // a timeout or bad-JSON response gets.
        let addr = one_shot_http("404 Not Found", "{}");
        match fetch_machine_specs(&addr, None) {
            SpecsProbe::RouteMissing => {}
            SpecsProbe::Unavailable => {
                panic!("404 read as generic Unavailable, not RouteMissing")
            }
            SpecsProbe::Ok(_) => panic!("expected RouteMissing, got Ok"),
            SpecsProbe::AuthRequired => panic!("expected RouteMissing, got AuthRequired"),
        }
    }

    #[test]
    fn fetch_machine_specs_bad_json_is_unavailable_not_route_missing() {
        // Inverted case: a generic failure (malformed body on a 200) must
        // NOT read as RouteMissing — the two outcomes stay distinguishable
        // in both directions.
        let addr = one_shot_http("200 OK", "this is not json");
        match fetch_machine_specs(&addr, None) {
            SpecsProbe::Unavailable => {}
            SpecsProbe::RouteMissing => {
                panic!("bad JSON on 200 read as RouteMissing, not Unavailable")
            }
            SpecsProbe::Ok(_) => panic!("expected Unavailable, got Ok"),
            SpecsProbe::AuthRequired => panic!("expected Unavailable, got AuthRequired"),
        }
    }
}
