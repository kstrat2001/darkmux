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
        ClientCmd::Approve { code } => {
            let store = ClientStore::production();
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            approve_interactive(
                &store,
                now(),
                code.as_deref(),
                &mut input,
                &mut std::io::stdout(),
            )
        }
        ClientCmd::List => list(&ClientStore::production(), now()),
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

fn scopes_line(s: &cp::Scopes) -> String {
    let profiles = if s.profiles.is_empty() {
        "(none)".to_string()
    } else {
        s.profiles.join(", ")
    };
    format!(
        "profiles: {profiles}; workspace: {}",
        if s.workspace { "yes" } else { "no" }
    )
}

fn describe(p: &PendingRequest, now: u64) -> String {
    format!(
        "  device: {}\n  code:   {}\n  scopes: {}\n  age:    {}",
        p.device_name,
        p.code,
        scopes_line(&p.scopes),
        age(now, p.created)
    )
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
    if let Some(code) = code {
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
        return Ok(1);
    }
    let pending = cp::list_pending(store, now)?;
    if pending.is_empty() {
        writeln!(out, "no pending pairing requests")?;
        return Ok(0);
    }
    for p in pending {
        writeln!(out, "pairing request:\n{}", describe(&p, now))?;
        match ask(input, out, "approve? [y/N, d = deny] ")?.as_str() {
            "y" | "yes" => {
                let name = cp::approve(store, &p.request_id, now)?;
                writeln!(out, "approved as client {name}")?;
            }
            "d" | "deny" => {
                cp::deny(store, &p.request_id, now)?;
                writeln!(out, "denied")?;
            }
            _ => writeln!(out, "left pending")?,
        }
    }
    Ok(0)
}

fn list(store: &ClientStore, now: u64) -> Result<i32> {
    let clients = cp::list_clients(store)?;
    let waiting = cp::list_pending(store, now)?.len();
    if clients.is_empty() {
        println!("no paired clients");
    }
    for c in clients {
        println!(
            "{}  (paired {})  {}",
            c.name,
            age(now, c.created),
            scopes_line(&c.scopes)
        );
    }
    if waiting > 0 {
        println!("{waiting} pairing request(s) waiting: darkmux client approve");
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
                        scopes: cp::Scopes {
                            profiles: vec!["fast".into()],
                            workspace: false,
                        },
                        created: 100 + i as u64,
                        status: cp::PendingStatus::Pending,
                        client_name: None,
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
            shown.contains("phone 0") && shown.contains("AAA") && shown.contains("profiles: fast"),
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
}
