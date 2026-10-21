//! (#3200) Dispatch containers whose darkmux host died without its handlers
//! running, and the reaper `darkmux serve` runs over them.
//!
//! A caught stop (SIGINT, SIGTERM, the watchdog) kills the dispatch
//! container itself (`docker_kill_by_name`). A host that dies without its
//! handlers running (SIGKILL, an OOM kill) cannot: its `docker run` client
//! is reparented and the `darkmux-dispatch-*` container runs on, forever.
//!
//! **Identity at spawn.** Every dispatch container carries three labels
//! naming the darkmux process that started it ([`host_label_args`]): its
//! pid, that process's start time (so a reused pid is not mistaken for the
//! host), and this machine's uid (so a docker daemon another machine can
//! also reach never has this machine judge a container it did not start).
//! A host that cannot read its own start time or machine uid labels
//! nothing, and an unlabeled container is never reaped.
//!
//! **The rule** ([`orphaned_dispatch_containers`]): reap a container only
//! when its name is a dispatch container's, all three labels are present,
//! the machine label is this machine, and its host is provably gone: the
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

/// The darkmux process that starts a dispatch container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub pid: u32,
    pub start: u64,
    pub machine: String,
}

impl HostIdentity {
    /// This process, or `None` when its start time or the machine's uid
    /// cannot be read (then nothing is labeled and nothing is reaped).
    /// Read once per process: none of the three changes while it runs.
    pub fn this_process() -> Option<HostIdentity> {
        static THIS: std::sync::OnceLock<Option<HostIdentity>> = std::sync::OnceLock::new();
        THIS.get_or_init(|| {
            let pid = std::process::id();
            Some(HostIdentity {
                pid,
                start: darkmux_platform::current().process_start_stamp(pid)?,
                machine: darkmux_hardware::machine_uid()?.to_string(),
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
    ]
}

/// One running container as `docker ps` lists it, with its host labels
/// (`None` where a label is absent or unparseable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedContainer {
    pub name: String,
    pub host_pid: Option<u32>,
    pub host_start: Option<u64>,
    pub host_machine: Option<String>,
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
/// for the rule. `this_machine` `None` reaps nothing.
pub fn orphaned_dispatch_containers(
    listed: &[ListedContainer],
    this_machine: Option<&str>,
    liveness: impl Fn(u32) -> HostLiveness,
) -> Vec<Orphan> {
    let Some(this_machine) = this_machine else {
        return Vec::new();
    };
    listed
        .iter()
        .filter_map(|c| orphan_of(c, this_machine, &liveness))
        .collect()
}

/// One container's verdict: `Some` only when every guard says its host is
/// provably gone.
fn orphan_of(
    c: &ListedContainer,
    this_machine: &str,
    liveness: &impl Fn(u32) -> HostLiveness,
) -> Option<Orphan> {
    if !c.name.starts_with(DISPATCH_CONTAINER_PREFIX) {
        return None;
    }
    let (pid, start, machine) = (c.host_pid?, c.host_start?, c.host_machine.as_deref()?);
    if machine != this_machine {
        return None;
    }
    let dead = match liveness(pid) {
        HostLiveness::Gone => true,
        HostLiveness::Running { start: Some(now) } => now != start,
        HostLiveness::Running { start: None } => false,
    };
    dead.then(|| Orphan {
        name: c.name.clone(),
        host_pid: pid,
    })
}

/// The `docker ps` arguments the reaper lists dispatch containers with: one
/// line per running container, name then the three labels, tab-separated.
fn docker_ps_args() -> Vec<String> {
    vec![
        "ps".to_string(),
        "--filter".to_string(),
        format!("name=^{DISPATCH_CONTAINER_PREFIX}"),
        "--format".to_string(),
        format!(
            "{{{{.Names}}}}\t{{{{.Label \"{HOST_PID_LABEL}\"}}}}\t{{{{.Label \"{HOST_START_LABEL}\"}}}}\t{{{{.Label \"{HOST_MACHINE_LABEL}\"}}}}"
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
            ListedContainer {
                name,
                host_pid: pid.and_then(|p| p.parse().ok()),
                host_start: start.and_then(|s| s.parse().ok()),
                host_machine: machine,
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
pub fn reap_tick(this_machine: Option<&str>, liveness: impl Fn(u32) -> HostLiveness) -> ReapTick {
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
    let reaped = orphaned_dispatch_containers(&listed, this_machine, liveness)
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
