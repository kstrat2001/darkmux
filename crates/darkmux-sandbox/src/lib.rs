//! (#3112) Isolation as a seam: what a sandbox may see and do
//! ([`SandboxPolicy`]), the identity of one that runs ([`SandboxHandle`]),
//! and the backend that turns a policy into a process ([`SandboxBackend`]).
//!
//! Three backends: Docker ([`DockerBackend`]), macOS Seatbelt
//! ([`SeatbeltBackend`]) and Linux Landlock ([`LandlockBackend`]), picked by
//! the `runtime.sandbox` setting ([`selection`]). Callers build a policy and spawn the command a backend
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
pub mod landlock;
pub use landlock::LandlockBackend;
pub mod toolchain;
pub mod host_leaders;
pub use host_leaders::host_sandbox_launchers;

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

/// Resource limits. `None` leaves the backend's own. Docker holds both in
/// the container's cgroup; Seatbelt holds `pids` with the user's
/// `RLIMIT_NPROC` and watches `memory` ([`seatbelt`]'s doc says how far
/// each reaches); Landlock holds neither ([`landlock`]'s doc).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limits {
    /// Memory, in Docker's size syntax (`4g`: a whole number, then `b`,
    /// `k`, `m` or `g`). A backend that holds memory refuses a value that
    /// is not one.
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
    /// Ports the host reaches on its loopback, at the same number. A
    /// container publishes them there and on no other address; a sandbox on
    /// the host ([`BackendKind::on_host`]) publishes nothing and its servers
    /// bind host ports directly, on whatever address they ask for (#3559).
    pub ports: Vec<u16>,
    /// Whether the sandbox reads the caller's stdin (a secret piped in);
    /// otherwise it gets none.
    pub stdin: bool,
    /// The ports of this host's own services the sandbox may connect to (a
    /// dispatch's model server). Under Seatbelt it reaches no other service
    /// listening on this machine, at any of its addresses, beyond its own
    /// `ports` (with no ports named: none listening when it is planned, so
    /// its own servers on any other port stay reachable). Landlock does not
    /// hold it. A container reaches the host by the engine's name for it
    /// (`host.docker.internal`), which Docker Desktop forwards to this
    /// host's loopback whatever this says.
    pub host_services: Vec<u16>,
    /// Ports of this host that a sandbox on the host naming no `ports` is
    /// refused whether or not anything listens on them when it is planned
    /// (darkmux's own services, which may start after it), unless given in
    /// `host_services`. A policy naming ports is refused every port of this
    /// machine but its own and the given ones already.
    pub refused_ports: Vec<u16>,
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

/// (#3112) `text` as a sandbox memory limit in bytes, or the refusal
/// naming it: the grammar `sandbox.memory`'s preflight reads
/// ([`darkmux_types::size::sandbox_memory_bytes`]), so a value one backend
/// takes, every backend takes, and none reads `0` as no limit.
pub fn memory_bytes(text: &str) -> Result<u64, String> {
    darkmux_types::size::sandbox_memory_bytes(text)
        .map_err(|why| format!("sandbox: memory limit refused: {why}; the form is {}", darkmux_types::size::SANDBOX_MEMORY_FORM))
}

/// A command that fails (125, Docker's "could not make it"), saying
/// `message`: what a backend hands back for a policy it refuses.
pub(crate) fn failing(message: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(r#"echo "$1" >&2; exit 125"#).arg("sh").arg(message);
    cmd
}

/// Which backend a handle belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Docker,
    /// (#3112 step 2, spike) macOS `sandbox-exec`.
    Seatbelt,
    /// (#3112 step 2) Linux namespaces and Landlock.
    Landlock,
}

impl BackendKind {
    /// Whether this backend's sandbox is a process on this host itself
    /// (Seatbelt, Landlock), not a container: it shares the host's loopback,
    /// toolchain and user, and has no image. Ask this, not which backend it
    /// is, wherever that is the question.
    pub fn on_host(self) -> bool {
        match self {
            BackendKind::Docker => false,
            BackendKind::Seatbelt | BackendKind::Landlock => true,
        }
    }
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

/// One read of `runtime.sandbox`: the backend it names and the tier that set
/// it. A caller that reads the setting once per unit of work and carries
/// this value gives every later message (why Docker, the way back) from that
/// same read, so the unit's parts agree by construction. No method of it
/// reads the setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub kind: BackendKind,
    pub source: darkmux_types::config_access::Source,
}

/// The one read of `runtime.sandbox` this crate offers: the backend it
/// names and the tier that set it, or why it names none this host can use
/// (an unregistered value, refused, never defaulted, contract 9; `seatbelt`
/// where `sandbox-exec` is missing; `landlock` where the kernel lacks what
/// it needs, saying what). Every other function here takes the value.
pub fn selection() -> Result<Selection, String> {
    let resolved = darkmux_types::config_access::sandbox_with_source().map_err(|bad| bad.to_string());
    let source = resolved.as_ref().map_or(darkmux_types::config_access::Source::BuiltIn, |(_, s)| *s);
    selected_from(resolved, SeatbeltBackend::available(), LandlockBackend::available()).map(|kind| Selection { kind, source })
}

impl Selection {
    /// (#3112) For a message saying Docker could not run something: why
    /// Docker was needed (the setting that chose it) and, where a sandbox on
    /// the host can run (Seatbelt, Landlock), how to need no Docker. `None`
    /// unless this selection is Docker: a Docker remedy never appears under
    /// another.
    pub fn docker_remedy(&self) -> Option<String> {
        docker_remedy_given(self.kind, self.source, host_alternative())
    }

    /// [`Selection::docker_remedy`] without naming the setting, for a
    /// message whose [`placed_in`] already does.
    pub fn docker_needs(&self) -> Option<String> {
        docker_needs_given(self.kind, self.source, host_alternative())
    }

    /// The way back from this selection to Docker
    /// ([`way_back_to_docker_given`]).
    pub fn way_back_to_docker(&self) -> String {
        way_back_to_docker_given(setting_value(self.kind), self.source)
    }
}

/// [`selection`] against `cfg`, a `config.json` read fresh (the env tier
/// still live), for a process that reads its config per request.
pub fn selection_in(cfg: &darkmux_types::config::DarkmuxConfig) -> Result<Selection, String> {
    let resolved = darkmux_types::config_access::sandbox_with_source_in(cfg).map_err(|bad| bad.to_string());
    let source = resolved.as_ref().map_or(darkmux_types::config_access::Source::BuiltIn, |(_, s)| *s);
    selected_from(resolved, SeatbeltBackend::available(), LandlockBackend::available()).map(|kind| Selection { kind, source })
}

fn selected_from(
    resolved: Result<(darkmux_types::config::SandboxKind, darkmux_types::config_access::Source), String>,
    seatbelt_available: bool,
    landlock_available: Result<(), String>,
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
            unset_env(source),
        )),
        (SandboxKind::Landlock, source) => landlock_available.map(|()| BackendKind::Landlock).map_err(|why| {
            format!(
                "{} is `landlock`, but {why}; to run in Docker instead, `darkmux config set {SANDBOX_KEY} docker`{}",
                setting_named(source),
                unset_env(source),
            )
        }),
    }
}

/// " and unset DARKMUX_SANDBOX" when the env var decided: a `config set`
/// under it changes nothing.
fn unset_env(source: darkmux_types::config_access::Source) -> String {
    if source == darkmux_types::config_access::Source::Env {
        format!(" and unset {SANDBOX_ENV}")
    } else {
        String::new()
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
        BackendKind::Landlock => darkmux_types::config::SandboxKind::Landlock.as_str(),
    }
}

/// (#3112) Where isolated work runs under `kind`, for a message: "a
/// Docker container (`runtime.sandbox` is `docker`)", or "the `seatbelt`
/// sandbox (`runtime.sandbox`)" (and `landlock` alike).
pub fn placed_in(kind: BackendKind) -> String {
    match kind {
        BackendKind::Docker => format!("a Docker container (`{SANDBOX_KEY}` is `{}`)", setting_value(kind)),
        BackendKind::Seatbelt | BackendKind::Landlock => format!("the `{}` sandbox (`{SANDBOX_KEY}`)", setting_value(kind)),
    }
}

/// (#3112 review) The way back from `current` (a sandbox on the host, by its
/// setting value) to Docker, naming what decided the sandbox (`source`):
/// the env var when it did (a `config set` under it changes nothing), else
/// `config set`.
pub fn way_back_to_docker_given(current: &str, source: darkmux_types::config_access::Source) -> String {
    let docker = setting_value(BackendKind::Docker);
    match source {
        darkmux_types::config_access::Source::Env => format!(
            "{SANDBOX_ENV} is `{current}`; to run in a Docker container instead, `export {SANDBOX_ENV}={docker}`, \
             or unset {SANDBOX_ENV} and `darkmux config set {SANDBOX_KEY} {docker}`"
        ),
        _ => format!("`{SANDBOX_KEY}` is `{current}`; to run in a Docker container instead, `darkmux config set {SANDBOX_KEY} {docker}`"),
    }
}

/// The sandbox on the host this host can run, if any: Seatbelt on a Mac,
/// Landlock on a Linux whose kernel has what it needs.
fn host_alternative() -> Option<BackendKind> {
    if SeatbeltBackend::available() {
        Some(BackendKind::Seatbelt)
    } else {
        LandlockBackend::available().ok().map(|()| BackendKind::Landlock)
    }
}

fn docker_remedy_given(kind: BackendKind, source: darkmux_types::config_access::Source, on_host: Option<BackendKind>) -> Option<String> {
    let needs = docker_needs_given(kind, source, on_host)?;
    let setting = if source == darkmux_types::config_access::Source::Env { SANDBOX_ENV.to_string() } else { format!("`{SANDBOX_KEY}`") };
    Some(format!("{setting} is `{}`, so {needs}", setting_value(kind)))
}

fn docker_needs_given(kind: BackendKind, source: darkmux_types::config_access::Source, on_host: Option<BackendKind>) -> Option<String> {
    if kind != BackendKind::Docker {
        return None;
    }
    let mut out = "Docker must be installed and running".to_string();
    if let Some(alternative) = on_host {
        let here = if alternative == BackendKind::Seatbelt { "on this Mac" } else { "here" };
        out.push_str(&format!("; to need no Docker {here}, `darkmux config set {SANDBOX_KEY} {}`", setting_value(alternative)));
        out.push_str(&unset_env(source));
    }
    Some(out)
}

/// The backend of `kind`: the one a caller's [`selection`] names, for a new
/// sandbox, or the one a sandbox was already made in.
pub fn backend_of(kind: BackendKind) -> &'static dyn SandboxBackend {
    match kind {
        BackendKind::Docker => &DockerBackend,
        BackendKind::Seatbelt => &SeatbeltBackend,
        BackendKind::Landlock => &LandlockBackend,
    }
}

/// The backend that made `handle`'s sandbox: stop, kill and remove it
/// there, whatever `runtime.sandbox` says now.
pub fn for_handle(handle: &SandboxHandle) -> &'static dyn SandboxBackend {
    backend_of(handle.backend)
}

/// (#3112 review) Where a sandbox of `kind` sees host path `host` mounted
/// at `at`: what a brief, a prompt or a note must name for the model to find
/// it. `at` in a container (byte for byte what it always was); the host
/// path itself, canonical, in a sandbox on the host ([`BackendKind::on_host`]),
/// which cannot place a mount elsewhere. `None` (a refused selection, which
/// runs nothing) is `at`.
pub fn seen_at(kind: Option<BackendKind>, host: &std::path::Path, at: &str) -> String {
    match kind {
        Some(kind) => backend_of(kind).place(&Mount { host: host.to_path_buf(), at: at.to_string(), access: Access::ReadOnly }),
        None => at.to_string(),
    }
}

/// Every backend this host can run (Docker when `docker` is on `PATH`,
/// Seatbelt where `sandbox-exec` is, Landlock where the kernel has what it
/// needs), for a reaper: a sandbox left from before the setting (or the
/// default) changed is still found.
pub fn available_backends() -> Vec<&'static dyn SandboxBackend> {
    available_given(DockerBackend::on_path(), SeatbeltBackend::available(), LandlockBackend::available().is_ok())
}

fn available_given(docker: bool, seatbelt: bool, landlock: bool) -> Vec<&'static dyn SandboxBackend> {
    let mut out: Vec<&'static dyn SandboxBackend> = Vec::new();
    if docker {
        out.push(&DockerBackend);
    }
    if seatbelt {
        out.push(&SeatbeltBackend);
    }
    if landlock {
        out.push(&LandlockBackend);
    }
    out
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

    /// (#3561) The `PATH` a sandbox of this backend runs with when its
    /// policy names none: a sandbox on the host's own (the system's
    /// programs, and Homebrew's on a Mac), which a caller puts a managed
    /// toolchain in front of; `None` for a container, which keeps its
    /// image's.
    fn default_path(&self) -> Option<&'static str> {
        None
    }

    /// (#3112) Whether a sandbox of this backend reads host path `path`
    /// with no mount: the OS and its system toolchain, for a sandbox on the
    /// host. A container sees only its image and mounts: `false`. A caller
    /// that grants a host program read-only (a preview's toolchain) grants
    /// only what this says is not already readable.
    fn host_reads(&self, path: &std::path::Path) -> bool {
        let _ = path;
        false
    }

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
mod setting_reads;

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
        let fits = || Ok(());
        assert_eq!(selected_from(Ok((SandboxKind::Docker, Source::BuiltIn)), false, fits()), Ok(BackendKind::Docker));
        assert_eq!(selected_from(Ok((SandboxKind::Seatbelt, Source::Config)), true, fits()), Ok(BackendKind::Seatbelt));
        let missing = selected_from(Ok((SandboxKind::Seatbelt, Source::Config)), false, fits()).unwrap_err();
        for part in ["runtime.sandbox", "seatbelt", seatbelt::SANDBOX_EXEC, "darkmux config set runtime.sandbox docker"] {
            assert!(missing.contains(part), "missing `{part}` in: {missing}");
        }
        let from_env = selected_from(Ok((SandboxKind::Seatbelt, Source::Env)), false, fits()).unwrap_err();
        assert!(from_env.contains("DARKMUX_SANDBOX"), "{from_env}");
        assert_eq!(selected_from(Err("bad value".into()), true, fits()), Err("bad value".to_string()));
        // `landlock` where the kernel has what it needs; refused, saying
        // what is missing and the way to Docker, where it does not.
        assert_eq!(selected_from(Ok((SandboxKind::Landlock, Source::BuiltIn)), false, fits()), Ok(BackendKind::Landlock));
        let short = selected_from(Ok((SandboxKind::Landlock, Source::Env)), true, Err("this kernel's Landlock ABI is 4".into())).unwrap_err();
        for part in ["DARKMUX_SANDBOX", "landlock", "ABI is 4", "darkmux config set runtime.sandbox docker", "unset DARKMUX_SANDBOX"] {
            assert!(short.contains(part), "missing `{part}` in: {short}");
        }
    }

    /// (#3112) A message about a sandbox names the one configured; Docker's
    /// remedy belongs to Docker alone, and names the way to need no Docker
    /// only where Seatbelt can run.
    #[test]
    fn messages_name_the_configured_sandbox_and_docker_remedies_only_docker() {
        use darkmux_types::config_access::Source;
        assert_eq!(placed_in(BackendKind::Seatbelt), "the `seatbelt` sandbox (`runtime.sandbox`)");
        assert_eq!(placed_in(BackendKind::Docker), "a Docker container (`runtime.sandbox` is `docker`)");
        assert_eq!(docker_remedy_given(BackendKind::Seatbelt, Source::Config, Some(BackendKind::Seatbelt)), None);
        let mac = docker_remedy_given(BackendKind::Docker, Source::Config, Some(BackendKind::Seatbelt)).unwrap();
        assert!(mac.starts_with("`runtime.sandbox` is `docker`, so Docker must be installed and running"), "{mac}");
        assert!(mac.contains("darkmux config set runtime.sandbox seatbelt"), "{mac}");
        let no_alternative = docker_remedy_given(BackendKind::Docker, Source::BuiltIn, None).unwrap();
        assert!(!no_alternative.contains("seatbelt") && !no_alternative.contains("landlock"), "{no_alternative}");
        let linux = docker_remedy_given(BackendKind::Docker, Source::Config, Some(BackendKind::Landlock)).unwrap();
        assert!(linux.contains("to need no Docker here, `darkmux config set runtime.sandbox landlock`"), "{linux}");
        assert_eq!(placed_in(BackendKind::Landlock), "the `landlock` sandbox (`runtime.sandbox`)");
        assert_eq!(docker_needs_given(BackendKind::Landlock, Source::Config, Some(BackendKind::Landlock)), None);
        let env = docker_remedy_given(BackendKind::Docker, Source::Env, Some(BackendKind::Seatbelt)).unwrap();
        assert!(env.starts_with("DARKMUX_SANDBOX is `docker`"), "{env}");
        assert!(env.contains("unset DARKMUX_SANDBOX"), "{env}");
        // (#3112 review) Beside a phrase that already names the setting
        // (`placed_in`), the remedy does not name it again.
        let needs = docker_needs_given(BackendKind::Docker, Source::Config, Some(BackendKind::Seatbelt)).unwrap();
        assert!(needs.starts_with("Docker must be installed and running;") && !needs.contains("is `docker`"), "{needs}");
        assert_eq!(docker_needs_given(BackendKind::Seatbelt, Source::Config, Some(BackendKind::Seatbelt)), None);
    }

    /// (#3112 review) The way back to Docker names what decided the
    /// sandbox: a `config set` when `config.json` (or the default) did; the
    /// env var when it did, since a `config set` under it changes nothing.
    #[test]
    fn the_way_back_to_docker_names_what_decided() {
        use darkmux_types::config_access::Source;
        let config = way_back_to_docker_given("seatbelt", Source::Config);
        assert!(config.starts_with("`runtime.sandbox` is `seatbelt`") && config.contains("darkmux config set runtime.sandbox docker"), "{config}");
        assert_eq!(way_back_to_docker_given("seatbelt", Source::BuiltIn), config);
        let landlock = way_back_to_docker_given("landlock", Source::BuiltIn);
        assert!(landlock.starts_with("`runtime.sandbox` is `landlock`"), "{landlock}");
        let env = way_back_to_docker_given("seatbelt", Source::Env);
        assert!(env.starts_with("DARKMUX_SANDBOX is `seatbelt`") && env.contains("DARKMUX_SANDBOX=docker"), "{env}");
        assert!(env.contains("unset DARKMUX_SANDBOX"), "{env}");
    }

    /// (#3112 review) Starting a sandbox goes through `selection()`, which
    /// carries a refused setting as an error: no caller can get Docker by
    /// accident. An existing sandbox is reached through its own handle's
    /// backend, whatever is configured now.
    #[test]
    #[serial_test::serial]
    fn a_refused_selection_gives_no_backend_and_a_handle_keeps_its_own() {
        let prev = std::env::var_os(SANDBOX_ENV);
        unsafe { std::env::set_var(SANDBOX_ENV, "podman") };
        let refused = selection().err();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(SANDBOX_ENV, v),
                None => std::env::remove_var(SANDBOX_ENV),
            }
        }
        let why = refused.expect("a refused selection is an error, never Docker");
        assert!(why.contains("podman") && why.contains("runtime.sandbox"), "{why}");
        assert_eq!(backend_of(BackendKind::Seatbelt).kind(), BackendKind::Seatbelt);
        assert_eq!(backend_of(BackendKind::Docker).kind(), BackendKind::Docker);
        assert_eq!(backend_of(BackendKind::Landlock).kind(), BackendKind::Landlock);
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: "x".into() };
        assert_eq!(for_handle(&handle).kind(), BackendKind::Seatbelt);
    }


    /// (#3112 review) A reaper lists every backend this host can run, so a
    /// sandbox left from before the default changed is still found.
    #[test]
    fn every_runnable_backend_is_listed() {
        let kinds = |d, s, l| available_given(d, s, l).iter().map(|b| b.kind()).collect::<Vec<_>>();
        assert_eq!(kinds(true, true, false), vec![BackendKind::Docker, BackendKind::Seatbelt]);
        assert_eq!(kinds(false, true, false), vec![BackendKind::Seatbelt]);
        assert_eq!(kinds(true, false, false), vec![BackendKind::Docker]);
        assert_eq!(kinds(true, false, true), vec![BackendKind::Docker, BackendKind::Landlock]);
        assert!(kinds(false, false, false).is_empty());
    }

    /// (#3112 review) Every Mac this suite runs on (CI's macOS job
    /// included) has `sandbox-exec`, so the tests that return early without
    /// it really run their Seatbelt branch there.
    #[test]
    fn every_mac_this_suite_runs_on_has_sandbox_exec() {
        if darkmux_platform::current().id() == "macos" {
            assert!(SeatbeltBackend::available(), "{} is missing on this Mac", seatbelt::SANDBOX_EXEC);
        }
    }

    /// (#3112 review) What a model is told a path is: where the sandbox
    /// sees the mount. `at` in a container, byte for byte; the host path
    /// itself (canonical) in a sandbox on the host; `at` when the selection
    /// is refused (nothing runs then).
    #[test]
    fn a_path_is_named_where_the_sandbox_sees_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().canonicalize().unwrap().display().to_string();
        assert_eq!(seen_at(Some(BackendKind::Docker), dir.path(), "/workspace"), "/workspace");
        assert_eq!(seen_at(Some(BackendKind::Seatbelt), dir.path(), "/workspace"), real);
        assert_eq!(seen_at(Some(BackendKind::Landlock), dir.path(), "/workspace"), real);
        assert_eq!(seen_at(None, dir.path(), "/workspace"), "/workspace");
    }

    /// (#3112) A backend whose sandbox is a process on this host (Seatbelt,
    /// Landlock) is `on_host`; Docker's container is not.
    #[test]
    fn a_host_process_backend_is_on_host_and_docker_is_not() {
        assert!(BackendKind::Seatbelt.on_host() && BackendKind::Landlock.on_host());
        assert!(!BackendKind::Docker.on_host());
    }

    /// (#3561) A sandbox on the host names the PATH it runs with when its
    /// policy names none, which a managed toolchain goes in front of; a
    /// container keeps its image's own.
    #[test]
    fn a_host_sandbox_names_its_own_path_and_a_container_none() {
        assert_eq!(SeatbeltBackend.default_path(), Some(seatbelt::DEFAULT_PATH));
        assert_eq!(LandlockBackend.default_path(), Some(landlock::DEFAULT_PATH));
        assert_eq!(DockerBackend.default_path(), None);
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
