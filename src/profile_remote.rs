//! `darkmux profile list --machine <peer>` / `--remote`: the profiles a peer
//! lets THIS machine dispatch to (#3045 follow-up, 5.0).
//!
//! The source is the peer's machine card, the same card `darkmux machine list`
//! and `GET /fleet/view` gather through the fleet listener
//! (`machine_list::local_fleet_view`); there is no second fetch path. The card
//! carries two things: `profiles` (everything in the peer's registry, with the
//! models each runs) and `accepts` (the peer's allow-list entry for THIS
//! machine). A profile is available here when it is in both. Each is printed as
//! its `<profile>@<peer>` dispatch address, the form `dispatch --profile` takes.
//!
//! A peer whose card could not be read is reported as unreadable with the
//! reason the card fetch gave. It is never counted as "0 profiles": a peer
//! that grants nothing is a different answer from a peer that did not answer.

use anyhow::{bail, Result};
use darkmux_serve::fleet_view::{AcceptsState, CardOutcome, FleetMachine, FleetView};
use darkmux_types::style;
use schemars::JsonSchema;
use serde::Serialize;

use crate::machine_list::{card_unreadable_reason, row_name};

/// Which peers a `profile list` call asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// `--machine <name>`: one roster machine.
    Machine(&'a str),
    /// `--remote`: every roster peer.
    EveryPeer,
}

/// What a target resolves to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The target is this machine: the plain local list answers.
    Local,
    /// One entry per peer asked about.
    Peers(PeerProfileList),
}

/// `profile list --machine <peer>` and `profile list --remote --json`.
#[derive(Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct PeerProfileList {
    pub peers: Vec<PeerProfiles>,
}

/// One peer's answer.
#[derive(Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PeerProfiles {
    /// The card was read and the peer's allow-list says whether this machine
    /// may use its profiles. `profiles` is empty when none is granted.
    Listed { machine: String, profiles: Vec<AvailableProfile> },
    /// The card could not be read, or does not say what this machine may use.
    /// `reason` is the card fetch's own phrase. There is no `profiles` field:
    /// nothing is known, which is not zero.
    Unreadable { machine: String, reason: String },
}

/// One profile this machine may dispatch to on a peer.
#[derive(Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct AvailableProfile {
    pub name: String,
    /// The `--profile` value that dispatches to it: `<profile>@<peer>`.
    pub address: String,
    /// The model ids the profile runs; empty when the peer's card does not
    /// list the profile it grants.
    pub models: Vec<String>,
    pub description: Option<String>,
}

fn peer_entry(row: &FleetMachine, machine: &str) -> PeerProfiles {
    let unreadable = |reason: String| PeerProfiles::Unreadable { machine: machine.to_string(), reason };
    let CardOutcome::Available { card, .. } = &row.card else {
        return unreadable(card_unreadable_reason(&row.card).unwrap_or_default());
    };
    let granted = match &row.accepts {
        AcceptsState::Granted { accepts } => &accepts.profiles,
        AcceptsState::NotListed => return PeerProfiles::Listed { machine: machine.to_string(), profiles: Vec::new() },
        AcceptsState::ThisMachine | AcceptsState::Unknown => {
            return unreadable("its card was read but does not say what this machine may use".to_string())
        }
    };
    let profiles = granted
        .iter()
        .map(|name| {
            let listed = card.profiles.iter().find(|p| &p.name == name);
            AvailableProfile {
                name: name.clone(),
                address: format!("{name}@{machine}"),
                models: listed.map(|p| p.models.iter().map(|m| m.id.clone()).collect()).unwrap_or_default(),
                description: listed.and_then(|p| p.description.clone()),
            }
        })
        .collect();
    PeerProfiles::Listed { machine: machine.to_string(), profiles }
}

/// Resolve `target` against a gathered view. A name no roster machine has is an
/// error naming the machines there are.
pub(crate) fn resolve(view: &FleetView, target: Target<'_>) -> Result<Resolved> {
    let peers = |rows: Vec<&FleetMachine>| {
        Resolved::Peers(PeerProfileList { peers: rows.into_iter().map(|m| peer_entry(m, &row_name(view, m))).collect() })
    };
    match target {
        Target::EveryPeer => Ok(peers(view.machines.iter().filter(|m| !m.is_this_machine).collect())),
        Target::Machine(name) => {
            let found = view.machines.iter().find(|m| row_name(view, m).eq_ignore_ascii_case(name));
            match found {
                Some(m) if m.is_this_machine => Ok(Resolved::Local),
                Some(m) => Ok(peers(vec![m])),
                None => {
                    let known: Vec<String> = view.machines.iter().map(|m| row_name(view, m)).collect();
                    bail!("no machine named `{name}` in the roster (known: {})", known.join(", "))
                }
            }
        }
    }
}

/// Whether every peer answered. A list with an unreadable peer exits nonzero.
pub(crate) fn all_readable(list: &PeerProfileList) -> bool {
    list.peers.iter().all(|p| matches!(p, PeerProfiles::Listed { .. }))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "profile" } else { "profiles" }
}

/// The text view: one block per peer.
pub(crate) fn render_text(list: &PeerProfileList) -> String {
    if list.peers.is_empty() {
        return "no peers in the roster: add one with `darkmux machine add <id> --address <dns-name>`\n".to_string();
    }
    let mut out = String::new();
    for peer in &list.peers {
        match peer {
            PeerProfiles::Listed { machine, profiles } if profiles.is_empty() => {
                out.push_str(&format!("{}: 0 profiles available to this machine\n", style::accent(machine)));
            }
            PeerProfiles::Listed { machine, profiles } => {
                let n = profiles.len();
                out.push_str(&format!("{}: {n} {} available to this machine\n", style::accent(machine), plural(n)));
                for p in profiles {
                    let models = if p.models.is_empty() { "model not listed".to_string() } else { p.models.join(", ") };
                    out.push_str(&format!("  {:<28} {}\n", p.address, style::dim(&models)));
                }
            }
            PeerProfiles::Unreadable { machine, reason } => {
                out.push_str(&format!("{}: {}\n", style::accent(machine), style::warn(&format!("unreadable, {reason}"))));
            }
        }
    }
    out
}

/// Gather the fleet view and print the answer. `None` means the target is this
/// machine and the caller prints the local list.
pub(crate) fn run(target: Target<'_>, json: bool) -> Result<Option<i32>> {
    let view = crate::machine_list::local_fleet_view();
    let Resolved::Peers(list) = resolve(&view, target)? else { return Ok(None) };
    if json {
        crate::cli_json::emit(&list)?;
    } else {
        print!("{}", render_text(&list));
    }
    Ok(Some(i32::from(!all_readable(&list))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine_list::tests::{card_with, granted, machine, own_row, read, unreachable as unreachable_peer, view, with_accepts};
    use darkmux_serve::fleet_view::{Liveness, UnreachableReason};

    fn studio(accepts: AcceptsState) -> FleetMachine {
        let card = card_with(&[], &[("fast".into(), "qwen3-4b"), ("review".into(), "devstral"), ("deep".into(), "qwen3-122b")]);
        with_accepts(machine("studio", Liveness::Live, read(card)), accepts)
    }

    fn only_peer(list: Resolved) -> PeerProfiles {
        let Resolved::Peers(mut l) = list else { panic!("expected peers, got {list:?}") };
        assert_eq!(l.peers.len(), 1);
        l.peers.remove(0)
    }

    #[test]
    fn granted_profiles_list_their_models_and_dispatch_addresses() {
        let v = view(vec![own_row(), studio(granted(&["fast", "review"]))]);
        let got = only_peer(resolve(&v, Target::Machine("studio")).unwrap());
        let PeerProfiles::Listed { machine, profiles } = got else { panic!("{got:?}") };
        assert_eq!(machine, "studio");
        let shown: Vec<(&str, &str, &[String])> =
            profiles.iter().map(|p| (p.name.as_str(), p.address.as_str(), p.models.as_slice())).collect();
        assert_eq!(
            shown,
            vec![
                ("fast", "fast@studio", &["qwen3-4b".to_string()][..]),
                ("review", "review@studio", &["devstral".to_string()][..]),
            ],
            "only the granted two, not the peer's third profile"
        );
    }

    #[test]
    fn none_granted_is_zero_profiles_and_says_so() {
        let v = view(vec![own_row(), studio(granted(&[]))]);
        let got = only_peer(resolve(&v, Target::Machine("studio")).unwrap());
        assert_eq!(got, PeerProfiles::Listed { machine: "studio".into(), profiles: vec![] });
        let not_listed = view(vec![own_row(), studio(AcceptsState::NotListed)]);
        let got = only_peer(resolve(&not_listed, Target::Machine("studio")).unwrap());
        assert_eq!(got, PeerProfiles::Listed { machine: "studio".into(), profiles: vec![] });
        style::set_colorize_override(Some(false));
        let Resolved::Peers(l) = resolve(&v, Target::Machine("studio")).unwrap() else { panic!() };
        assert_eq!(render_text(&l), "studio: 0 profiles available to this machine\n");
    }

    #[test]
    fn an_unreadable_peer_is_not_zero_profiles_and_carries_the_fetch_reason() {
        let down = unreachable_peer("studio", UnreachableReason::ListenerOff, Some("connection refused"));
        let v = view(vec![own_row(), down]);
        let got = only_peer(resolve(&v, Target::Machine("studio")).unwrap());
        let PeerProfiles::Unreadable { reason, .. } = &got else { panic!("an unreadable peer is not a list: {got:?}") };
        assert!(reason.contains("its fleet listener did not answer") && reason.contains("connection refused"), "{reason}");
        let Resolved::Peers(l) = resolve(&v, Target::Machine("studio")).unwrap() else { panic!() };
        assert!(!all_readable(&l));
        style::set_colorize_override(Some(false));
        let text = render_text(&l);
        assert!(text.starts_with("studio: unreadable, unreachable: its fleet listener did not answer"), "{text}");
        assert!(!text.contains("0 profiles"), "{text}");
        let json = serde_json::to_value(&l).unwrap();
        assert!(json["peers"][0].get("profiles").is_none(), "no profiles field for an unreadable peer: {json}");
    }

    #[test]
    fn a_read_card_that_does_not_say_what_this_machine_may_use_is_unreadable() {
        let v = view(vec![own_row(), studio(AcceptsState::Unknown)]);
        let got = only_peer(resolve(&v, Target::Machine("studio")).unwrap());
        assert!(matches!(got, PeerProfiles::Unreadable { .. }), "{got:?}");
    }

    #[test]
    fn this_machines_own_name_resolves_to_the_local_list() {
        let mut own = own_row();
        own.entry = None;
        let v = view(vec![own, studio(granted(&["fast"]))]);
        // The view's `local_machine_id` is "laptop": the row is listed under it.
        assert_eq!(resolve(&v, Target::Machine("laptop")).unwrap(), Resolved::Local);
        assert_eq!(resolve(&v, Target::Machine("LAPTOP")).unwrap(), Resolved::Local, "names match case-insensitively");
    }

    #[test]
    fn an_unknown_machine_name_lists_the_roster() {
        let v = view(vec![own_row(), studio(granted(&["fast"]))]);
        let err = resolve(&v, Target::Machine("nowhere")).unwrap_err().to_string();
        assert!(err.contains("no machine named `nowhere`") && err.contains("studio"), "{err}");
    }

    #[test]
    fn remote_lists_every_peer_and_never_this_machine() {
        let mini = unreachable_peer("mini", UnreachableReason::DnsFailed, None);
        let v = view(vec![own_row(), studio(granted(&["fast"])), mini]);
        let Resolved::Peers(l) = resolve(&v, Target::EveryPeer).unwrap() else { panic!() };
        let names: Vec<&str> = l
            .peers
            .iter()
            .map(|p| match p {
                PeerProfiles::Listed { machine, .. } | PeerProfiles::Unreadable { machine, .. } => machine.as_str(),
            })
            .collect();
        assert_eq!(names, vec!["studio", "mini"]);
        assert!(!all_readable(&l));
    }

    #[test]
    fn a_single_profile_reads_in_the_singular() {
        let v = view(vec![own_row(), studio(granted(&["fast"]))]);
        let Resolved::Peers(l) = resolve(&v, Target::Machine("studio")).unwrap() else { panic!() };
        style::set_colorize_override(Some(false));
        let text = render_text(&l);
        assert!(text.starts_with("studio: 1 profile available to this machine\n  fast@studio"), "{text}");
    }
}
