//! (#3112) Isolation as a seam: what a sandbox may see and do
//! ([`SandboxPolicy`]), the identity of one that runs ([`SandboxHandle`]),
//! and the backend that turns a policy into a process ([`SandboxBackend`]).
//!
//! Docker is the only backend ([`DockerBackend`]). Callers build a policy and
//! spawn the command a backend gives them; they never spell a container
//! engine's command line. Bounding a run (deadline, interrupt, process-group
//! kill) stays with the caller's runner, so a backend only plans processes.
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
}

/// A running (or once-running) sandbox, as a record can store it: which
/// backend made it, and its id there (a container's name for Docker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub backend: BackendKind,
    pub id: String,
}

/// One sandbox a listing found carrying a label, and that label's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labeled {
    pub handle: SandboxHandle,
    pub value: String,
}

/// A way to run a policy. Each method plans a process for the caller to
/// spawn and bound; none runs anything itself.
pub trait SandboxBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    /// The command that runs `policy` attached until it exits, and the
    /// handle naming the sandbox. Killing that command's process need not
    /// stop the sandbox: a caller whose run was cut short [`remove`]s it.
    ///
    /// [`remove`]: SandboxBackend::remove
    fn run(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle);

    /// The command that stops and removes `handle`'s sandbox, its whole
    /// process tree with it. Removing one already gone succeeds or fails
    /// harmlessly; either way it is gone.
    fn remove(&self, handle: &SandboxHandle) -> Command;

    /// The command that lists every sandbox carrying a `key` label, running
    /// or not; [`parse_labeled`] reads its output.
    ///
    /// [`parse_labeled`]: SandboxBackend::parse_labeled
    fn list_labeled(&self, key: &str) -> Command;

    /// The sandboxes in [`list_labeled`]'s output, each with the label's
    /// value as listed.
    ///
    /// [`list_labeled`]: SandboxBackend::list_labeled
    fn parse_labeled(&self, listing: &str) -> Vec<Labeled>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
