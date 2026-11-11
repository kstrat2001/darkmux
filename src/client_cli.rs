//! `darkmux client` (local prototype): the CLI half of client pairing. The
//! store, the listener and the HTTP contract live in
//! `darkmux_serve::client_pairing`; this module is the operator's side:
//! `serve`, `approve` (the operator's way to grant a pairing; `client serve`
//! also grants one to a verified app, or on the dialog's Allow, #3544),
//! `list`, `revoke`, `forget`.

use anyhow::Result;
use darkmux_serve::client_pairing::{self as cp, ClientStore, PendingRequest};
use std::io::{BufRead, Write};

use crate::cli::ClientCmd;

pub fn run(sub: ClientCmd) -> Result<i32> {
    if let ClientCmd::Serve { port } = sub {
        return cp::run(port).map(|()| 0);
    }
    let (list, source) = darkmux_types::config_access::clients_profiles_with_source();
    let here = Here {
        store: ClientStore::production(),
        base: darkmux_types::workdir::worktrees_base_dir(),
        now: now(),
        policy: policy_line(list, source),
    };
    let stdin = std::io::stdin();
    run_verb(sub, &here, &mut stdin.lock(), &mut std::io::stdout(), &mut std::io::stderr())
}

/// What the operator's verbs act on: the client store, the worktrees base,
/// the time, and what apps may run here (`client list`'s policy line).
struct Here {
    store: ClientStore,
    base: std::path::PathBuf,
    now: u64,
    policy: String,
}

/// Every verb but `serve`, against `here`, reading answers from `input`
/// and writing to `out` (and refusals to `err`).
fn run_verb(sub: ClientCmd, here: &Here, input: &mut dyn BufRead, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    match sub {
        ClientCmd::Approve { code, code_arg } => {
            approve_interactive(&here.store, here.now, approve_code(code, code_arg).as_deref(), input, out)
        }
        ClientCmd::List => list_to(&here.store, here.now, &here.policy, out),
        ClientCmd::Claim { workspace, client } => claim(here, &workspace, &client, out, err),
        ClientCmd::Revoke { name } => revoke(here, &name, out, err),
        ClientCmd::Forget { app } => forget(here, &app, out, err),
        ClientCmd::Workspace { sub: crate::cli::ClientWorkspaceCmd::Rm { name, force } } => {
            remove_workspace(here, &name, force, out, err)
        }
        ClientCmd::Serve { .. } => anyhow::bail!("`client serve` listens; it is not run as a verb here"),
    }
}

/// `client revoke <name>`.
fn revoke(here: &Here, name: &str, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    if cp::revoke(&here.store, name)? {
        writeln!(out, "revoked client {name}")?;
        return Ok(0);
    }
    writeln!(err, "no paired client named {name} (see `darkmux client list`)")?;
    Ok(1)
}

/// `client forget <team>/<bundle>`.
fn forget(here: &Here, app: &str, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let Some(identity) = cp::SigningIdentity::parse(app) else {
        writeln!(err, "`{app}` is not <team>/<bundle> (see `darkmux client list`)")?;
        return Ok(1);
    };
    if cp::forget(&here.store, &identity)? {
        writeln!(out, "forgot {identity}: its next pairing asks again (its paired clients stay; `darkmux client revoke <name>` removes one)")?;
        return Ok(0);
    }
    writeln!(err, "no remembered app {identity} (see `darkmux client list`)")?;
    Ok(1)
}

/// `client workspace rm <name> [--force]`: a workspace darkmux made for a
/// paired app, whoever owns it (or no one), removed with what darkmux made
/// for it. A run, an Undo or a live preview in it (in `client serve`, which
/// holds its name while it uses it) refuses it.
fn remove_workspace(here: &Here, name: &str, force: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    use darkmux_serve::client_workspace as ws;
    let base = match here.base.canonicalize() {
        Ok(b) => b,
        Err(e) => return writeln!(err, "no workspaces under {}: {e}", here.base.display()).map(|()| 1).map_err(Into::into),
    };
    match ws::remove_workspace_within(&base, name, force, std::time::Duration::ZERO) {
        Ok(r) => {
            writeln!(out, "removed {} workspace {name}{}", r.kind.word(), branch_said(&r))?;
            Ok(0)
        }
        Err(e) => writeln!(err, "{e}").map(|()| 1).map_err(Into::into),
    }
}

/// What `client workspace rm` says about the removed workspace's branch.
fn branch_said(r: &darkmux_serve::client_workspace::Removal) -> String {
    match (&r.branch, r.branch_removed) {
        (Some(b), true) => format!("; branch {b} removed"),
        (Some(b), false) if b.starts_with("mux/") => {
            format!("; branch {b} kept (no other branch holds its commits: `--force` removes it)")
        }
        (Some(b), false) => format!("; branch {b} left as it is"),
        (None, _) => String::new(),
    }
}

/// `client claim <workspace> <client>`.
fn claim(here: &Here, workspace: &str, client: &str, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    match cp::claim(&here.store, &here.base, workspace, client) {
        Ok(_) => writeln!(out, "workspace {workspace} now belongs to client {client}").map(|()| 0),
        Err(e) => writeln!(err, "{e:#}").map(|()| 1),
    }
    .map_err(Into::into)
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
        darkmux_platform::display_safe(&p.device_name),
        darkmux_platform::display_safe(&p.code),
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
        Err(e) => return writeln!(out, "{e}").map(|()| 1).map_err(Into::into),
    };
    writeln!(out, "pairing request:\n{}", describe(&p, now))?;
    let reply = ask(input, out, "approve? [y/N] ")?;
    let (said, code) = approve_on_yes(store, now, &p, &reply)?;
    writeln!(out, "{said}")?;
    Ok(code)
}

/// `--code`'s reply carried out: only `y` approves. What to say, and the
/// exit code.
fn approve_on_yes(store: &ClientStore, now: u64, p: &PendingRequest, reply: &str) -> Result<(String, i32)> {
    if reply != "y" {
        return Ok(("not approved".to_string(), 1));
    }
    let name = cp::approve(store, &p.request_id, now)?;
    Ok((format!("approved as client {name}; the app collects its token on its next poll"), 0))
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
        .map(|c| format!("{}  (paired {}{})", c.name, age(now, c.created), on_its_own(c.auto_approved.as_ref())))
        .collect();
    lines.extend(cp::list_uncollected(store, now)?.into_iter().map(|p| {
        let name = p.client_name.unwrap_or_default();
        format!(
            "{name}  (approved {}{}; its app has not collected its token yet: `darkmux client revoke {name}` takes it back)",
            age(now, p.created),
            on_its_own(p.auto_approved.as_ref())
        )
    }));
    if lines.is_empty() {
        lines.push("no paired clients".to_string());
    }
    lines.extend(cp::remembered_apps(store)?.into_iter().map(|r| {
        let app = r.identity();
        format!("remembered app {app}  (allowed {}; pairs with no dialog: `darkmux client forget {app}` forgets it)", age(now, r.allowed))
    }));
    Ok(lines)
}

/// What `client list` adds for a pairing approved with no operator step.
fn on_its_own(auto: Option<&cp::SigningIdentity>) -> String {
    auto.map(|app| format!(", auto-approved: signed by {app}")).unwrap_or_default()
}

/// `darkmux doctor`'s row for client workspaces no paired client owns.
pub(crate) fn doctor_checks() -> Vec<crate::doctor::Check> {
    let base = darkmux_types::workdir::worktrees_base_dir();
    let unowned = if base.is_dir() {
        cp::unowned_workspaces(&ClientStore::production(), &base)
    } else {
        Ok(Vec::new())
    };
    let store = ClientStore::production().peek();
    let remembered = store.as_ref().map(|f| f.remembered_apps.clone()).map_err(|e| anyhow::anyhow!("{e:#}"));
    let auto = store.map(|f| f.clients);
    vec![unowned_row(unowned), remembered_row(remembered), auto_approved_row(auto)]
}

/// (#3544) The row naming each paired client that paired with no operator
/// step, by the signature that verified it.
fn auto_approved_row(clients: Result<Vec<cp::ApprovedClient>>) -> crate::doctor::Check {
    use crate::doctor::{Check, Status};
    let name = "client auto-approvals".to_string();
    let (status, message) = match clients {
        Ok(list) => {
            let auto: Vec<String> = list
                .iter()
                .filter_map(|c| c.auto_approved.as_ref().map(|app| format!("{} (signed by {app})", c.name)))
                .collect();
            match auto.is_empty() {
                true => (Status::Pass, "none: every paired client was approved by you".to_string()),
                false => (Status::Pass, format!("paired with no dialog, by a verified signature: {}", auto.join(", "))),
            }
        }
        Err(e) => (Status::Warn, format!("could not read the client store: {e:#}")),
    };
    Check { name, status, message, hint: Some("`darkmux client revoke <name>` removes a client".to_string()) }
}

/// (#3544) The row naming the apps the operator allowed in the pairing
/// dialog, each of which pairs again with no dialog.
fn remembered_row(remembered: Result<Vec<cp::RememberedApp>>) -> crate::doctor::Check {
    use crate::doctor::{Check, Status};
    let name = "client remembered apps".to_string();
    let (status, message, hint) = match remembered {
        Ok(list) if list.is_empty() => (Status::Pass, "none: no app has been allowed in the pairing dialog".to_string(), None),
        Ok(list) => (
            Status::Pass,
            format!(
                "{} pair with no dialog (allowed in the pairing dialog)",
                list.iter().map(|r| r.identity().to_string()).collect::<Vec<_>>().join(", ")
            ),
            Some("`darkmux client forget <team>/<bundle>` forgets one".to_string()),
        ),
        Err(e) => (Status::Warn, format!("could not read the client store: {e:#}"), None),
    };
    Check { name, status, message, hint }
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
    let mut lines = client_lines(store, now)?;
    lines.push(policy.to_string());
    if waiting > 0 {
        lines.push(format!(
            "{waiting} pairing request(s) waiting: {APPROVE_COMMAND}, with the code your app shows"
        ));
    }
    writeln!(out, "{}", lines.join("\n"))?;
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
                        auto_approved: None,
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

    /// The verbs `run` hands on, against a store and worktrees base of the
    /// test's own: `Here` with `paired` clients and `ws` workspaces darkmux
    /// made for an app.
    fn here(paired: &[&str], ws: &[&str]) -> (tempfile::TempDir, Here) {
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::at(dir.path().join("clients.json"));
        store
            .update(|f| {
                for (i, name) in paired.iter().enumerate() {
                    f.clients.push(cp::ApprovedClient {
                        name: name.to_string(),
                        id: format!("c-{i:016}"),
                        created: 1,
                        token_hash: "00".repeat(32),
                        auto_approved: None,
                    });
                }
                Ok(())
            })
            .unwrap();
        let base = dir.path().join("worktrees");
        for name in ws {
            std::fs::create_dir_all(base.join(name)).unwrap();
            let held = base.join(format!(".{name}.git"));
            std::fs::create_dir_all(&held).unwrap();
            darkmux_serve::client_git::record_git_dir(&base, name, &held).unwrap();
        }
        let here = Here { store, base, now: 200, policy: "apps may run on: x".into() };
        (dir, here)
    }

    /// Runs `sub` with `answers` as the operator's replies: (exit code, out, err).
    fn verb(here: &Here, sub: ClientCmd, answers: &str) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_verb(sub, here, &mut answers.as_bytes(), &mut out, &mut err).unwrap();
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    /// (#3544) `client list` names a client that paired on its own by the
    /// signature that verified it, and each remembered app with how to
    /// forget it; `client forget` forgets it, and refuses one not remembered
    /// or not written `<team>/<bundle>`.
    #[test]
    fn list_shows_auto_approvals_and_remembered_apps_and_forget_forgets() {
        let (_d, here) = here(&["mux-bot"], &[]);
        here.store
            .update(|f| {
                f.clients[0].auto_approved = Some(cp::SigningIdentity { team: "5LDXYW8B6T".into(), bundle: "com.darklyenergized.darkmuxapp".into() });
                f.remembered_apps.push(cp::RememberedApp { team: "AAAAAAAAAA".into(), bundle: "com.example.muxfork".into(), allowed: 100 });
                Ok(())
            })
            .unwrap();
        let (_, out, _) = verb(&here, ClientCmd::List, "");
        assert!(out.contains("mux-bot  (paired 3m ago, auto-approved: signed by 5LDXYW8B6T/com.darklyenergized.darkmuxapp)"), "{out}");
        assert!(out.contains("remembered app AAAAAAAAAA/com.example.muxfork") && out.contains("`darkmux client forget AAAAAAAAAA/com.example.muxfork`"), "{out}");
        let (code, out, _) = verb(&here, ClientCmd::Forget { app: "AAAAAAAAAA/com.example.muxfork".into() }, "");
        assert_eq!(code, 0, "{out}");
        assert!(here.store.read().unwrap().remembered_apps.is_empty());
        let (code, _, err) = verb(&here, ClientCmd::Forget { app: "AAAAAAAAAA/com.example.muxfork".into() }, "");
        assert_eq!(code, 1);
        assert!(err.contains("no remembered app"), "{err}");
        let (code, _, err) = verb(&here, ClientCmd::Forget { app: "nonsense".into() }, "");
        assert_eq!(code, 1);
        assert!(err.contains("<team>/<bundle>"), "{err}");
    }

    #[test]
    fn doctor_names_each_auto_approved_client() {
        let client = |name: &str, auto: bool| cp::ApprovedClient {
            name: name.into(),
            id: "c-0".into(),
            created: 1,
            token_hash: "00".into(),
            auto_approved: auto.then(|| cp::SigningIdentity { team: "5LDXYW8B6T".into(), bundle: "com.darklyenergized.darkmuxapp".into() }),
        };
        let row = auto_approved_row(Ok(vec![client("mux-bot", true), client("phone", false)]));
        assert!(row.message.contains("mux-bot (signed by 5LDXYW8B6T/com.darklyenergized.darkmuxapp)"), "{}", row.message);
        assert!(!row.message.contains("phone"), "{}", row.message);
        assert!(auto_approved_row(Ok(vec![client("phone", false)])).message.starts_with("none"));
    }

    #[test]
    fn doctor_names_each_remembered_app() {
        let row = remembered_row(Ok(vec![cp::RememberedApp { team: "AAAAAAAAAA".into(), bundle: "com.example.muxfork".into(), allowed: 1 }]));
        assert!(row.message.contains("AAAAAAAAAA/com.example.muxfork"), "{}", row.message);
        assert!(row.hint.unwrap().contains("darkmux client forget"));
        let none = remembered_row(Ok(Vec::new())).message;
        assert_eq!(none, "none: no app has been allowed in the pairing dialog", "true on every platform (#3547 review)");
    }

    /// `client revoke` says what it revoked, or refuses a name no client has.
    #[test]
    fn revoke_names_what_it_took_back_or_refuses() {
        let (_d, here) = here(&["phone"], &[]);
        let (code, out, _) = verb(&here, ClientCmd::Revoke { name: "phone".into() }, "");
        assert_eq!((code, out.as_str()), (0, "revoked client phone\n"));
        let (code, _, err) = verb(&here, ClientCmd::Revoke { name: "phone".into() }, "");
        assert_eq!(code, 1);
        assert!(err.contains("no paired client named phone"), "{err}");
    }

    /// `client claim` gives a workspace darkmux made to a paired client, and
    /// refuses an unknown client or a folder darkmux did not make.
    #[test]
    fn claim_gives_a_workspace_or_says_why_not() {
        let (_d, here) = here(&["phone"], &["hcr"]);
        let claim = |w: &str, c: &str| ClientCmd::Claim { workspace: w.into(), client: c.into() };
        let (code, out, _) = verb(&here, claim("hcr", "phone"), "");
        assert_eq!((code, out.as_str()), (0, "workspace hcr now belongs to client phone\n"));
        assert_eq!(darkmux_serve::client_workspace::owner_of(&here.base, "hcr").as_deref(), Some("c-0000000000000000"));
        let (code, _, err) = verb(&here, claim("hcr", "nobody"), "");
        assert_eq!(code, 1);
        assert!(err.contains("no paired client named nobody"), "{err}");
        let (code, _, err) = verb(&here, claim("missions", "phone"), "");
        assert_eq!(code, 1);
        assert!(err.contains("missions is not a workspace darkmux made"), "{err}");
    }

    /// A real clone under `here`'s base, made the way darkmux makes one
    /// (its repository beside it, recorded), on a fresh `mux/` branch.
    fn real_clone(here: &Here, name: &str) {
        let git = |dir: &std::path::Path, args: &[&str]| {
            let ok = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        let origin = here.base.parent().unwrap().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--quiet", "-b", "main"]);
        git(&origin, &["-c", "user.name=o", "-c", "user.email=o@x", "commit", "--quiet", "--allow-empty", "-m", "one"]);
        std::fs::create_dir_all(&here.base).unwrap();
        let (held, dir) = (here.base.join(format!(".{name}.git")), here.base.join(name));
        git(&here.base, &["clone", "--quiet", "--separate-git-dir", held.to_str().unwrap(), origin.to_str().unwrap(), dir.to_str().unwrap()]);
        git(&dir, &["checkout", "--quiet", "-b", "mux/app-abc123"]);
        darkmux_serve::client_git::record_git_dir(&here.base, name, &held).unwrap();
    }

    /// `client workspace rm` removes a workspace darkmux made, whoever owns
    /// it; refuses one in use (`client serve` holds its name) and a folder
    /// darkmux did not make; and parses with `--force`.
    #[test]
    fn workspace_rm_removes_a_workspace_or_says_why_not() {
        use clap::Parser;
        use darkmux_serve::client_workspace::{try_hold, Hold};
        let (_d, here) = here(&["phone"], &["handmade"]);
        real_clone(&here, "hcr");
        let rm = |n: &str| ClientCmd::Workspace { sub: crate::cli::ClientWorkspaceCmd::Rm { name: n.into(), force: false } };
        let in_use = try_hold(&here.base, "hcr", Hold::Shared).unwrap().unwrap();
        let (code, _, err) = verb(&here, rm("hcr"), "");
        assert_eq!(code, 1);
        assert!(err.contains("workspace hcr is in use"), "{err}");
        drop(in_use);
        let (code, out, err) = verb(&here, rm("hcr"), "");
        assert_eq!((code, out.as_str()), (0, "removed clone workspace hcr; branch mux/app-abc123 removed\n"), "{err}");
        assert!(!here.base.join("hcr").exists() && !here.base.join(".hcr.git").exists());
        let (code, _, err) = verb(&here, rm("handmade"), "");
        assert_eq!(code, 1);
        assert!(err.contains("refusing to remove it"), "a recorded repository that is no git repository: {err}");
        assert!(here.base.join("handmade").is_dir(), "left as it was");
        std::fs::create_dir(here.base.join("loose")).unwrap();
        let (code, _, err) = verb(&here, rm("loose"), "");
        assert_eq!(code, 1);
        assert!(err.contains("not a workspace darkmux made"), "{err}");
        assert!(here.base.join("loose").is_dir(), "left as it was");
        let (code, _, err) = verb(&here, rm("nope"), "");
        assert!(code == 1 && err.contains("no workspace named nope"), "{err}");
        let cli = crate::cli::Cli::try_parse_from(["darkmux", "client", "workspace", "rm", "hcr", "--force"]).unwrap();
        let crate::cli::Cmd::Client { sub: ClientCmd::Workspace { sub: crate::cli::ClientWorkspaceCmd::Rm { name, force } } } = cli.command else {
            panic!("not `client workspace rm`");
        };
        assert_eq!((name.as_str(), force), ("hcr", true));
    }

    /// What `client workspace rm` says about the branch: removed, a `mux/`
    /// branch kept (and how to remove it), a branch of the user's left as
    /// it is, or nothing when detached.
    #[test]
    fn workspace_rm_says_what_became_of_the_branch() {
        use darkmux_serve::client_workspace::{Removal, WorkspaceKind};
        let said = |branch: Option<&str>, branch_removed: bool| {
            branch_said(&Removal { kind: WorkspaceKind::Worktree, branch: branch.map(str::to_string), branch_removed })
        };
        assert_eq!(said(Some("mux/app-abc123"), true), "; branch mux/app-abc123 removed");
        assert_eq!(
            said(Some("mux/app-abc123"), false),
            "; branch mux/app-abc123 kept (no other branch holds its commits: `--force` removes it)"
        );
        assert_eq!(said(Some("feature"), false), "; branch feature left as it is");
        assert_eq!(said(None, false), "");
    }

    /// `client list` with no clients says so and how to approve what waits;
    /// with nothing waiting it names no approve command; an unreadable store
    /// refuses.
    #[test]
    fn list_says_what_is_paired_and_what_waits() {
        let (_d, here) = here(&[], &[]);
        let (code, out, _) = verb(&here, ClientCmd::List, "");
        assert_eq!((code, out.as_str()), (0, "no paired clients\napps may run on: x\n"));
        let (_d2, store, _) = store_with(&["AAA"]);
        let here = Here { store, ..here };
        let (_, out, _) = verb(&here, ClientCmd::List, "");
        assert!(out.ends_with("1 pairing request(s) waiting: darkmux client approve --code <CODE>, with the code your app shows\n"), "{out}");
        std::fs::write(here.store.path(), "not json").unwrap();
        assert!(run_verb(ClientCmd::List, &here, &mut "".as_bytes(), &mut Vec::new(), &mut Vec::new()).is_err());
    }

    /// `client approve --code` names a code no request shows, approves on
    /// `y` and leaves it pending otherwise; `serve` is not a verb here.
    #[test]
    fn approve_by_code_through_the_verb() {
        let (_d, store, _) = store_with(&["AAA"]);
        let (_d2, here) = here(&[], &[]);
        let here = Here { store, ..here };
        let approve = |c: &str| ClientCmd::Approve { code: Some(c.into()), code_arg: None };
        let (code, out, _) = verb(&here, approve("ZZZ"), "");
        assert_eq!((code, out.as_str()), (1, "no live pairing request shows code ZZZ\n"));
        let (code, out, _) = verb(&here, approve("AAA"), "n\n");
        assert!(code == 1 && out.ends_with("not approved\n"), "{out}");
        let (code, out, _) = verb(&here, ClientCmd::Approve { code: None, code_arg: Some("aaa".into()) }, "y\n");
        assert!(code == 0 && out.contains("approved as client phone-0"), "{out}");
        assert!(run_verb(ClientCmd::Serve { port: 1 }, &here, &mut "".as_bytes(), &mut Vec::new(), &mut Vec::new()).is_err());
    }

}
