//! `darkmux client` (local prototype): the CLI half of client pairing. The
//! store, the listener and the HTTP contract live in
//! `darkmux_serve::client_pairing`; this module is the operator's side:
//! `serve`, `approve` (the only way a pairing is ever granted), `list`,
//! `revoke`.

use anyhow::Result;
use darkmux_serve::client_pairing::{self as cp, ClientStore, PendingRequest};
use std::io::{BufRead, Write};

use crate::cli::ClientCmd;

pub fn run(sub: ClientCmd) -> Result<i32> {
    match sub {
        ClientCmd::Serve { port } => {
            cp::run(port)?;
            Ok(0)
        }
        ClientCmd::Approve { code, code_arg } => {
            let store = ClientStore::production();
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            approve_interactive(
                &store,
                now(),
                approve_code(code, code_arg).as_deref(),
                &mut input,
                &mut std::io::stdout(),
            )
        }
        ClientCmd::List => {
            let (list, source) = darkmux_types::config_access::clients_profiles_with_source();
            list_to(&ClientStore::production(), now(), &policy_line(list, source), &mut std::io::stdout())
        }
        ClientCmd::Claim { workspace, client } => claim(&workspace, &client),
        ClientCmd::Revoke { name } => {
            if cp::revoke(&ClientStore::production(), &name)? {
                println!("revoked client {name}");
                Ok(0)
            } else {
                eprintln!("no paired client named {name} (see `darkmux client list`)");
                Ok(1)
            }
        }
    }
}

/// `client claim <workspace> <client>`.
fn claim(workspace: &str, client: &str) -> Result<i32> {
    let base = darkmux_types::workdir::worktrees_base_dir();
    match cp::claim(&ClientStore::production(), &base, workspace, client) {
        Ok(_) => {
            println!("workspace {workspace} now belongs to client {client}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("{e:#}");
            Ok(1)
        }
    }
}

/// The code `client approve` was given: `--code <CODE>` or the bare `<CODE>`
/// (clap refuses both at once).
fn approve_code(code: Option<String>, code_arg: Option<String>) -> Option<String> {
    code.or(code_arg)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn age(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    if s < 120 {
        format!("{s}s ago")
    } else if s < 7200 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h ago", s / 3600)
    }
}

fn describe(p: &PendingRequest, now: u64) -> String {
    format!(
        "  device: {}\n  code:   {}\n  age:    {}",
        p.device_name,
        p.code,
        age(now, p.created)
    )
}

/// What every paired app may run here: `clients.profiles`, else any local
/// profile, credited to the tier that set it.
fn policy_line(list: Option<Vec<String>>, source: darkmux_types::config_access::Source) -> String {
    use darkmux_types::config_access::Source;
    let from = match source {
        Source::Env => "DARKMUX_CLIENTS_PROFILES env",
        Source::Config => "clients.profiles in config.json",
        Source::BuiltIn => "clients.profiles unset",
    };
    match list {
        Some(list) if list.is_empty() => format!("apps may run on: nothing ({from} is empty)"),
        Some(list) => format!("apps may run on: {} ({from})", list.join(", ")),
        None => format!("apps may run on: any local profile, one whose work stays on this machine ({from})"),
    }
}

fn ask(input: &mut dyn BufRead, out: &mut dyn Write, prompt: &str) -> Result<String> {
    write!(out, "{prompt}")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().to_ascii_lowercase())
}

/// Offer each live request (or the one showing `code`). `y` approves; with
/// no `--code`, `d` denies and anything else leaves it pending.
pub(crate) fn approve_interactive(
    store: &ClientStore,
    now: u64,
    code: Option<&str>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<i32> {
    match code {
        Some(code) => approve_by_code(store, now, code, input, out),
        None => approve_each_pending(store, now, input, out),
    }
}

/// `--code`: the one request showing it, approved only on an explicit `y`.
fn approve_by_code(
    store: &ClientStore,
    now: u64,
    code: &str,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<i32> {
    let p = match cp::find_by_code(store, code, now) {
        Ok(p) => p,
        Err(e) => {
            writeln!(out, "{e}")?;
            return Ok(1);
        }
    };
    writeln!(out, "pairing request:\n{}", describe(&p, now))?;
    if ask(input, out, "approve? [y/N] ")? == "y" {
        let name = cp::approve(store, &p.request_id, now)?;
        writeln!(
            out,
            "approved as client {name}; the app collects its token on its next poll"
        )?;
        return Ok(0);
    }
    writeln!(out, "not approved")?;
    Ok(1)
}

/// No `--code`: every live request in turn.
fn approve_each_pending(
    store: &ClientStore,
    now: u64,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<i32> {
    let pending = cp::list_pending(store, now)?;
    if pending.is_empty() {
        writeln!(out, "no pending pairing requests")?;
        return Ok(0);
    }
    for p in pending {
        offer(store, now, &p, input, out)?;
    }
    Ok(0)
}

/// One request: `y` approves, `d` denies, anything else leaves it pending.
fn offer(
    store: &ClientStore,
    now: u64,
    p: &PendingRequest,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    writeln!(out, "pairing request:\n{}", describe(p, now))?;
    let reply = ask(input, out, "approve? [y/N, d = deny] ")?;
    let outcome = act_on(store, now, p, &reply)?;
    writeln!(out, "{outcome}")?;
    Ok(())
}

/// Carries out one reply to a request and says what happened.
fn act_on(store: &ClientStore, now: u64, p: &PendingRequest, reply: &str) -> Result<String> {
    Ok(match reply {
        "y" | "yes" => {
            let name = cp::approve(store, &p.request_id, now)?;
            format!("approved as client {name}")
        }
        "d" | "deny" => {
            cp::deny(store, &p.request_id, now)?;
            "denied".to_string()
        }
        _ => "left pending".to_string(),
    })
}

/// One line per paired client, then per approval its app has not collected
/// yet; "no paired clients" when there are neither.
fn client_lines(store: &ClientStore, now: u64) -> Result<Vec<String>> {
    let mut lines: Vec<String> = cp::list_clients(store)?
        .into_iter()
        .map(|c| format!("{}  (paired {})", c.name, age(now, c.created)))
        .collect();
    lines.extend(cp::list_uncollected(store, now)?.into_iter().map(|p| {
        let name = p.client_name.unwrap_or_default();
        format!(
            "{name}  (approved {}; its app has not collected its token yet: `darkmux client revoke {name}` takes it back)",
            age(now, p.created)
        )
    }));
    if lines.is_empty() {
        lines.push("no paired clients".to_string());
    }
    Ok(lines)
}

/// `darkmux doctor`'s row for client workspaces no paired client owns.
pub(crate) fn doctor_checks() -> Vec<crate::doctor::Check> {
    let base = darkmux_types::workdir::worktrees_base_dir();
    let unowned = if base.is_dir() {
        cp::unowned_workspaces(&ClientStore::production(), &base)
    } else {
        Ok(Vec::new())
    };
    vec![unowned_row(unowned)]
}

/// The row for `unowned`: Pass when every client workspace has a paired
/// owner, Warn naming each one that has none and the command that gives it
/// to a client.
fn unowned_row(unowned: Result<Vec<String>>) -> crate::doctor::Check {
    use crate::doctor::{Check, Status};
    let name = "client workspaces".to_string();
    match unowned {
        Ok(list) if list.is_empty() => Check {
            name,
            status: Status::Pass,
            message: "every workspace darkmux made for a paired app belongs to one".to_string(),
            hint: None,
        },
        Ok(list) => Check {
            name,
            status: Status::Warn,
            message: format!(
                "no paired app owns {} (made before workspaces had owners while several apps were \
                 paired, or its app was revoked), so no app can use it",
                list.join(", ")
            ),
            hint: Some("`darkmux client claim <workspace> <client>` gives one to a client (`darkmux client list` names them)".to_string()),
        },
        Err(e) => Check {
            name,
            status: Status::Warn,
            message: format!("could not read the client store: {e:#}"),
            hint: None,
        },
    }
}

/// The command that approves a pairing, as every text names it.
pub(crate) const APPROVE_COMMAND: &str = "darkmux client approve --code <CODE>";

/// `client list`: paired clients, approvals whose app has not collected its
/// token yet (revocable by name like a client), what apps may run here, and
/// how to approve what waits.
fn list_to(store: &ClientStore, now: u64, policy: &str, out: &mut dyn Write) -> Result<i32> {
    let waiting = cp::list_pending(store, now)?.len();
    for line in client_lines(store, now)? {
        writeln!(out, "{line}")?;
    }
    writeln!(out, "{policy}")?;
    if waiting > 0 {
        writeln!(
            out,
            "{waiting} pairing request(s) waiting: {APPROVE_COMMAND}, with the code your app shows"
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store holding one live request, written the way the listener writes it.
    fn store_with(codes: &[&str]) -> (tempfile::TempDir, ClientStore, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::at(dir.path().join("clients.json"));
        let ids: Vec<String> = codes
            .iter()
            .enumerate()
            .map(|(i, _)| format!("r-{i}"))
            .collect();
        store
            .update(|f| {
                for (i, code) in codes.iter().enumerate() {
                    f.pending.push(PendingRequest {
                        request_id: ids[i].clone(),
                        device_name: format!("phone {i}"),
                        code: code.to_string(),
                        pair_secret_hash: "00".repeat(32),
                        created: 100 + i as u64,
                        status: cp::PendingStatus::Pending,
                        client_name: None,
                        client_id: None,
                    });
                }
                Ok(())
            })
            .unwrap();
        (dir, store, ids)
    }

    fn statuses(store: &ClientStore) -> Vec<cp::PendingStatus> {
        store
            .read()
            .unwrap()
            .pending
            .iter()
            .map(|p| p.status)
            .collect()
    }

    #[test]
    fn approve_asks_per_request_y_approves_d_denies_enter_skips() {
        let (_d, store, _) = store_with(&["AAA", "BBB", "CCC"]);
        let mut out = Vec::new();
        let code =
            approve_interactive(&store, 200, None, &mut "y\nd\n\n".as_bytes(), &mut out).unwrap();
        assert_eq!(code, 0);
        use cp::PendingStatus::*;
        assert_eq!(statuses(&store), [Approved, Denied, Pending]);
        let shown = String::from_utf8(out).unwrap();
        assert!(
            shown.contains("phone 0") && shown.contains("AAA") && !shown.contains("profiles"),
            "{shown}"
        );
    }

    #[test]
    fn approve_by_code_needs_confirmation() {
        let (_d, store, _) = store_with(&["AAA", "BBB"]);
        let code = approve_interactive(
            &store,
            200,
            Some("bbb"),
            &mut "n\n".as_bytes(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(
            statuses(&store),
            [cp::PendingStatus::Pending, cp::PendingStatus::Pending]
        );
        let code = approve_interactive(
            &store,
            200,
            Some("bbb"),
            &mut "y\n".as_bytes(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            statuses(&store),
            [cp::PendingStatus::Pending, cp::PendingStatus::Approved]
        );
    }

    /// The approve command darkmux and the app show, `darkmux client approve
    /// --code <CODE>`, parses; so does the bare `darkmux client approve
    /// <CODE>` an earlier text showed. Both name the same code.
    #[test]
    fn the_approve_command_every_text_shows_parses() {
        use clap::Parser;
        for argv in [
            &["darkmux", "client", "approve", "--code", "4F7-K2Q"][..],
            &["darkmux", "client", "approve", "4F7-K2Q"][..],
        ] {
            let cli = crate::cli::Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            let crate::cli::Cmd::Client { sub: ClientCmd::Approve { code, code_arg } } = cli.command else {
                panic!("{argv:?} is not `client approve`");
            };
            assert_eq!(approve_code(code, code_arg).as_deref(), Some("4F7-K2Q"), "{argv:?}");
        }
        assert!(
            crate::cli::Cli::try_parse_from(["darkmux", "client", "approve", "A", "--code", "B"]).is_err(),
            "one code, not two"
        );
        let shown: Vec<&str> = APPROVE_COMMAND.split_whitespace().collect();
        let mut argv = shown.clone();
        *argv.last_mut().unwrap() = "4F7-K2Q";
        assert!(crate::cli::Cli::try_parse_from(argv).is_ok(), "{APPROVE_COMMAND}");
    }

    fn listed(store: &ClientStore, now: u64, policy: &str) -> String {
        let mut out = Vec::new();
        list_to(store, now, policy, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// `client list` shows an approval its app has not collected yet, by
    /// the name `client revoke` takes it back with, and names the approve
    /// command that works for what still waits.
    #[test]
    fn list_shows_uncollected_approvals_and_the_approve_command() {
        let (_d, store, ids) = store_with(&["AAA", "BBB"]);
        let name = cp::approve(&store, &ids[0], 200).unwrap();
        let shown = listed(&store, 200, "apps may run on: x");
        assert!(shown.contains(&format!("{name}  (approved")), "{shown}");
        assert!(shown.contains(&format!("`darkmux client revoke {name}`")), "{shown}");
        assert!(shown.contains("1 pairing request(s) waiting: darkmux client approve --code <CODE>"), "{shown}");
        assert!(cp::revoke(&store, &name).unwrap());
        assert!(!listed(&store, 200, "x").contains(&name), "taken back");
    }

    /// `client claim <workspace> <client>` parses, and doctor's row names each
    /// workspace no paired app owns with the command that gives it to one.
    #[test]
    fn claim_parses_and_doctor_names_unowned_workspaces() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["darkmux", "client", "claim", "hcr", "sam-s-iphone"]).unwrap();
        let crate::cli::Cmd::Client { sub: ClientCmd::Claim { workspace, client } } = cli.command else {
            panic!("not `client claim`");
        };
        assert_eq!((workspace.as_str(), client.as_str()), ("hcr", "sam-s-iphone"));
        let row = unowned_row(Ok(vec!["hcr".into(), "site".into()]));
        assert_eq!(row.status, crate::doctor::Status::Warn);
        assert!(row.message.contains("hcr, site"), "{}", row.message);
        assert!(row.hint.unwrap().contains("darkmux client claim <workspace> <client>"));
        assert_eq!(unowned_row(Ok(Vec::new())).status, crate::doctor::Status::Pass);
    }

    /// The policy line credits `clients.profiles` to the tier that set it:
    /// an env value is never called config's.
    #[test]
    fn the_policy_line_credits_its_real_source() {
        use darkmux_types::config_access::Source;
        let list = Some(vec!["fast".to_string()]);
        assert_eq!(policy_line(list.clone(), Source::Env), "apps may run on: fast (DARKMUX_CLIENTS_PROFILES env)");
        assert_eq!(policy_line(list, Source::Config), "apps may run on: fast (clients.profiles in config.json)");
        assert_eq!(policy_line(Some(vec![]), Source::Env), "apps may run on: nothing (DARKMUX_CLIENTS_PROFILES env is empty)");
        assert!(policy_line(None, Source::BuiltIn).ends_with("(clients.profiles unset)"));
    }
}
