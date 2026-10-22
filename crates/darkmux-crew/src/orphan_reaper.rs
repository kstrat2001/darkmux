//! (#3200) Dispatch containers whose darkmux host died without its handlers
//! running, and the reaper `darkmux serve` runs over them.
//!
//! A caught stop (SIGINT, SIGTERM, the watchdog) kills the dispatch
//! container itself (`docker_kill_by_name`). A host that dies without its
//! handlers running (SIGKILL, an OOM kill) cannot: its `docker run` client
//! is reparented and the `darkmux-dispatch-*` container runs on until its
//! own limits (turns, tokens, the inactivity bound) end it. Nothing can
//! collect its result any more, so the reaper ends it sooner.
//!
//! **Identity at spawn.** Every dispatch container carries four labels
//! naming the darkmux process that started it ([`host_label_args`]): its
//! pid, that process's start time (so a reused pid is not mistaken for the
//! host), this machine's uid (so a docker daemon another machine can also
//! reach never has this machine judge a container it did not start), and
//! the host's pid namespace (a Linux machine-id is not unique across
//! containers and clones, #3278, and a pid means nothing outside its
//! namespace). A host that cannot read its own start time, machine uid or
//! namespace labels nothing, and an unlabeled container is never reaped.
//!
//! **The rule** ([`orphaned_dispatch_containers`]): reap a container only
//! when its name is a dispatch container's, all four labels are present,
//! the machine and namespace labels are the reaper's own (a reaper that
//! cannot read either reaps nothing), and its host is provably gone: the
//! pid has no process, or the pid's start time reads and differs from the
//! label's. A pid whose start time cannot be read is kept (fail safe). The
//! same pid-plus-start-time test as the residency lease (#2917).
//!
//! **What it does to one.** `docker kill`, nothing else: the workspace and
//! out-dir are bind mounts and survive the container.

use crate::bounded_command::{run_bounded, Bounded};
use std::process::Command;
use std::time::Duration;

/// The prefix every dispatch container's name carries
/// (`dispatch_internal::container_name_for`). Contract 4: the reaper touches
/// only containers darkmux names this way.
pub const DISPATCH_CONTAINER_PREFIX: &str = "darkmux-dispatch-";

/// The host darkmux process's pid.
pub const HOST_PID_LABEL: &str = "darkmux.dispatch.host-pid";
/// That process's start time ([`darkmux_platform::Platform::process_start_stamp`]).
pub const HOST_START_LABEL: &str = "darkmux.dispatch.host-start";
/// The machine that process ran on (`darkmux_hardware::machine_uid`).
pub const HOST_MACHINE_LABEL: &str = "darkmux.dispatch.host-machine";
/// That process's pid namespace ([`darkmux_platform::Platform::pid_namespace`]).
pub const HOST_PID_NS_LABEL: &str = "darkmux.dispatch.host-pid-ns";

/// The darkmux process that starts a dispatch container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub pid: u32,
    pub start: u64,
    pub machine: String,
    pub pid_ns: String,
}

impl HostIdentity {
    /// This process, or `None` when its start time, the machine's uid or
    /// its pid namespace cannot be read (then nothing is labeled and nothing
    /// is reaped). Read once per process: none of them changes while it runs.
    pub fn this_process() -> Option<HostIdentity> {
        static THIS: std::sync::OnceLock<Option<HostIdentity>> = std::sync::OnceLock::new();
        THIS.get_or_init(|| {
            let pid = std::process::id();
            Some(HostIdentity {
                pid,
                start: darkmux_platform::current().process_start_stamp(pid)?,
                machine: darkmux_hardware::machine_uid()?.to_string(),
                pid_ns: darkmux_platform::current().pid_namespace()?,
            })
        })
        .clone()
    }
}

/// The `docker run` options that label a container with its host.
pub fn host_label_args(host: &HostIdentity) -> Vec<String> {
    vec![
        "--label".to_string(),
        format!("{HOST_PID_LABEL}={}", host.pid),
        "--label".to_string(),
        format!("{HOST_START_LABEL}={}", host.start),
        "--label".to_string(),
        format!("{HOST_MACHINE_LABEL}={}", host.machine),
        "--label".to_string(),
        format!("{HOST_PID_NS_LABEL}={}", host.pid_ns),
    ]
}

/// Where the reaper itself runs: the machine and pid namespace a
/// container's labels must name for its host pid to mean anything here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHost {
    pub machine: String,
    pub pid_ns: String,
}

impl LocalHost {
    /// This process's machine and namespace, `None` when either is unreadable.
    pub fn this_host() -> Option<LocalHost> {
        Some(LocalHost {
            machine: darkmux_hardware::machine_uid()?.to_string(),
            pid_ns: darkmux_platform::current().pid_namespace()?,
        })
    }
}

/// One running container as `docker ps` lists it, with its host labels
/// (`None` where a label is absent or unparseable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedContainer {
    pub name: String,
    pub host_pid: Option<u32>,
    pub host_start: Option<u64>,
    pub host_machine: Option<String>,
    pub host_pid_ns: Option<String>,
}

/// What this machine knows about a host pid right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostLiveness {
    /// No process has this pid.
    Gone,
    /// A process has this pid; `start` is its start time, `None` when it
    /// could not be read.
    Running { start: Option<u64> },
}

/// A container the reaper kills, and the dead host pid it is killed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub name: String,
    pub host_pid: u32,
}

/// The dispatch containers whose host is provably dead. See the module doc
/// for the rule. `local` `None` (the reaper cannot place itself) reaps nothing.
pub fn orphaned_dispatch_containers(
    listed: &[ListedContainer],
    local: Option<&LocalHost>,
    liveness: impl Fn(u32) -> HostLiveness,
) -> Vec<Orphan> {
    let Some(local) = local else {
        return Vec::new();
    };
    listed
        .iter()
        .filter_map(|c| orphan_of(c, local, &liveness))
        .collect()
}

/// One container's verdict: `Some` only when every guard says its host is
/// provably gone.
fn orphan_of(
    c: &ListedContainer,
    local: &LocalHost,
    liveness: &impl Fn(u32) -> HostLiveness,
) -> Option<Orphan> {
    let (pid, start) = local_host_of(c, local)?;
    host_is_gone(start, liveness(pid)).then(|| Orphan {
        name: c.name.clone(),
        host_pid: pid,
    })
}

/// The container's host pid and start time, when it is a dispatch
/// container fully labeled with this reaper's own machine and namespace.
fn local_host_of(c: &ListedContainer, local: &LocalHost) -> Option<(u32, u64)> {
    if !c.name.starts_with(DISPATCH_CONTAINER_PREFIX) {
        return None;
    }
    let (pid, start, machine) = (c.host_pid?, c.host_start?, c.host_machine.as_deref()?);
    let pid_ns = c.host_pid_ns.as_deref()?;
    (machine == local.machine && pid_ns == local.pid_ns).then_some((pid, start))
}

/// Gone, or a different process now on the pid. An unreadable start time
/// is not proof, so it keeps.
fn host_is_gone(labeled_start: u64, now: HostLiveness) -> bool {
    match now {
        HostLiveness::Gone => true,
        HostLiveness::Running { start: Some(now) } => now != labeled_start,
        HostLiveness::Running { start: None } => false,
    }
}

/// The `docker ps` arguments the reaper lists dispatch containers with: one
/// line per running container, name then the four labels, tab-separated.
fn docker_ps_args() -> Vec<String> {
    vec![
        "ps".to_string(),
        "--filter".to_string(),
        format!("name=^{DISPATCH_CONTAINER_PREFIX}"),
        "--format".to_string(),
        format!(
            "{{{{.Names}}}}\t{{{{.Label \"{HOST_PID_LABEL}\"}}}}\t{{{{.Label \"{HOST_START_LABEL}\"}}}}\t{{{{.Label \"{HOST_MACHINE_LABEL}\"}}}}\t{{{{.Label \"{HOST_PID_NS_LABEL}\"}}}}"
        ),
    ]
}

/// Parse [`docker_ps_args`]'s output. An empty label renders as an empty
/// field and reads as absent.
pub fn parse_docker_ps(stdout: &str) -> Vec<ListedContainer> {
    let field = |s: Option<&str>| {
        s.map(str::trim)
            .filter(|s| !s.is_empty() && *s != "<no value>")
            .map(str::to_string)
    };
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next().unwrap_or_default().trim().to_string();
            let pid = field(parts.next());
            let start = field(parts.next());
            let machine = field(parts.next());
            let pid_ns = field(parts.next());
            ListedContainer {
                name,
                host_pid: pid.and_then(|p| p.parse().ok()),
                host_start: start.and_then(|s| s.parse().ok()),
                host_machine: machine,
                host_pid_ns: pid_ns,
            }
        })
        .collect()
}

/// This machine's answer about `pid`: `kill(pid, 0)` for existence (only
/// `ESRCH` is gone), then the platform's start-time read.
pub fn host_liveness(pid: u32) -> HostLiveness {
    if !darkmux_types::residency_lease::process_alive(pid) {
        return HostLiveness::Gone;
    }
    HostLiveness::Running {
        start: darkmux_platform::current().process_start_stamp(pid),
    }
}

/// How long one `docker ps` or `docker kill` may take before the tick gives
/// up on it. A wedged Docker Desktop must not wedge the reaper thread.
const DOCKER_CALL_BOUND: Duration = Duration::from_secs(10);

/// One reaper pass.
#[derive(Debug)]
pub enum ReapTick {
    /// `docker ps` answered; each orphan found, with whether its kill
    /// succeeded.
    Listed { reaped: Vec<(Orphan, bool)> },
    /// `docker ps` did not answer (no docker, a stopped daemon, a timeout).
    DockerUnavailable,
}

/// List the dispatch containers, decide, and `docker kill` each orphan.
pub fn reap_tick(local: Option<&LocalHost>, liveness: impl Fn(u32) -> HostLiveness) -> ReapTick {
    let mut ps = Command::new("docker");
    ps.args(docker_ps_args());
    let stdout = match run_bounded(ps, DOCKER_CALL_BOUND) {
        Bounded::Finished {
            success: true,
            stdout,
            ..
        } => stdout,
        _ => return ReapTick::DockerUnavailable,
    };
    let listed = parse_docker_ps(&String::from_utf8_lossy(&stdout));
    let reaped = orphaned_dispatch_containers(&listed, local, liveness)
        .into_iter()
        .map(|orphan| {
            let mut kill = Command::new("docker");
            kill.args(["kill", &orphan.name]);
            let ok = matches!(
                run_bounded(kill, DOCKER_CALL_BOUND),
                Bounded::Finished { success: true, .. }
            );
            (orphan, ok)
        })
        .collect();
    ReapTick::Listed { reaped }
}

#[cfg(test)]
#[path = "orphan_reaper_tests.rs"]
mod tests;
