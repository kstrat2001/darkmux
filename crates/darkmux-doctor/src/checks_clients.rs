//! Paired client apps' settings (`clients.*`, read by `darkmux client
//! serve` on every request): each resolved, with the tier that set it.
//! Informational, so always `Pass`.

use crate::{Check, Status};
use darkmux_types::config_access::{self as ca, Source};

/// Where a value came from, as a doctor row says it.
fn tier(source: Source, env: &str) -> String {
    match source {
        Source::Env => format!("from {env} env"),
        Source::Config => "from config.json".to_string(),
        Source::BuiltIn => "default".to_string(),
    }
}

fn row(name: &str, message: String) -> Check {
    Check { name: name.into(), status: Status::Pass, message, hint: None }
}

/// The `clients.*` rows: `clients.profiles`, `clients.preview.enabled`,
/// `clients.preview.idle_minutes`, `clients.pairing.verified_teams`,
/// `clients.pairing.dialog`, against this process's config tier.
pub(crate) fn check_clients_settings() -> Vec<Check> {
    let (profiles, ps) = ca::clients_profiles_with_source();
    let (enabled, es) = ca::clients_preview_enabled_with_source();
    let (minutes, ms) = ca::clients_preview_idle_minutes_with_source();
    let (teams, ts) = ca::clients_pairing_verified_teams_with_source();
    let (dialog, ds) = ca::clients_pairing_dialog_with_source();
    vec![
        row("clients.profiles", profiles_message(profiles, ps)),
        row("clients.preview.enabled", preview_message(enabled, es)),
        row("clients.preview.idle_minutes", idle_message(minutes, ms)),
        row("clients.pairing.verified_teams", teams_message(&teams, ts)),
        row("clients.pairing.dialog", dialog_message(dialog, ds)),
    ]
}

/// (#3560) The name of the row that says what `client serve` does on the
/// tailnet (`clients.remote`'s own enum row says the value).
pub(crate) const REMOTE_CHECK_NAME: &str = "client serve on the tailnet";

/// What the remote row looks at when `clients.remote` is `tailscale`: this
/// machine on the network, and each entry darkmux's records say it
/// published, with what the network shows for it.
pub(crate) struct RemoteLook {
    pub net: darkmux_fleet::overlay::LocalNetwork,
    pub published: Vec<(darkmux_fleet::overlay::ServeEntry, Shown)>,
}

/// What the network shows for a recorded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shown {
    /// Exactly as darkmux published it.
    Exactly,
    /// Not so: gone, or changed.
    Not,
    /// `serve status` could not be read: why.
    Unreadable(String),
}

/// (#3560) The remote row against this process's config tier. Off reads
/// darkmux's own records only (a file read); on, read-only `tailscale`
/// calls.
pub(crate) fn check_clients_remote() -> Check {
    use darkmux_fleet::overlay;
    let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).root;
    remote_check(ca::clients_remote_with_source(), overlay::records(&root.join(overlay::PUBLISHED_DIR)), look_here)
}

/// The tailnet as it is now: `version` and `status`, then, when there are
/// records, `serve status` (read-only, never a change).
fn look_here(recs: &[darkmux_fleet::overlay::PublishedRecord]) -> RemoteLook {
    use darkmux_fleet::overlay;
    let bin = overlay::tool_path();
    let net = overlay::local_network(&bin, overlay::CALL_TIMEOUT);
    let status = if recs.is_empty() { Ok(Vec::new()) } else { overlay::serve_status(&bin, overlay::CALL_TIMEOUT) };
    let status = status.map_err(|e| format!("{e:#}"));
    let published = recs.iter().map(|r| (r.entry.clone(), shown_in(&status, &r.entry))).collect();
    RemoteLook { net, published }
}

/// What `status` (a `serve status` read, or why it failed) shows for `e`.
pub(crate) fn shown_in(status: &Result<Vec<u8>, String>, e: &darkmux_fleet::overlay::ServeEntry) -> Shown {
    use darkmux_fleet::overlay;
    match status.as_deref().map(|st| overlay::at_port(st, e)) {
        Ok(Ok(overlay::AtPort::Exactly)) => Shown::Exactly,
        Ok(Ok(_)) => Shown::Not,
        Ok(Err(err)) => Shown::Unreadable(format!("{err:#}")),
        Err(why) => Shown::Unreadable(why.clone()),
    }
}

/// The remote row from the resolved setting, darkmux's records (`recs`),
/// and, only when the setting is on, what `look` sees.
pub(crate) fn remote_check(
    setting: Result<(darkmux_types::config::ClientsRemote, Source), darkmux_types::config_enum::BadEnumValue>,
    recs: Vec<darkmux_fleet::overlay::PublishedRecord>,
    look: impl FnOnce(&[darkmux_fleet::overlay::PublishedRecord]) -> RemoteLook,
) -> Check {
    use darkmux_types::config::ClientsRemote;
    let (value, source) = match setting {
        Ok(v) => v,
        Err(bad) => return remote_row(Status::Warn, format!("not checked: {}", bad.summary()), Some(bad.fix())),
    };
    let from = tier(source, "DARKMUX_CLIENTS_REMOTE");
    if value == ClientsRemote::Off {
        return off_check(&from, &recs);
    }
    let seen = look(&recs);
    if let Some(u) = seen.net.unusable() {
        let message = format!("tailscale ({from}), but {}: `client serve` serves this machine only", u.reason);
        return remote_row(Status::Warn, message, Some(format!("{}, or `darkmux config set clients.remote off`", u.fix)));
    }
    tailnet_ready_check(&from, &seen)
}

fn remote_row(status: Status, message: String, hint: Option<String>) -> Check {
    Check { name: REMOTE_CHECK_NAME.into(), status, message, hint }
}

/// The row with `clients.remote` off: Pass, or a Warn for each entry a
/// record says an earlier `client serve` left (read from the file only).
fn off_check(from: &str, recs: &[darkmux_fleet::overlay::PublishedRecord]) -> Check {
    let Some(rec) = recs.first() else {
        return remote_row(
            Status::Pass,
            format!("off ({from}): `client serve` is reached from this machine only, and never publishes on the tailnet"),
            None,
        );
    };
    let message = format!(
        "off ({from}): a record says an earlier `client serve` published {} on the tailnet; the next `client serve` \
         removes it if the tailnet still shows it exactly so (off never publishes, and runs `tailscale` only to remove \
         an entry its own record proves it left)",
        recs.iter().map(|r| r.entry.url()).collect::<Vec<_>>().join(", ")
    );
    remote_row(Status::Warn, message, Some(format!("`{}` removes it now", rec.entry.remove_command())))
}

/// The remote row once the network can carry the entry.
fn tailnet_ready_check(from: &str, seen: &RemoteLook) -> Check {
    const NOT_ACCESS: &str = "reach, not access: a request through it is refused until remote pairing ships";
    let dns = seen.net.dns_name.as_deref().unwrap_or_default();
    if let Some(warn) = seen.published.iter().find_map(|(e, shown)| record_problem(from, dns, e, shown)) {
        return warn;
    }
    let message = match seen.published.as_slice() {
        [] => format!(
            "tailscale ({from}): Tailscale is ready on {dns}; `client serve` publishes https://{dns}:<port>/ while it runs \
             and removes it on shutdown ({NOT_ACCESS})"
        ),
        live => format!(
            "tailscale ({from}): `client serve` is on the tailnet at {} and removes it on shutdown ({NOT_ACCESS})",
            live.iter().map(|(e, _)| e.url()).collect::<Vec<_>>().join(", ")
        ),
    };
    remote_row(Status::Pass, message, None)
}

/// The Warn a recorded entry calls for, when it is not simply live under
/// this machine's current name `dns`.
fn record_problem(from: &str, dns: &str, e: &darkmux_fleet::overlay::ServeEntry, shown: &Shown) -> Option<Check> {
    use darkmux_fleet::overlay;
    let status_hint = Some(format!("`{} serve status` shows what is there", overlay::TOOL));
    match shown {
        Shown::Exactly if e.host != dns => {
            Some(remote_row(Status::Warn, format!("tailscale ({from}): {}", overlay::renamed_reason(e, dns)), None))
        }
        Shown::Exactly => None,
        Shown::Not => Some(remote_row(
            Status::Warn,
            format!(
                "tailscale ({from}): darkmux's record says it published {}, but the tailnet no longer shows that entry as \
                 darkmux published it; the next `client serve` clears the record and leaves the entry alone",
                e.url()
            ),
            status_hint,
        )),
        Shown::Unreadable(why) => Some(remote_row(
            Status::Warn,
            format!(
                "tailscale ({from}): darkmux's record says it published {}, and darkmux could not read the tailnet's serve \
                 entries to check it ({why})",
                e.url()
            ),
            status_hint,
        )),
    }
}

fn teams_message(teams: &[String], source: Source) -> String {
    let from = tier(source, "DARKMUX_CLIENTS_PAIRING_VERIFIED_TEAMS");
    match teams {
        [] => format!("empty ({from}): no signed Mux Bot pairs on its own; every pairing asks"),
        _ => format!("{} ({from}): a Mux Bot signed by one of these teams pairs on this machine with no dialog", teams.join(", ")),
    }
}

fn dialog_message(on: bool, source: Source) -> String {
    let from = tier(source, "DARKMUX_CLIENTS_PAIRING_DIALOG");
    match on {
        true => format!("on ({from}): an app that does not verify gets a native Allow dialog"),
        false => format!("off ({from}): no dialog; `darkmux client approve --code <CODE>` approves a pairing"),
    }
}

fn profiles_message(profiles: Option<Vec<String>>, source: Source) -> String {
    let from = tier(source, "DARKMUX_CLIENTS_PROFILES");
    match profiles {
        None => format!(
            "unset ({from}): a paired app may run on any local profile, one whose work stays on this machine"
        ),
        Some(list) if list.is_empty() => format!("empty ({from}): a paired app may run on no profile"),
        Some(list) => format!("{} ({from}): a paired app may run only on these", list.join(", ")),
    }
}

fn preview_message(enabled: bool, source: Source) -> String {
    let from = tier(source, "DARKMUX_CLIENTS_PREVIEW_ENABLED");
    if enabled {
        format!("on ({from}): a paired app may start a preview of a workspace's own code, in a container")
    } else {
        format!("off ({from}): previews are refused; `darkmux config set clients.preview.enabled true` allows them")
    }
}

fn idle_message(minutes: u64, source: Source) -> String {
    let from = tier(source, "DARKMUX_CLIENTS_PREVIEW_IDLE_MINUTES");
    if minutes == 0 {
        format!("0 ({from}): a preview is never stopped for going unasked-about")
    } else {
        format!("{minutes} ({from}): a preview no app asks about for this many minutes is stopped")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 5] = [
        "DARKMUX_CLIENTS_PROFILES",
        "DARKMUX_CLIENTS_PREVIEW_ENABLED",
        "DARKMUX_CLIENTS_PREVIEW_IDLE_MINUTES",
        "DARKMUX_CLIENTS_PAIRING_VERIFIED_TEAMS",
        "DARKMUX_CLIENTS_PAIRING_DIALOG",
    ];

    /// The `clients.*` rows under `env` (each key set or cleared) and `cfg`,
    /// the environment restored after.
    fn rows_with(env: [Option<&str>; 5], cfg: darkmux_types::config::DarkmuxConfig) -> Vec<Check> {
        let prev: Vec<Option<String>> = KEYS.iter().map(|k| std::env::var(k).ok()).collect();
        for (k, v) in KEYS.iter().zip(env) {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let _guard = darkmux_types::config_access::set_config_for_test(cfg);
        let rows = check_clients_settings();
        for (k, v) in KEYS.iter().zip(prev) {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        rows
    }

    fn message<'a>(rows: &'a [Check], name: &str) -> &'a str {
        &rows.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no {name} row: {rows:?}")).message
    }

    /// `darkmux doctor` reports a row for each `clients.*` setting, the
    /// rows `config get` points at for their resolved values; (#3560) then
    /// the tailnet row, and `clients.remote`'s own enum row among the enum
    /// settings.
    #[serial_test::serial]
    #[test]
    fn doctor_reports_a_row_for_each_clients_setting() {
        let _state = darkmux_types::test_isolation::IsolatedState::new();
        let names: Vec<String> =
            crate::run().checks.into_iter().map(|c| c.name).filter(|n| n.starts_with("clients.") || n == REMOTE_CHECK_NAME).collect();
        assert_eq!(names, ["clients.profiles", "clients.preview.enabled", "clients.preview.idle_minutes", "clients.pairing.verified_teams", "clients.pairing.dialog", REMOTE_CHECK_NAME, "clients.remote"]);
    }

    fn net_ready() -> darkmux_fleet::overlay::LocalNetwork {
        let mut net = darkmux_fleet::overlay::LocalNetwork { installed: true, version: Some("1.102.4".into()), ..Default::default() };
        darkmux_fleet::overlay::read_status_into(
            &mut net,
            br#"{"BackendState":"Running","Self":{"DNSName":"laptop.example.ts.net."},"CertDomains":["laptop.example.ts.net"]}"#,
        );
        net
    }

    /// (#3560) `clients.remote`'s row: off looks only at darkmux's own
    /// records (a file read, never the tool) and warns when one is there;
    /// on and usable names the URL (published now, or what `client serve`
    /// publishes); a record whose entry is gone, under an old name, or
    /// unreadable says which; on and unusable warns with the reason and the
    /// fix; a bad value is not checked.
    #[test]
    fn the_remote_row_says_off_working_or_why_not() {
        use darkmux_fleet::overlay::{LocalNetwork, PublishedRecord, ServeEntry};
        use darkmux_types::config::ClientsRemote;
        let never = |_: &[PublishedRecord]| -> RemoteLook { panic!("off must not look at the tailnet") };
        let entry = ServeEntry::for_port("laptop.example.ts.net", 8767);
        let rec = vec![PublishedRecord { entry: entry.clone(), pid: 1 }];
        let off = remote_check(Ok((ClientsRemote::Off, Source::BuiltIn)), Vec::new(), never);
        assert_eq!((off.name.as_str(), off.status), (REMOTE_CHECK_NAME, Status::Pass));
        assert!(off.message.starts_with("off (default)") && off.message.contains("this machine only"), "{off:?}");
        let left = remote_check(Ok((ClientsRemote::Off, Source::BuiltIn)), rec.clone(), never);
        assert_eq!(left.status, Status::Warn, "{left:?}");
        assert!(left.message.contains("https://laptop.example.ts.net:8767/") && left.message.contains("next `client serve` removes"), "{left:?}");
        assert!(left.hint.as_deref().is_some_and(|h| h.contains("tailscale serve --https=8767 --set-path=/ off")), "{left:?}");

        let on = Ok((ClientsRemote::Tailscale, Source::Config));
        let look = |published: Vec<(ServeEntry, Shown)>| move |_: &[PublishedRecord]| RemoteLook { net: net_ready(), published };
        let ready = remote_check(on.clone(), Vec::new(), look(vec![]));
        assert_eq!(ready.status, Status::Pass, "{ready:?}");
        assert!(ready.message.starts_with("tailscale (from config.json)"), "{ready:?}");
        assert!(ready.message.contains("https://laptop.example.ts.net:<port>/") && ready.message.contains("refused"), "{ready:?}");

        let live = remote_check(on.clone(), rec.clone(), look(vec![(entry.clone(), Shown::Exactly)]));
        assert_eq!(live.status, Status::Pass, "{live:?}");
        assert!(live.message.contains("on the tailnet at https://laptop.example.ts.net:8767/"), "{live:?}");

        let gone = remote_check(on.clone(), rec.clone(), look(vec![(entry.clone(), Shown::Not)]));
        assert_eq!(gone.status, Status::Warn, "{gone:?}");
        assert!(gone.message.contains("no longer shows"), "{gone:?}");

        let unread = remote_check(on.clone(), rec.clone(), look(vec![(entry.clone(), Shown::Unreadable("permission denied".into()))]));
        assert_eq!(unread.status, Status::Warn, "{unread:?}");
        assert!(unread.message.contains("could not read") && unread.message.contains("permission denied"), "{unread:?}");
        assert!(!unread.message.contains("no longer shows"), "{unread:?}");

        let old = ServeEntry::for_port("mini.example.ts.net", 8767);
        let renamed = remote_check(on.clone(), rec.clone(), look(vec![(old, Shown::Exactly)]));
        assert_eq!(renamed.status, Status::Warn, "{renamed:?}");
        assert!(renamed.message.contains("renamed") && renamed.message.contains("tailscale serve reset"), "{renamed:?}");

        let down = remote_check(on, Vec::new(), |_: &[PublishedRecord]| RemoteLook { net: LocalNetwork::default(), published: vec![] });
        assert_eq!(down.status, Status::Warn, "{down:?}");
        assert!(down.message.contains("not installed") && down.message.contains("this machine only"), "{down:?}");
        assert!(down.hint.as_deref().is_some_and(|h| h.contains("install Tailscale") && h.contains("clients.remote off")), "{down:?}");

        let s = darkmux_types::config_enum::setting("clients.remote").unwrap();
        let cfg = darkmux_types::config_enum::config_with_value(s, "funnel");
        let bad = darkmux_types::config_enum::resolve_in(s, &cfg, |_| None).map(|_| (ClientsRemote::Off, Source::Config));
        let bad = remote_check(bad, Vec::new(), never);
        assert_eq!(bad.status, Status::Warn, "{bad:?}");
        assert!(bad.message.contains("`funnel`") && bad.message.contains("not checked"), "{bad:?}");
    }

    /// (#3560 review) A `serve status` that could not be read, or is not
    /// JSON, is said as such, never as an entry the tailnet no longer shows.
    #[test]
    fn an_unreadable_serve_status_is_not_read_as_a_missing_entry() {
        use darkmux_fleet::overlay::ServeEntry;
        let e = ServeEntry::for_port("laptop.example.ts.net", 8767);
        assert_eq!(shown_in(&Err("permission denied".into()), &e), Shown::Unreadable("permission denied".into()));
        assert!(matches!(shown_in(&Ok(b"not json".to_vec()), &e), Shown::Unreadable(_)));
        assert_eq!(shown_in(&Ok(b"{}".to_vec()), &e), Shown::Not);
        let ours = br#"{"TCP":{"8767":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8767":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8767"}}}}}"#;
        assert_eq!(shown_in(&Ok(ours.to_vec()), &e), Shown::Exactly);
    }

    /// Each row says the resolved value and the tier that set it.
    #[serial_test::serial]
    #[test]
    fn each_clients_row_says_its_value_and_where_it_came_from() {
        let rows = rows_with([None; 5], Default::default());
        let names: Vec<&str> = rows.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["clients.profiles", "clients.preview.enabled", "clients.preview.idle_minutes", "clients.pairing.verified_teams", "clients.pairing.dialog"]);
        assert!(rows.iter().all(|c| c.status == Status::Pass), "{rows:?}");
        assert!(message(&rows, "clients.profiles").starts_with("unset (default)"), "{rows:?}");
        assert!(message(&rows, "clients.preview.enabled").starts_with("off (default)"), "{rows:?}");
        assert!(message(&rows, "clients.preview.idle_minutes").starts_with("15 (default)"), "{rows:?}");
        assert!(message(&rows, "clients.pairing.verified_teams").starts_with("5LDXYW8B6T (default)"), "{rows:?}");
        assert!(message(&rows, "clients.pairing.dialog").starts_with("on (default)"), "{rows:?}");

        let cfg = darkmux_types::config::DarkmuxConfig {
            clients: Some(darkmux_types::config::ClientsConfig {
                profiles: Some(vec!["deep".into()]),
                preview: Some(darkmux_types::config::ClientsPreviewConfig {
                    enabled: Some(true),
                    idle_minutes: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rows = rows_with([None; 5], cfg.clone());
        assert!(message(&rows, "clients.profiles").starts_with("deep (from config.json)"), "{rows:?}");
        assert!(message(&rows, "clients.preview.enabled").starts_with("on (from config.json)"), "{rows:?}");
        assert!(message(&rows, "clients.preview.idle_minutes").starts_with("0 (from config.json)"), "{rows:?}");

        let rows = rows_with([Some("fast,tiny"), Some("false"), Some("5"), Some("AAAAAAAAAA"), Some("off")], cfg);
        assert!(
            message(&rows, "clients.profiles").starts_with("fast, tiny (from DARKMUX_CLIENTS_PROFILES env)"),
            "{rows:?}"
        );
        assert!(message(&rows, "clients.preview.enabled").starts_with("off (from DARKMUX_CLIENTS_PREVIEW_ENABLED env)"));
        assert!(message(&rows, "clients.preview.idle_minutes").starts_with("5 (from DARKMUX_CLIENTS_PREVIEW_IDLE_MINUTES env)"));
        assert!(message(&rows, "clients.pairing.verified_teams").starts_with("AAAAAAAAAA (from DARKMUX_CLIENTS_PAIRING_VERIFIED_TEAMS env)"));
        assert!(message(&rows, "clients.pairing.dialog").starts_with("off (from DARKMUX_CLIENTS_PAIRING_DIALOG env)"));
    }
}
