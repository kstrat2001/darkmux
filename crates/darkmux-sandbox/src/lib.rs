//! (#3112) Isolation as a seam: what a sandbox may see and do
//! ([`SandboxPolicy`]), the identity of one that runs ([`SandboxHandle`]),
//! and the backend that turns a policy into a process ([`SandboxBackend`]).
//!
//! Two backends: Docker ([`DockerBackend`]) and macOS Seatbelt
//! ([`SeatbeltBackend`]), picked by the `runtime.sandbox` setting
//! ([`selected`]). Callers build a policy and spawn the command a backend
//! gives them; they never spell a container engine's command line. Bounding
//! a run (deadline, interrupt, process-group kill) stays with the caller's
//! runner, so a backend only plans processes.
//!
//! Every sandbox, whatever the backend, is removed when it exits, has an init
//! as its first process (orphans are reaped, not left as zombies), drops
//! every capability and cannot gain privileges. A policy cannot turn those
//! off.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

pub mod docker;
pub use docker::DockerBackend;
pub mod seatbelt;
pub use seatbelt::SeatbeltBackend;

#[cfg(test)]
mod docker_calls;

/// Whether a sandbox may write through a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// A host path the sandbox sees, and where. A mount at a path inside an
/// earlier mount is layered over it: a read-write tree at `/workspace` with
/// its `.git` read-only at `/workspace/.git` sees the tree writable and its
/// `.git` not.
///
/// `at` places a host path at another path, which Docker (and bubblewrap,
/// and Apple's `container`) can do and Seatbelt and Landlock cannot: they
/// only allow or deny a host path where it is. How such a backend honors
/// `/workspace` is the first question of the Seatbelt step (#3112).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub at: String,
    pub access: Access,
}

/// What the sandbox can reach over the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// No interface but loopback.
    None,
    /// The backend's default network.
    Default,
}

/// Resource limits. `None` leaves the backend's own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limits {
    /// Memory, in the backend's size syntax (`4g`).
    pub memory: Option<String>,
    /// Processes the sandbox may hold at once.
    pub pids: Option<u64>,
}

/// Everything a sandbox may see and do. Nothing outside it is granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// The sandbox's name, unique on this machine; its handle's id.
    pub name: String,
    /// The image (the root filesystem and toolchain) it runs.
    pub image: String,
    pub network: Network,
    pub limits: Limits,
    /// `key=value` labels, by which a reaper finds the sandbox's owner.
    pub labels: Vec<(String, String)>,
    /// In order: a later mount at a nested path is layered over an earlier.
    pub mounts: Vec<Mount>,
    /// Ports the host reaches on its loopback, at the same number, and on
    /// no other address.
    pub ports: Vec<u16>,
    /// Whether the sandbox reads the caller's stdin (a secret piped in);
    /// otherwise it gets none.
    pub stdin: bool,
    /// A name inside the sandbox that reaches this host's own services (a
    /// model server on the host), where the backend does not provide one.
    pub host_alias: Option<String>,
    /// `uid:gid` to run as; `None` keeps the image's user.
    pub user: Option<String>,
    /// The whole environment the policy adds, each `KEY=value`; nothing of
    /// the host's is copied in.
    pub env: Vec<(String, String)>,
    /// The working directory, a path inside the sandbox.
    pub workdir: Option<String>,
    /// The program that runs in place of the image's entrypoint.
    pub entrypoint: Option<String>,
    /// Its arguments.
    pub command: Vec<String>,
}

/// Which backend a handle belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Docker,
    /// (#3112 step 2, spike) macOS `sandbox-exec`.
    Seatbelt,
}

/// A running (or once-running) sandbox, as a record can store it: which
/// backend made it, and its id there (a container's name for Docker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub backend: BackendKind,
    pub id: String,
}

/// Which sandboxes a listing returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Having {
    /// Those carrying this label key, whatever its value.
    Label(String),
    /// Those whose name starts with this.
    NamePrefix(String),
    /// The one with exactly this name.
    Name(String),
}

/// A listing: which sandboxes, whether stopped ones too, and which labels
/// to read from each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub having: Having,
    /// Sandboxes that have exited but not been removed, as well as running
    /// ones.
    pub stopped_too: bool,
    /// The label keys whose values each [`Listed`] carries, in this order.
    pub read: Vec<String>,
}

impl Query {
    /// Every sandbox carrying `key`, running or not, with its value.
    pub fn labeled(key: &str) -> Query {
        Query { having: Having::Label(key.to_string()), stopped_too: true, read: vec![key.to_string()] }
    }
}

/// One sandbox a listing found, with the value of each label its query
/// read, in the query's order: `None` where the sandbox has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub handle: SandboxHandle,
    pub labels: Vec<Option<String>>,
}

/// The env var over `runtime.sandbox`, the setting that picks the backend
/// (#3112): `docker` or `seatbelt`. Resolved by
/// `darkmux_types::config_access::sandbox` (env > config.json > this host's
/// default, `darkmux_platform::Platform::default_sandbox`).
pub const SANDBOX_ENV: &str = "DARKMUX_SANDBOX";

/// The `config.json` key that picks the backend.
pub const SANDBOX_KEY: &str = "runtime.sandbox";

/// The backend `runtime.sandbox` names, or why it names none this host can
/// use: an unregistered value (refused, never defaulted, contract 9), or
/// `seatbelt` where `sandbox-exec` is missing.
pub fn selected() -> Result<BackendKind, String> {
    let resolved = darkmux_types::config_access::sandbox_with_source().map_err(|bad| bad.to_string());
    selected_from(resolved, SeatbeltBackend::available())
}

fn selected_from(
    resolved: Result<(darkmux_types::config::SandboxKind, darkmux_types::config_access::Source), String>,
    seatbelt_available: bool,
) -> Result<BackendKind, String> {
    use darkmux_types::config::SandboxKind;
    match resolved? {
        (SandboxKind::Docker, _) => Ok(BackendKind::Docker),
        (SandboxKind::Seatbelt, _) if seatbelt_available => Ok(BackendKind::Seatbelt),
        (SandboxKind::Seatbelt, source) => Err(format!(
            "{} is `seatbelt`, but this host has no {}; to run in Docker instead, \
             `darkmux config set {SANDBOX_KEY} docker`{}",
            setting_named(source),
            seatbelt::SANDBOX_EXEC,
            if source == darkmux_types::config_access::Source::Env { format!(" and unset {SANDBOX_ENV}") } else { String::new() },
        )),
    }
}

/// The setting as the operator set it: the env var when it decided, else
/// the key (also when neither is set: the key is where to change it).
fn setting_named(source: darkmux_types::config_access::Source) -> String {
    match source {
        darkmux_types::config_access::Source::Env => format!("{SANDBOX_ENV} (over {SANDBOX_KEY})"),
        _ => SANDBOX_KEY.to_string(),
    }
}

/// The backend's `runtime.sandbox` token, for messages.
pub fn setting_value(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Docker => darkmux_types::config::SandboxKind::Docker.as_str(),
        BackendKind::Seatbelt => darkmux_types::config::SandboxKind::Seatbelt.as_str(),
    }
}

/// (#3112) Where isolated work runs under `kind`, for a message: "a
/// Docker container (`runtime.sandbox` is `docker`)", or "the `seatbelt`
/// sandbox (`runtime.sandbox`)".
pub fn placed_in(kind: BackendKind) -> String {
    match kind {
        BackendKind::Docker => format!("a Docker container (`{SANDBOX_KEY}` is `{}`)", setting_value(kind)),
        BackendKind::Seatbelt => format!("the `{}` sandbox (`{SANDBOX_KEY}`)", setting_value(kind)),
    }
}

/// (#3112) Where isolated work runs under the configured sandbox (Docker's
/// wording when the setting is refused: the entry point already refused it).
pub fn placed_in_configured() -> String {
    placed_in(selected().unwrap_or(BackendKind::Docker))
}

/// (#3112) For a message saying Docker could not run something: why Docker
/// was needed (the setting that chose it) and, where Seatbelt can run, how
/// to need no Docker. `None` when the configured sandbox is not Docker: a
/// Docker remedy never appears under another sandbox.
pub fn docker_remedy() -> Option<String> {
    let source = darkmux_types::config_access::sandbox_with_source().map_or(darkmux_types::config_access::Source::BuiltIn, |(_, s)| s);
    docker_remedy_given(selected().unwrap_or(BackendKind::Docker), source, SeatbeltBackend::available())
}

fn docker_remedy_given(kind: BackendKind, source: darkmux_types::config_access::Source, seatbelt_available: bool) -> Option<String> {
    if kind != BackendKind::Docker {
        return None;
    }
    let from_env = source == darkmux_types::config_access::Source::Env;
    let setting = if from_env { SANDBOX_ENV.to_string() } else { format!("`{SANDBOX_KEY}`") };
    let mut out = format!("{setting} is `{}`, so Docker must be installed and running", setting_value(kind));
    if seatbelt_available {
        let seatbelt = setting_value(BackendKind::Seatbelt);
        out.push_str(&format!("; to need no Docker on this Mac, `darkmux config set {SANDBOX_KEY} {seatbelt}`"));
        if from_env {
            out.push_str(&format!(" and unset {SANDBOX_ENV}"));
        }
    }
    Some(out)
}

/// The backend darkmux runs sandboxes through: the one [`selected`] names.
/// A refused selection falls back to Docker here; every entry point that
/// starts a sandbox calls [`selected`] first and refuses it there.
pub fn backend() -> &'static dyn SandboxBackend {
    match selected() {
        Ok(BackendKind::Seatbelt) => &SeatbeltBackend,
        _ => &DockerBackend,
    }
}

/// A way to run a policy. Each method plans a process for the caller to
/// spawn and bound; none runs anything itself.
pub trait SandboxBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    /// Where the sandbox sees `mount`: its `at` for a backend that places
    /// mounts (Docker), its host path for one that cannot (Seatbelt). A
    /// caller builds every in-sandbox path into a mount from this, never
    /// from `at` (#3112 step 2).
    fn place(&self, mount: &Mount) -> String;

    /// The command that runs `policy` attached until it exits, and the
    /// handle naming the sandbox. Killing that command's process need not
    /// stop the sandbox: a caller whose run was cut short [`remove`]s it.
    ///
    /// [`remove`]: SandboxBackend::remove
    fn run(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle);

    /// The command that makes `policy`'s sandbox without starting it, and
    /// its handle. Once that command succeeds, a [`remove`] finds it, and a
    /// sandbox removed before it starts never runs. A [`stop`] is not
    /// enough: it ends a running sandbox and leaves a created one to start
    /// later, so a caller ending a created sandbox removes it.
    ///
    /// [`stop`]: SandboxBackend::stop
    /// [`remove`]: SandboxBackend::remove
    fn create(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle);

    /// The command that starts a created sandbox, attached until it exits.
    fn start(&self, handle: &SandboxHandle) -> Command;

    /// The command that asks the sandbox to end (SIGTERM to its tree) and
    /// ends it after `grace` (whole seconds) if it has not.
    fn stop(&self, handle: &SandboxHandle, grace: std::time::Duration) -> Command;

    /// The command that ends `handle`'s sandbox at once (SIGKILL to its
    /// tree). It succeeds only when the sandbox was running, so its success
    /// is proof the sandbox was there; a sandbox that ends this way is
    /// removed as any other is when it exits.
    fn kill(&self, handle: &SandboxHandle) -> Command;

    /// The command that stops and removes `handle`'s sandbox, its whole
    /// process tree with it. Removing one already gone succeeds or fails
    /// harmlessly; either way it is gone.
    fn remove(&self, handle: &SandboxHandle) -> Command;

    /// The command that lists the sandboxes `query` asks for;
    /// [`parse_list`] reads its output.
    ///
    /// [`parse_list`]: SandboxBackend::parse_list
    fn list(&self, query: &Query) -> Command;

    /// The sandboxes in [`list`]'s output for `query`, each with one value
    /// per label `query` reads.
    ///
    /// [`list`]: SandboxBackend::list
    fn parse_list(&self, query: &Query, listing: &str) -> Vec<Listed>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#3112) The backend is the `runtime.sandbox` setting as the types
    /// crate resolves it (env over config over this host's default). A
    /// refused value stays refused; `seatbelt` is refused where
    /// `sandbox-exec` is missing, naming the setting and the way to Docker.
    #[test]
    fn the_backend_is_the_sandbox_setting_and_refuses_what_cannot_run() {
        use darkmux_types::config::SandboxKind;
        use darkmux_types::config_access::Source;
        assert_eq!(selected_from(Ok((SandboxKind::Docker, Source::BuiltIn)), false), Ok(BackendKind::Docker));
        assert_eq!(selected_from(Ok((SandboxKind::Seatbelt, Source::Config)), true), Ok(BackendKind::Seatbelt));
        let missing = selected_from(Ok((SandboxKind::Seatbelt, Source::Config)), false).unwrap_err();
        for part in ["runtime.sandbox", "seatbelt", seatbelt::SANDBOX_EXEC, "darkmux config set runtime.sandbox docker"] {
            assert!(missing.contains(part), "missing `{part}` in: {missing}");
        }
        let from_env = selected_from(Ok((SandboxKind::Seatbelt, Source::Env)), false).unwrap_err();
        assert!(from_env.contains("DARKMUX_SANDBOX"), "{from_env}");
        assert_eq!(selected_from(Err("bad value".into()), true), Err("bad value".to_string()));
    }

    /// (#3112) A message about a sandbox names the one configured; Docker's
    /// remedy belongs to Docker alone, and names the way to need no Docker
    /// only where Seatbelt can run.
    #[test]
    fn messages_name_the_configured_sandbox_and_docker_remedies_only_docker() {
        use darkmux_types::config_access::Source;
        assert_eq!(placed_in(BackendKind::Seatbelt), "the `seatbelt` sandbox (`runtime.sandbox`)");
        assert_eq!(placed_in(BackendKind::Docker), "a Docker container (`runtime.sandbox` is `docker`)");
        assert_eq!(docker_remedy_given(BackendKind::Seatbelt, Source::Config, true), None);
        let mac = docker_remedy_given(BackendKind::Docker, Source::Config, true).unwrap();
        assert!(mac.starts_with("`runtime.sandbox` is `docker`, so Docker must be installed and running"), "{mac}");
        assert!(mac.contains("darkmux config set runtime.sandbox seatbelt"), "{mac}");
        let linux = docker_remedy_given(BackendKind::Docker, Source::BuiltIn, false).unwrap();
        assert!(!linux.contains("seatbelt"), "{linux}");
        let env = docker_remedy_given(BackendKind::Docker, Source::Env, true).unwrap();
        assert!(env.starts_with("DARKMUX_SANDBOX is `docker`"), "{env}");
        assert!(env.contains("unset DARKMUX_SANDBOX"), "{env}");
    }

    /// A handle is stored by a reaper or a resume record and read back as
    /// the same handle; its backend is spelled in snake case.
    #[test]
    fn a_handle_round_trips_through_a_record() {
        let handle = SandboxHandle { backend: BackendKind::Docker, id: "darkmux-gate-1-0".into() };
        let stored = serde_json::to_string(&handle).unwrap();
        assert_eq!(stored, r#"{"backend":"docker","id":"darkmux-gate-1-0"}"#);
        assert_eq!(serde_json::from_str::<SandboxHandle>(&stored).unwrap(), handle);
    }
}
