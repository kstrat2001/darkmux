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

    /// The install's machine-id (`/etc/machine-id`, else D-Bus's copy). A dual-boot Mac's macOS side has a
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
    /// use, after firmware and its own reservations (7.36 GiB on an 8 GB M2).
    /// Display only. Close to the size as sold where the reserved share is
    /// under a GiB; lower where firmware sets aside more (an integrated GPU's
    /// carve-out, a large crashkernel).
    fn ram_installed_bytes(&self) -> Option<u64> {
        installed_ram_from_meminfo(&read_to_string("/proc/meminfo")?)
    }

    /// `cpufreq` policies grouped into clusters by `cpu_capacity`.
    fn cpu_clusters(&self) -> Vec<crate::CpuClusterInfo> {
        clusters_from_policies(read_policies())
    }

    /// `/proc/stat`'s per-CPU lines.
    fn cpu_ticks(&self) -> Option<Vec<crate::CpuTicks>> {
        cpu_ticks_from_stat(&read_to_string("/proc/stat")?)
    }

    /// `/proc/meminfo`.
    fn memory_snapshot(&self) -> Option<crate::MemorySnapshot> {
        memory_from_meminfo(&read_to_string("/proc/meminfo")?)
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

/// The machine-id (`/etc/machine-id`, else `/var/lib/dbus/machine-id`): the id for this install, stable across reboots
/// and hostname changes, new on a reinstall. Readable by any user.
pub(crate) fn machine_uid_from_disk() -> Option<String> {
    read_machine_id()
}

#[cfg(target_os = "linux")]
fn read_machine_id() -> Option<String> {
    first_machine_id(&MACHINE_ID_PATHS)
}

/// systemd's machine-id, then D-Bus's copy, which distributions without
/// systemd keep (machine-id(5)).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// The first of `paths` holding a valid machine-id.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn first_machine_id<P: AsRef<std::path::Path>>(paths: &[P]) -> Option<String> {
    paths.iter().find_map(|p| parse_machine_id(&std::fs::read_to_string(p).ok()?))
}

#[cfg(not(target_os = "linux"))]
fn read_machine_id() -> Option<String> {
    None
}

/// A machine id is 32 lowercase hex characters (`machine-id(5)`); anything
/// else (empty, `uninitialized` during first boot) is no identity.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// Per-CPU ticks from `/proc/stat` (`cpuN user nice system idle iowait irq
/// softirq ...`, in USER_HZ). `system` folds in irq and softirq and `idle`
/// folds in iowait, so busy = user + nice + system the way mach counts it.
/// The aggregate `cpu` line is skipped. `None` with no per-CPU line.
pub(crate) fn cpu_ticks_from_stat(stat: &str) -> Option<Vec<crate::CpuTicks>> {
    let ticks: Vec<crate::CpuTicks> = stat
        .lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .filter_map(|l| {
            let f: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .filter_map(|v| v.parse().ok())
                .collect();
            let at = |i: usize| f.get(i).copied().unwrap_or(0);
            (f.len() >= 4).then(|| crate::CpuTicks {
                user: at(0),
                nice: at(1),
                system: at(2) + at(5) + at(6),
                idle: at(3) + at(4),
            })
        })
        .collect();
    (!ticks.is_empty()).then_some(ticks)
}

/// Each `cpufreq` policy's CPUs and their `cpu_capacity`.
#[cfg(target_os = "linux")]
fn read_policies() -> Vec<(Vec<usize>, Option<u64>)> {
    let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu/cpufreq") else {
        return Vec::new();
    };
    let mut policies: Vec<(Vec<usize>, Option<u64>)> = dir
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("policy"))
        .filter_map(|e| {
            let cpus: Vec<usize> = std::fs::read_to_string(e.path().join("related_cpus"))
                .ok()?
                .split_whitespace()
                .filter_map(|c| c.parse().ok())
                .collect();
            let first = *cpus.first()?;
            let capacity =
                std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{first}/cpu_capacity"))
                    .ok()
                    .and_then(|c| c.trim().parse().ok());
            Some((cpus, capacity))
        })
        .collect();
    policies.sort_by_key(|(cpus, _)| cpus.first().copied());
    policies
}

#[cfg(not(target_os = "linux"))]
fn read_policies() -> Vec<(Vec<usize>, Option<u64>)> {
    Vec::new()
}

/// Clusters from `(cpus, capacity)` policies, fastest first. With more than
/// one capacity the highest is "Performance" and the rest "Efficiency"; one
/// kind is "CPU".
pub(crate) fn clusters_from_policies(
    mut policies: Vec<(Vec<usize>, Option<u64>)>,
) -> Vec<crate::CpuClusterInfo> {
    policies.sort_by(|a, b| b.1.cmp(&a.1));
    let top = policies.first().and_then(|p| p.1);
    let mixed = policies.iter().any(|p| p.1 != top);
    policies
        .into_iter()
        .map(|(cpus, capacity)| crate::CpuClusterInfo {
            name: match (mixed, capacity == top) {
                (false, _) => "CPU",
                (true, true) => "Performance",
                (true, false) => "Efficiency",
            }
            .to_string(),
            logical_cpus: cpus.len(),
        })
        .collect()
}

/// A memory snapshot from `/proc/meminfo`; `None` without `MemTotal`.
pub(crate) fn memory_from_meminfo(meminfo: &str) -> Option<crate::MemorySnapshot> {
    let kib = |key: &str| -> Option<u64> {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|k| k * 1024)
    };
    Some(crate::MemorySnapshot {
        capacity_bytes: kib("MemTotal:")?,
        free_bytes: kib("MemFree:"),
        available_bytes: kib("MemAvailable:"),
        swap_used_bytes: kib("SwapTotal:")
            .zip(kib("SwapFree:"))
            .map(|(t, f)| t.saturating_sub(f)),
    })
}
