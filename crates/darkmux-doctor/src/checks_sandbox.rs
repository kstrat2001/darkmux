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

/// What this host could run besides the configured sandbox, for the row.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Here {
    /// The sandbox on the host this host can run (Seatbelt on a Mac,
    /// Landlock on a Linux whose kernel has what it needs), so a Docker
    /// problem can name the way to need no Docker.
    pub(crate) on_host: Option<BackendKind>,
    /// This kernel's Landlock ABI, which the Landlock row names.
    pub(crate) landlock_abi: Option<u32>,
}

impl Here {
    /// This host, asked.
    pub(crate) fn now() -> Here {
        let on_host = if darkmux_sandbox::SeatbeltBackend::available() {
            Some(BackendKind::Seatbelt)
        } else {
            darkmux_sandbox::LandlockBackend::available().ok().map(|()| BackendKind::Landlock)
        };
        Here { on_host, landlock_abi: darkmux_platform::current().confine_support().landlock_abi }
    }
}

/// One row: whether the configured sandbox can run here. Seatbelt passes
/// where `sandbox-exec` is, Landlock where the kernel has what it needs (the
/// selection already refused each where not); Docker is as ready as the
/// `docker runtime` row (`docker`) says, never probed twice. `here`: what
/// else this host could run. `host_runtime`: for a sandbox on the host, the
/// runtime it would run, as a dispatch chooses and identity-checks it
/// (#3112 review); `None` reads as not checked.
pub(crate) fn sandbox_row(
    selected: &Result<BackendKind, String>,
    source: Source,
    docker: Option<&Check>,
    here: Here,
    host_runtime: Option<&Result<String, String>>,
) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: SANDBOX_CHECK_NAME.into(), status, message, hint };
    let kind = match selected {
        Ok(kind) => *kind,
        Err(why) => return check(Status::Fail, format!("no sandbox can run, so nothing isolated (a dispatch, a lab run's tests, the mods gate) can start: {why}"), None),
    };
    let named = format!("`{}` ({})", darkmux_sandbox::setting_value(kind), from(source));
    match (kind, docker) {
        (BackendKind::Seatbelt, _) => {
            host_row(&named, &format!("macOS Seatbelt ({})", darkmux_sandbox::seatbelt::SANDBOX_EXEC), kind, host_runtime, source)
        }
        (BackendKind::Landlock, _) => {
            let abi = here.landlock_abi.map_or_else(|| "ABI unknown".to_string(), |abi| format!("ABI {abi}"));
            host_row(&named, &format!("Linux namespaces and Landlock ({abi})"), kind, host_runtime, source)
        }
        (BackendKind::Docker, Some(row)) if row.status == Status::Pass => {
            check(Status::Pass, format!("{named}: every isolated run here runs in a Docker container ({})", row.message), None)
        }
        (BackendKind::Docker, row) => check(
            row.map_or(Status::Warn, |r| r.status),
            format!("{named}: Docker cannot run isolated work here yet (see `docker runtime`)"),
            here.on_host.map(|alternative| match alternative {
                BackendKind::Landlock => {
                    "to need no Docker here, `darkmux config set runtime.sandbox landlock` (the kernel's own sandbox)".to_string()
                }
                _ => "to need no Docker on this Mac, `darkmux config set runtime.sandbox seatbelt` (macOS's own sandbox)".to_string(),
            }),
        ),
    }
}

/// A sandbox on the host (`what`: Seatbelt, Landlock): present (the
/// selection already refused it where not), so the row is whether its
/// runtime can run.
fn host_row(named: &str, what: &str, kind: BackendKind, host_runtime: Option<&Result<String, String>>, source: Source) -> Check {
    let check = |status, message: String, hint: Option<String>| Check { name: SANDBOX_CHECK_NAME.into(), status, message, hint };
    match host_runtime {
        Some(Err(why)) => check(
            Status::Fail,
            format!("{named}: {what} is here, but no dispatch can start: its runtime cannot run: {why}"),
            Some(format!(
                "restart the darkmux process that will dispatch; or: {}",
                darkmux_sandbox::way_back_to_docker_given(darkmux_sandbox::setting_value(kind), source)
            )),
        ),
        Some(Ok(program)) => check(
            Status::Pass,
            format!(
                "{named}: {what}; a darkmux started now runs its runtime as `{program}`, so Docker is \
                 not needed. A darkmux process already running since this one was replaced (the daemon after an upgrade) \
                 is not seen here, and refuses until it restarts"
            ),
            None,
        ),
        None => check(Status::Pass, format!("{named}: {what} runs every isolated run here; Docker is not needed"), None),
    }
}

/// (#3561) Each runtime darkmux supplies to every sandbox it starts on the
/// host (Node), one row each, named `managed <runtime>`, as `statuses`
/// (from [`darkmux_crew::managed_toolchain::statuses`]) reads it: the
/// default build for a project that declares no version, installed and
/// verified (its version and path), or fetched on first use. A machine with
/// no pinned build warns: a sandbox for a project that declares the
/// runtime is refused there, one that declares none runs without it. Under
/// Docker the rows are not asked (the image carries its own runtimes).
pub(crate) fn managed_runtime_rows(
    selected: &Result<BackendKind, String>,
    statuses: impl FnOnce() -> Vec<(&'static darkmux_crew::managed_toolchain::Runtime, darkmux_crew::managed_toolchain::Status)>,
    c_library: Option<darkmux_platform::CLibrary>,
) -> Vec<Check> {
    let names = || darkmux_crew::managed_toolchain::RUNTIMES.iter().map(|r| format!("managed {}", r.name));
    match selected {
        Ok(kind) if kind.on_host() => statuses().into_iter().map(|(r, s)| runtime_row(*kind, r, s, c_library)).collect(),
        Ok(_) => names().map(|n| not_applicable(&n, "a container runs its image's own runtimes")).collect(),
        Err(_) => names().map(|n| not_applicable(&n, "no sandbox can run here (see `sandbox`)")).collect(),
    }
}

/// [`managed_runtime_rows`] read from `platform`: its runtimes' statuses
/// and its C library.
pub(crate) fn managed_runtimes_on(selected: &Result<BackendKind, String>, platform: &dyn darkmux_platform::Platform) -> Vec<Check> {
    managed_runtime_rows(selected, || darkmux_crew::managed_toolchain::statuses_on(platform), platform.c_library())
}

/// One managed runtime's row under the sandbox on the host `kind`.
/// `c_library`: this machine's, which names why no build is pinned when it
/// is musl.
fn runtime_row(
    kind: BackendKind,
    runtime: &darkmux_crew::managed_toolchain::Runtime,
    status: darkmux_crew::managed_toolchain::Status,
    c_library: Option<darkmux_platform::CLibrary>,
) -> Check {
    use darkmux_crew::managed_toolchain::Status as Rt;
    let check = |status, message: String, hint: Option<String>| Check { name: format!("managed {}", runtime.name), status, message, hint };
    let display = runtime.display;
    let others = "a project's own declaration (for Node `.nvmrc`, `.node-version` or `engines.node`) picks a pinned version \
                  and requires it (the sandbox is refused when it cannot be had); a project that declares no version gets \
                  this build when it can be had and otherwise runs without it";
    match status {
        Rt::Installed(i) => check(
            Status::Pass,
            format!(
                "{display} {} at {} (verified against its pinned SHA-256 when installed): first on the PATH of a \
                 sandbox darkmux starts here for a project that declares no version; {others}",
                i.pin.version,
                i.dir.display()
            ),
            None,
        ),
        Rt::WillFetch(pin) => check(
            Status::Pass,
            format!(
                "{display} {} is fetched from {} and verified against its pinned SHA-256 on first use, then first on \
                 the PATH of a sandbox darkmux starts here for a project that declares no version; {others}",
                pin.version,
                pin.url
            ),
            None,
        ),
        Rt::Unsupported => check(
            Status::Warn,
            format!(
                "darkmux pins no {display} build for this machine's {}: under `{}` a sandbox for a project that \
                 declares a {display} version is refused here, and one for a project that declares none runs without \
                 darkmux's {display}",
                match c_library {
                    Some(darkmux_platform::CLibrary::Musl) => "C library (musl: the official builds link glibc)",
                    _ => "CPU",
                },
                darkmux_sandbox::setting_value(kind)
            ),
            Some(format!(
                "to run in a container instead, `darkmux config set {} {}`",
                darkmux_sandbox::SANDBOX_KEY,
                darkmux_sandbox::setting_value(BackendKind::Docker)
            )),
        ),
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

    const MAC: Here = Here { on_host: Some(BackendKind::Seatbelt), landlock_abi: None };

    fn row(status: Status, message: &str) -> Check {
        Check { name: "docker runtime".into(), status, message: message.into(), hint: Some("Start Docker Desktop".into()) }
    }

    /// Seatbelt that can run passes, and says Docker is not needed.
    #[test]
    fn seatbelt_passes_and_needs_no_docker() {
        let c = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, MAC, Some(&Ok("/x/darkmux runtime".into())));
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
        let c = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, MAC, Some(&Err(why.into())));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains(why) && c.message.contains("`seatbelt`"), "{}", c.message);
        assert!(c.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox docker"), "{:?}", c.hint);
        // (#3112 review) When the env var chose Seatbelt, the way back is
        // the env var's own; a `config set` under it would change nothing.
        let env = sandbox_row(&Ok(BackendKind::Seatbelt), Source::Env, None, MAC, Some(&Err(why.into())));
        assert!(env.hint.as_deref().unwrap_or("").contains("export DARKMUX_SANDBOX=docker"), "{:?}", env.hint);
        let ok = sandbox_row(&Ok(BackendKind::Seatbelt), Source::BuiltIn, None, MAC, Some(&Ok("/opt/homebrew/bin/darkmux runtime".into())));
        assert_eq!(ok.status, Status::Pass);
        assert!(ok.message.contains("/opt/homebrew/bin/darkmux runtime"), "{}", ok.message);
    }

    /// A sandbox that cannot run here (Seatbelt with no `sandbox-exec`, or a
    /// value no entry point accepts) fails, naming why.
    #[test]
    fn a_sandbox_that_cannot_run_here_fails() {
        let why = "runtime.sandbox is `seatbelt`, but this host has no /usr/bin/sandbox-exec";
        let c = sandbox_row(&Err(why.into()), Source::Config, None, Here::default(), None);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains(why), "{}", c.message);
    }

    /// (#3112) Landlock that can run passes, names the kernel's ABI and
    /// says Docker is not needed; a runtime it cannot run fails with the way
    /// back to Docker from `landlock`.
    #[test]
    fn landlock_passes_naming_its_abi() {
        let here = Here { on_host: Some(BackendKind::Landlock), landlock_abi: Some(9) };
        let c = sandbox_row(&Ok(BackendKind::Landlock), Source::BuiltIn, None, here, Some(&Ok("/usr/bin/darkmux runtime".into())));
        assert_eq!(c.status, Status::Pass);
        for part in ["`landlock`", "this machine's default", "Landlock (ABI 9)", "Docker is not needed"] {
            assert!(c.message.contains(part), "missing `{part}` in: {}", c.message);
        }
        let failed = sandbox_row(&Ok(BackendKind::Landlock), Source::Config, None, here, Some(&Err("replaced".into())));
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.hint.as_deref().unwrap_or("").contains("`runtime.sandbox` is `landlock`"), "{:?}", failed.hint);
    }

    /// Docker is as ready as its own row says; the Docker remedy appears
    /// only under Docker, and the way to need no Docker only where Seatbelt
    /// can run.
    #[test]
    fn docker_is_as_ready_as_its_own_row_says() {
        let up = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&row(Status::Pass, "ready")), MAC, None);
        assert_eq!(up.status, Status::Pass);
        assert!(up.message.contains("`docker` (from config.json)"), "{}", up.message);

        let down = row(Status::Warn, "the daemon isn't reachable");
        let mac = sandbox_row(&Ok(BackendKind::Docker), Source::Config, Some(&down), MAC, None);
        assert_eq!(mac.status, Status::Warn);
        assert!(mac.message.contains("docker runtime"), "{}", mac.message);
        assert!(mac.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox seatbelt"), "{:?}", mac.hint);
        let linux = sandbox_row(&Ok(BackendKind::Docker), Source::BuiltIn, Some(&down), Here::default(), None);
        assert!(!linux.hint.as_deref().unwrap_or("").contains("seatbelt"), "{:?}", linux.hint);
        let confinable = Here { on_host: Some(BackendKind::Landlock), landlock_abi: Some(9) };
        let landlock = sandbox_row(&Ok(BackendKind::Docker), Source::BuiltIn, Some(&down), confinable, None);
        assert!(landlock.hint.as_deref().unwrap_or("").contains("darkmux config set runtime.sandbox landlock"), "{:?}", landlock.hint);
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

    /// (#3561) A row per managed runtime: under a sandbox on the host, the
    /// default build darkmux verified (version, path), or that it is
    /// fetched on first use; a machine with no pinned build warns (only a declared runtime is refused there), naming
    /// the way to a container; under Docker it is not applicable (the image
    /// has its own runtimes) and the runtimes are never read.
    #[test]
    fn the_managed_runtime_rows_name_the_verified_install_or_the_first_use_fetch() {
        use darkmux_crew::managed_toolchain::{Installed, Status as Rt, NODE};
        let pin = NODE.pins[0];
        let seatbelt = Ok(BackendKind::Seatbelt);
        let installed = Installed { pin, dir: std::path::PathBuf::from("/h/.darkmux/toolchains/node-26.13.1-darwin-arm64"), env: Vec::new() };
        let rows = managed_runtime_rows(&seatbelt, || vec![(&NODE, Rt::Installed(installed.clone()))], None);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].name.as_str(), rows[0].status), ("managed node", Status::Pass));
        assert!(rows[0].message.contains("Node 26.13.1") && rows[0].message.contains("/h/.darkmux/toolchains/node-26.13.1-darwin-arm64") && rows[0].message.contains("verified"), "{}", rows[0].message);
        let rows = managed_runtime_rows(&seatbelt, || vec![(&NODE, Rt::WillFetch(pin))], None);
        assert_eq!(rows[0].status, Status::Pass);
        assert!(rows[0].message.contains("Node 26.13.1") && rows[0].message.contains("first use") && rows[0].message.contains("nodejs.org"), "{}", rows[0].message);
        assert!(rows[0].message.contains("declares no version") && rows[0].message.contains("runs without it"), "{}", rows[0].message);
        let rows = managed_runtime_rows(&Ok(BackendKind::Landlock), || vec![(&NODE, Rt::Unsupported)], Some(darkmux_platform::CLibrary::Glibc));
        assert_eq!(rows[0].status, Status::Warn, "only a project that declares Node is refused here");
        assert!(rows[0].message.contains("declares a Node version is refused") && rows[0].message.contains("declares none runs without"), "{}", rows[0].message);
        assert!(rows[0].message.contains("CPU") && !rows[0].message.contains("musl"), "{}", rows[0].message);
        let rows = managed_runtime_rows(&Ok(BackendKind::Landlock), || vec![(&NODE, Rt::Unsupported)], Some(darkmux_platform::CLibrary::Musl));
        assert!(rows[0].message.contains("C library (musl") && !rows[0].message.contains("CPU"), "the reason is the C library: {}", rows[0].message);
        assert!(rows[0].hint.as_deref().unwrap_or("").contains("runtime.sandbox docker"), "{:?}", rows[0].hint);
        let asked = std::cell::Cell::new(false);
        let rows = managed_runtime_rows(&Ok(BackendKind::Docker), || {
            asked.set(true);
            Vec::new()
        }, None);
        assert_eq!(rows.len(), 1, "one row per runtime even when not applicable");
        assert!(rows[0].message.contains("not applicable") && !asked.get(), "{}", rows[0].message);
        assert!(managed_runtime_rows(&Err("no".into()), Vec::new, None)[0].message.contains("not applicable"));
    }
}
