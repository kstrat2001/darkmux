//! `darkmux fleet` command handlers — extracted from `main.rs` (mechanical,
//! zero behavior change) alongside the `lab_cli`/`cli` split. `FleetCmd`
//! itself (the arg surface) lives in `cli.rs`; this module owns the
//! dispatch logic `cli::run` calls into.

use anyhow::Result;

use crate::cli::FleetCmd;
use crate::fleet;
use crate::flow;

pub(crate) fn cmd_fleet(sub: FleetCmd) -> Result<i32> {
    match sub {
        FleetCmd::Add {
            id,
            address,
            description,
        } => cmd_fleet_add(&id, &address, description.as_deref()),
        FleetCmd::Remove { id } => cmd_fleet_remove(&id),
        FleetCmd::Status { json, deep } => cmd_fleet_status(json, deep),
    }
}

fn cmd_fleet_add(id: &str, address: &str, description: Option<&str>) -> Result<i32> {
    let was_present = fleet::mutate_roster(|roster| {
        let was_present = roster.machines.contains_key(id);
        fleet::add_machine(roster, id, address, description)?;
        Ok(was_present)
    })?;
    let verb = if was_present { "updated" } else { "added" };
    println!("fleet: {verb} {id} (address={address})");
    if let Some(d) = description {
        println!("  description: {d}");
    }
    println!("  roster: {}", fleet::roster_path().display());
    Ok(0)
}

fn cmd_fleet_remove(id: &str) -> Result<i32> {
    let removed = fleet::mutate_roster(|roster| Ok(fleet::remove_machine(roster, id)))?;
    match removed {
        Some(entry) => {
            println!("fleet: removed {id} (address was {})", entry.address);
            println!("  roster: {}", fleet::roster_path().display());
            Ok(0)
        }
        None => {
            eprintln!("fleet: no machine `{id}` in roster — nothing to remove");
            Ok(2)
        }
    }
}

fn cmd_fleet_status(emit_json: bool, deep: bool) -> Result<i32> {
    let roster = fleet::load_roster()?;

    // Probe each machine's reachability (TCP connect to its daemon port).
    // Done sequentially — the roster is small and the budget per probe
    // is 300ms; total wall is bounded.
    let probes: Vec<(fleet::MachineEntry, fleet::ReachabilityResult)> = roster
        .machines
        .values()
        .map(|m| {
            let probe = fleet::probe_reachability(&m.address);
            (m.clone(), probe)
        })
        .collect();

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
    let specs_by_id: std::collections::BTreeMap<String, Option<serde_json::Value>> = if deep {
        probes
            .iter()
            .map(|(m, p)| {
                let value = if p.reachable {
                    match fetch_machine_specs(&m.address, token_str) {
                        SpecsProbe::Ok(v) => Some(v),
                        SpecsProbe::AuthRequired => {
                            auth_required.push(m.id.clone());
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
                .map(|(m, p)| serde_json::json!({
                    "id": m.id,
                    "address": m.address,
                    "description": m.description,
                    "added_unix_ms": m.added_unix_ms,
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
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }

    // Human-readable table.
    use darkmux_types::style;
    println!("{}", style::header("darkmux fleet status"));
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
        println!("Add a peer: darkmux fleet add <id> --address <tailnet-addr>");
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
                "MACHINE", "ADDRESS", "PROBE", "RAM-FREE", "OS"
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
    for (m, p) in &probes {
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
    Ok(0)
}

/// Outcome of probing a peer's `/machine/specs` (#881). `AuthRequired`
/// (401/403) is distinguished from `Unavailable` (timeout, refused, other
/// non-2xx, bad JSON) so a missing shared fleet token reads as `auth?`, not a
/// silent `specs?`.
enum SpecsProbe {
    Ok(serde_json::Value),
    AuthRequired,
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
        Err(_) => SpecsProbe::Unavailable,
    }
}

/// Format a byte count as a human-friendly "N GB" string for the
/// `fleet status --deep` table. Round to whole GB — the precision the
/// `RAM-FREE` column wants. (#275 PR-B)
fn human_gb(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{:.0} GB", gb.round())
}
