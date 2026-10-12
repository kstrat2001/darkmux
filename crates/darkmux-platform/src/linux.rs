//! Linux (first target: Omarchy on Apple Silicon, under Asahi).

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct LinuxPlatform;

impl Platform for LinuxPlatform {
    fn id(&self) -> &'static str {
        "linux"
    }

    /// Docker Engine doesn't define `host.docker.internal`, so it is mapped
    /// to the host gateway. Docker Desktop for Linux, which does define it,
    /// accepts the same mapping, so no engine detection is needed.
    fn container_host_access(&self) -> ContainerHostAccess {
        ContainerHostAccess::HostGateway
    }

    /// Docker Engine keeps the host's ownership on bind mounts, and darkmux
    /// creates the workspace and out dirs private (0700) to the host user, so
    /// the container runs as that user. The runtime image's own user (uid
    /// 1000) only matches by chance. With no readable `/proc` (this module
    /// compiled for another OS, in tests) the answer is the image's user.
    ///
    /// Assumes rootful Docker Engine. Under rootless Docker, rootless Podman
    /// or Docker Desktop for Linux, the container's uid 0 is the host user, so
    /// `runtime.container_user = "0:0"` is the setting there (#3276 review).
    fn container_user(&self) -> ContainerUser {
        host_ids()
            .map(|(uid, gid)| ContainerUser::Host {
                uid,
                gid: Some(gid),
            })
            .unwrap_or(ContainerUser::Image)
    }

    /// The install's `/etc/machine-id`. A dual-boot Mac's macOS side has a
    /// different identity (its `IOPlatformUUID`): one install, one uid.
    fn machine_uid(&self) -> Option<String> {
        machine_uid_from_disk()
    }
}

/// `/etc/machine-id`: systemd's id for this install, stable across reboots
/// and hostname changes, new on a reinstall. Readable by any user.
pub(crate) fn machine_uid_from_disk() -> Option<String> {
    read_machine_id()
}

#[cfg(target_os = "linux")]
fn read_machine_id() -> Option<String> {
    parse_machine_id(&std::fs::read_to_string("/etc/machine-id").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn read_machine_id() -> Option<String> {
    None
}

/// A machine id is 32 lowercase hex characters (`machine-id(5)`); anything
/// else (empty, `uninitialized` during first boot) is no identity.
pub(crate) fn parse_machine_id(contents: &str) -> Option<String> {
    let id = contents.trim();
    let valid = id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| id.to_string())
}

#[cfg(target_os = "linux")]
fn host_ids() -> Option<(u32, u32)> {
    ids_from_status(&std::fs::read_to_string("/proc/self/status").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn host_ids() -> Option<(u32, u32)> {
    None
}

/// The effective uid and gid from `/proc/<pid>/status` (`Uid:` and `Gid:`
/// list real, effective, saved and filesystem ids; the second is effective).
/// Compiled everywhere and tested everywhere; only Linux calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn ids_from_status(status: &str) -> Option<(u32, u32)> {
    let effective = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some((effective("Uid:")?, effective("Gid:")?))
}
