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
/// Docker problem can name the way to need no Docker.
pub(crate) fn sandbox_row(selected: &Result<BackendKind, String>, source: Source, docker: Option<&Check>, seatbelt_here: bool) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: SANDBOX_CHECK_NAME.into(), status, message, hint };
    let kind = match selected {
        Ok(kind) => *kind,
        Err(why) => return check(Status::Fail, format!("no sandbox can run, so nothing isolated (a dispatch, a lab run's tests, the mods gate) can start: {why}"), None),
    };
    let named = format!("`{}` ({})", darkmux_sandbox::setting_value(kind), from(source));
    match (kind, docker) {
        (BackendKind::Seatbelt, _) => check(
            Status::Pass,
            format!("{named}: macOS Seatbelt ({}) runs every isolated run here; Docker is not needed", darkmux_sandbox::seatbelt::SANDBOX_EXEC),
            None,
        ),
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

    fn row(status: Status, message: &str) -> Check {
        Check { name: "docker runtime".into(), status, message: message.into(), hint: Some("Start Docker Desktop".into()) }
    }

    /// Seatbelt that can run passes, and says Docker is not needed.
    #[test]
    fn seatbelt_passes_and_needs_no_docker() {
        let c = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, true);
        assert_eq!((c.name.as_str(), c.status), (SANDBOX_CHECK_NAME, Status::Pass));
        for part in ["`seatbelt`", "this machine's default", "Docker is not needed"] {
            assert!(c.message.contains(part), "missing `{part}` in: {}", c.message);
        }
    }

    /// A sandbox that cannot run here (Seatbelt with no `sandbox-exec`, or a
    /// value no entry point accepts) fails, naming why.
    #[test]
    fn a_sandbox_that_cannot_run_here_fails() {
        let why = "runtime.sandbox is `seatbelt`, but this host has no /usr/bin/sandbox-exec";
        let c = sandbox_row(&Err(why.into()), Source::Config, None, false);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains(why), "{}", c.message);
    }

    /// Docker is as ready as its own row says; the Docker remedy appears
    /// only under Docker, and the way to need no Docker only where Seatbelt
    /// can run.
    #[test]
    fn docker_is_as_ready_as_its_own_row_says() {
        let up = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&row(Status::Pass, "ready")), true);
        assert_eq!(up.status, Status::Pass);
        assert!(up.message.contains("`docker` (from config.json)"), "{}", up.message);

        let down = row(Status::Warn, "the daemon isn't reachable");
        let mac = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&down), true);
        assert_eq!(mac.status, Status::Warn);
        assert!(mac.message.contains("docker runtime"), "{}", mac.message);
        assert!(mac.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox seatbelt"), "{:?}", mac.hint);
        let linux = sandbox_row(&Ok(BackendKind::Docker), Source::BuiltIn, Some(&down), false);
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
