//! (#3112) The sandbox every isolated run uses (`runtime.sandbox`), and
//! whether it can run here. Docker's own rows (its daemon, its image, the
//! daemon's PATH, the runtime binary cache) are asked only when the sandbox
//! is Docker; under any other sandbox each says Docker is not needed.

use crate::{not_applicable, Check, Status};
use darkmux_sandbox::BackendKind;
use darkmux_types::config_access::Source;

/// The row's name in doctor's output and `--json`.
pub(crate) const SANDBOX_CHECK_NAME: &str = "sandbox";

/// The sandbox `runtime.sandbox` resolves to here, as every entry point
/// that starts one resolves it, and the tier that decided.
pub(crate) fn configured() -> (Result<BackendKind, String>, Source) {
    let source = darkmux_types::config_access::sandbox_with_source().map_or(Source::BuiltIn, |(_, s)| s);
    (darkmux_sandbox::selected(), source)
}

/// One row: whether the configured sandbox can run here. Seatbelt passes
/// where `sandbox-exec` is (the selection already refused it where not);
/// Docker is as ready as the `docker runtime` row (`docker`) says, never
/// probed twice. `seatbelt_here`: Seatbelt could run on this host, so a
/// Docker problem can name the way to need no Docker. `host_runtime`: for a
/// sandbox on the host, the runtime it would run, as a dispatch chooses and
/// identity-checks it (#3112 review); `None` reads as not checked.
pub(crate) fn sandbox_row(
    selected: &Result<BackendKind, String>,
    source: Source,
    docker: Option<&Check>,
    seatbelt_here: bool,
    host_runtime: Option<&Result<String, String>>,
) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: SANDBOX_CHECK_NAME.into(), status, message, hint };
    let kind = match selected {
        Ok(kind) => *kind,
        Err(why) => return check(Status::Fail, format!("no sandbox can run, so nothing isolated (a dispatch, a lab run's tests, the mods gate) can start: {why}"), None),
    };
    let named = format!("`{}` ({})", darkmux_sandbox::setting_value(kind), from(source));
    match (kind, docker) {
        (BackendKind::Seatbelt, _) => seatbelt_row(&named, host_runtime, source),
        (BackendKind::Docker, Some(row)) if row.status == Status::Pass => {
            check(Status::Pass, format!("{named}: every isolated run here runs in a Docker container ({})", row.message), None)
        }
        (BackendKind::Docker, row) => check(
            row.map_or(Status::Warn, |r| r.status),
            format!("{named}: Docker cannot run isolated work here yet (see `docker runtime`)"),
            seatbelt_here.then(|| {
                "to need no Docker on this Mac, `darkmux config set runtime.sandbox seatbelt` (macOS's own sandbox)".to_string()
            }),
        ),
    }
}

/// Seatbelt: present (the selection already refused it where not), so the
/// row is whether its runtime can run.
fn seatbelt_row(named: &str, host_runtime: Option<&Result<String, String>>, source: Source) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: SANDBOX_CHECK_NAME.into(), status, message, hint };
    let exec = darkmux_sandbox::seatbelt::SANDBOX_EXEC;
    match host_runtime {
        Some(Err(why)) => check(
            Status::Fail,
            format!("{named}: macOS Seatbelt ({exec}) is here, but no dispatch can start: its runtime cannot run: {why}"),
            Some(format!("restart the darkmux process that will dispatch; or: {}", darkmux_sandbox::way_back_to_docker_given(source))),
        ),
        Some(Ok(program)) => check(
            Status::Pass,
            format!(
                "{named}: macOS Seatbelt ({exec}); a darkmux started now runs its runtime as `{program}`, so Docker is \
                 not needed. A darkmux process already running since this one was replaced (the daemon after an upgrade) \
                 is not seen here, and refuses until it restarts"
            ),
            None,
        ),
        None => check(Status::Pass, format!("{named}: macOS Seatbelt ({exec}) runs every isolated run here; Docker is not needed"), None),
    }
}

/// (#3112 review) The row naming user mission configs whose step messages
/// say the container's `/workspace`, read from `darkmux`'s own mission-config
/// directory.
pub(crate) fn check_mission_config_workspace_paths() -> Check {
    check_mission_config_workspace_paths_in(&darkmux_crew::loader::mission_configs_dir())
}

/// [`check_mission_config_workspace_paths`] over the `*.json` files in `dir`.
pub(crate) fn check_mission_config_workspace_paths_in(dir: &std::path::Path) -> Check {
    let name = "mission config workspace paths";
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            let at = darkmux_crew::mission_config::workspace_paths::container_workspace_messages(&doc);
            (!at.is_empty()).then(|| format!("{} ({})", p.display(), at.join(", ")))
        })
        .collect();
    found.sort();
    if found.is_empty() {
        return Check { name: name.into(), status: Status::Pass, message: "no user mission config names `/workspace` in a step message".into(), hint: None };
    }
    Check {
        name: name.into(),
        status: Status::Warn,
        message: format!(
            "a step message names the container's `/workspace`: {}. Under a sandbox on the host the runtime renames it \
             at run time, but only in the text the model reads",
            found.join("; ")
        ),
        hint: Some(
            "write `${WORKSPACE_ROOT}` in its place: the step names the tree where its sandbox sees it, `/workspace` in a container"
                .into(),
        ),
    }
}

/// Where the value came from, for a person.
fn from(source: Source) -> &'static str {
    match source {
        Source::Env => "from DARKMUX_SANDBOX",
        Source::Config => "from config.json",
        _ => "this machine's default",
    }
}

/// `probe`'s row when the sandbox is Docker; otherwise the row `name`, not
/// applicable, saying why, with `probe` never run (a Docker probe can take
/// seconds against a wedged daemon).
pub(crate) fn docker_only(selected: &Result<BackendKind, String>, name: &str, probe: impl FnOnce() -> Check) -> Check {
    match selected {
        Ok(BackendKind::Docker) => probe(),
        Ok(kind) => not_applicable(
            name,
            &format!("Docker is not needed: `runtime.sandbox` is `{}`", darkmux_sandbox::setting_value(*kind)),
        ),
        Err(_) => not_applicable(name, "no sandbox can run here, so Docker is not asked (see `sandbox`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#3112 review) A user mission config whose step message names the
    /// container's `/workspace` is named, with the placeholder to use; a
    /// directory with none, or none at all, passes.
    #[test]
    fn a_user_mission_config_naming_workspace_is_named_with_the_placeholder() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("review-judged.json"),
            r#"{"phases":[{"tasks":[{"steps":[{"config":{"message":"mounted at `/workspace/<source>`"}}]}]}]}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("fine.json"), r#"{"message":"at `${WORKSPACE_ROOT}/<source>`"}"#).unwrap();
        let c = check_mission_config_workspace_paths_in(dir.path());
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("review-judged.json") && !c.message.contains("fine.json"), "{}", c.message);
        assert!(c.hint.as_deref().unwrap_or("").contains("${WORKSPACE_ROOT}"), "{:?}", c.hint);
        std::fs::remove_file(dir.path().join("review-judged.json")).unwrap();
        assert_eq!(check_mission_config_workspace_paths_in(dir.path()).status, Status::Pass);
        assert_eq!(check_mission_config_workspace_paths_in(&dir.path().join("none")).status, Status::Pass);
    }

    fn row(status: Status, message: &str) -> Check {
        Check { name: "docker runtime".into(), status, message: message.into(), hint: Some("Start Docker Desktop".into()) }
    }

    /// Seatbelt that can run passes, and says Docker is not needed.
    #[test]
    fn seatbelt_passes_and_needs_no_docker() {
        let c = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, true, Some(&Ok("/x/darkmux runtime".into())));
        assert_eq!((c.name.as_str(), c.status), (SANDBOX_CHECK_NAME, Status::Pass));
        for part in ["`seatbelt`", "this machine's default", "Docker is not needed"] {
            assert!(c.message.contains(part), "missing `{part}` in: {}", c.message);
        }
    }

    /// (#3112 review, regression) Seatbelt as this machine's default with
    /// no runtime it can run fails, naming why and the way back: doctor
    /// asks the same identity check a dispatch does, about THIS process's
    /// executable: a green row means a darkmux started now can dispatch; a
    /// long-running process whose binary was replaced is not seen (the row
    /// says so); it refuses on its own.
    #[test]
    fn a_seatbelt_default_with_no_runnable_runtime_fails() {
        let why = "darkmux was replaced since this process started; Restart this darkmux process";
        let c = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, true, Some(&Err(why.into())));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains(why) && c.message.contains("`seatbelt`"), "{}", c.message);
        assert!(c.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox docker"), "{:?}", c.hint);
        // (#3112 review) When the env var chose Seatbelt, the way back is
        // the env var's own; a `config set` under it would change nothing.
        let env = sandbox_row(&Ok(BackendKind::Seatbelt), Source::Env, None, true, Some(&Err(why.into())));
        assert!(env.hint.as_deref().unwrap_or("").contains("export DARKMUX_SANDBOX=docker"), "{:?}", env.hint);
        let ok = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, true, Some(&Ok("/opt/homebrew/bin/darkmux runtime".into())));
        assert_eq!(ok.status, Status::Pass);
        assert!(ok.message.contains("/opt/homebrew/bin/darkmux runtime"), "{}", ok.message);
    }

    /// A sandbox that cannot run here (Seatbelt with no `sandbox-exec`, or a
    /// value no entry point accepts) fails, naming why.
    #[test]
    fn a_sandbox_that_cannot_run_here_fails() {
        let why = "runtime.sandbox is `seatbelt`, but this host has no /usr/bin/sandbox-exec";
        let c = sandbox_row(&Err(why.into()), Source::Config, None, false, None);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains(why), "{}", c.message);
    }

    /// Docker is as ready as its own row says; the Docker remedy appears
    /// only under Docker, and the way to need no Docker only where Seatbelt
    /// can run.
    #[test]
    fn docker_is_as_ready_as_its_own_row_says() {
        let up = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&row(Status::Pass, "ready")), true, None);
        assert_eq!(up.status, Status::Pass);
        assert!(up.message.contains("`docker` (from config.json)"), "{}", up.message);

        let down = row(Status::Warn, "the daemon isn't reachable");
        let mac = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&down), true, None);
        assert_eq!(mac.status, Status::Warn);
        assert!(mac.message.contains("docker runtime"), "{}", mac.message);
        assert!(mac.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox seatbelt"), "{:?}", mac.hint);
        let linux = sandbox_row(&Ok(BackendKind::Docker), Source::BuiltIn, Some(&down), false, None);
        assert!(!linux.hint.as_deref().unwrap_or("").contains("seatbelt"), "{:?}", linux.hint);
    }

    /// Under Seatbelt a Docker row is not asked: its probe never runs, and
    /// the row says why. Under Docker it runs as before.
    #[test]
    fn a_docker_row_is_asked_only_under_docker() {
        let asked = std::cell::Cell::new(0);
        let probe = || {
            asked.set(asked.get() + 1);
            row(Status::Warn, "probed")
        };
        let skipped = docker_only(&Ok(BackendKind::Seatbelt), "docker runtime", probe);
        assert_eq!(asked.get(), 0, "the Docker probe ran under Seatbelt");
        assert_eq!((skipped.name.as_str(), skipped.status), ("docker runtime", Status::Pass));
        assert!(skipped.message.contains("Docker is not needed: `runtime.sandbox` is `seatbelt`"), "{}", skipped.message);
        assert!(skipped.hint.is_none());

        let refused = docker_only(&Err("bad".into()), "docker runtime", probe);
        assert_eq!(asked.get(), 0);
        assert!(refused.message.contains("see `sandbox`"), "{}", refused.message);

        let ran = docker_only(&Ok(BackendKind::Docker), "docker runtime", probe);
        assert_eq!(asked.get(), 1);
        assert_eq!(ran.message, "probed");
    }
}
