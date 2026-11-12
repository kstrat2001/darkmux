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
    /// rows `config get` points at for their resolved values.
    #[serial_test::serial]
    #[test]
    fn doctor_reports_a_row_for_each_clients_setting() {
        let _state = darkmux_types::test_isolation::IsolatedState::new();
        let names: Vec<String> =
            crate::run().checks.into_iter().map(|c| c.name).filter(|n| n.starts_with("clients.")).collect();
        assert_eq!(names, ["clients.profiles", "clients.preview.enabled", "clients.preview.idle_minutes", "clients.pairing.verified_teams", "clients.pairing.dialog"]);
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
