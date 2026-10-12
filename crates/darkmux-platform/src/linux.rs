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

    /// On Apple Silicon (Asahi), the chip named from the device tree the way
    /// macOS names it; otherwise `/proc/cpuinfo`'s `model name` (x86 has
    /// one; most ARM boards don't, and get `None`).
    fn cpu_brand(&self) -> Option<String> {
        read_to_string("/proc/device-tree/compatible")
            .and_then(|c| apple_chip_from_compatible(&c))
            .or_else(|| model_name_from_cpuinfo(&read_to_string("/proc/cpuinfo")?))
    }

    /// `MemTotal` rounded up to a whole GiB: the kernel reports what it can
    /// use, after firmware and its own reservations (7.36 GiB on an 8 GB M2),
    /// and that share is always under a GiB.
    fn ram_installed_bytes(&self) -> Option<u64> {
        installed_ram_from_meminfo(&read_to_string("/proc/meminfo")?)
    }
}

/// `/proc/meminfo`'s `MemTotal`, rounded up to a whole GiB.
pub(crate) fn installed_ram_from_meminfo(meminfo: &str) -> Option<u64> {
    const GIB: u64 = 1024 * 1024 * 1024;
    let kib: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((kib * 1024).div_ceil(GIB) * GIB)
}

#[cfg(target_os = "linux")]
fn read_to_string(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(not(target_os = "linux"))]
fn read_to_string(_path: &str) -> Option<String> {
    None
}

/// Apple SoCs Asahi Linux runs on, by the device tree's `apple,tNNNN`, named
/// as macOS's `machdep.cpu.brand_string` names them.
const APPLE_CHIPS: &[(&str, &str)] = &[
    ("apple,t8103", "Apple M1"),
    ("apple,t6000", "Apple M1 Pro"),
    ("apple,t6001", "Apple M1 Max"),
    ("apple,t6002", "Apple M1 Ultra"),
    ("apple,t8112", "Apple M2"),
    ("apple,t6020", "Apple M2 Pro"),
    ("apple,t6021", "Apple M2 Max"),
    ("apple,t6022", "Apple M2 Ultra"),
];

/// The chip in a device tree `compatible` list (NUL-separated). An Apple SoC
/// not in the table is `None`, never a guess.
pub(crate) fn apple_chip_from_compatible(compatible: &str) -> Option<String> {
    compatible.split('\0').find_map(|c| {
        APPLE_CHIPS
            .iter()
            .find(|(id, _)| *id == c)
            .map(|(_, name)| name.to_string())
    })
}

/// The first `model name` in `/proc/cpuinfo`.
pub(crate) fn model_name_from_cpuinfo(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        (key.trim() == "model name" && !value.trim().is_empty()).then(|| value.trim().to_string())
    })
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
