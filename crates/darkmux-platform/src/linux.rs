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

    /// The machine-id, the kernel's host name and, on Apple Silicon, the
    /// serial number the device tree carries.
    fn identifying_strings(&self) -> Vec<String> {
        let read = ["/proc/sys/kernel/hostname", "/proc/device-tree/serial-number"].map(read_to_string);
        machine_uid_from_disk().into_iter().chain(read.into_iter().flatten().filter_map(|s| crate::trimmed(&s))).collect()
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

    /// `cpufreq` policies grouped into tiers by `cpu_capacity` (see
    /// [`tiers_from_policies`]); one "CPU" tier of every online CPU where
    /// there is no `cpufreq` (most VMs and containers).
    fn cpu_clusters(&self) -> Vec<crate::CpuClusterInfo> {
        tiers_from_policies(read_policies_with_freq(), &online_cpus()).into_iter().map(|(info, _)| info).collect()
    }

    /// `/proc/stat`'s per-CPU lines.
    fn cpu_ticks(&self) -> Option<Vec<crate::CpuTicks>> {
        cpu_ticks_from_stat(&read_to_string("/proc/stat")?)
    }

    /// `/proc/meminfo`.
    fn memory_snapshot(&self) -> Option<crate::MemorySnapshot> {
        memory_from_meminfo(&read_to_string("/proc/meminfo")?)
    }

    /// `/sys/class/power_supply`: the Battery supply, and whether a Mains
    /// supply is online.
    fn battery(&self) -> Option<crate::BatteryReading> {
        read_battery()
    }

    /// Each tier's mean `scaling_cur_freq`, aligned with [`Self::cpu_clusters`].
    fn cpu_cluster_mhz(&self) -> Vec<Option<u32>> {
        tiers_from_policies(read_policies_with_freq(), &online_cpus()).into_iter().map(|(_, mhz)| mhz).collect()
    }

    /// The SMC's `Heatpipe Power` sensor (hwmon, Asahi's `macsmc_hwmon`):
    /// it follows SoC load, ~2.6 W idle to ~20 W with every CPU busy on an
    /// M2 (#3331), and there is no CPU, GPU or ANE split beside it.
    fn package_power_mw(&self) -> Option<f64> {
        package_mw_from_hwmon(&read_hwmon())
    }

    /// Every labeled hwmon temperature, and the fastest fan.
    fn board_sensors(&self) -> Option<crate::BoardSensors> {
        board_sensors_from_hwmon(&read_hwmon())
    }

    /// `/proc/<pid>/stat` field 22: the start time in clock ticks since
    /// boot. World-readable unless `/proc` is mounted with `hidepid`.
    fn process_start_stamp(&self, pid: u32) -> Option<u64> {
        start_ticks_from_proc_stat(&read_to_string(&format!("/proc/{pid}/stat"))?)
    }

    /// The inode of `/proc/self/ns/pid` (its link reads `pid:[4026531836]`).
    /// Two processes see each other's pids only when this matches.
    fn pid_namespace(&self) -> Option<String> {
        pid_namespace_from_link(&read_link("/proc/self/ns/pid")?)
    }

    /// `/proc/meminfo`'s `MemAvailable`.
    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        reclaimable_from_meminfo(&read_to_string("/proc/meminfo")?)
    }

    /// `/etc/os-release`'s `PRETTY_NAME`, named Omarchy where Omarchy is
    /// installed (`~/.local/share/omarchy`). A rolling release says no
    /// install time, so none is claimed.
    fn os_release(&self) -> Option<crate::OsRelease> {
        let omarchy = std::env::var_os("HOME")
            .is_some_and(|h| std::path::Path::new(&h).join(".local/share/omarchy").is_dir());
        let name = os_name_from_release(&read_to_string("/etc/os-release")?, omarchy)?;
        Some(crate::OsRelease { name, installed_at_unix_ms: None })
    }

    /// Asahi's Apple GPU driver serves Vulkan.
    fn gpu_api(&self) -> Option<&'static str> {
        read_to_string("/proc/device-tree/compatible").and_then(|c| apple_chip_from_compatible(&c)).map(|_| "Vulkan")
    }

    /// The CPU tiers [`Platform::cpu_clusters`] reads, one core per online
    /// CPU (Apple Silicon has no SMT; an offlined core is not counted), on
    /// Apple Silicon only. The GPU core count
    /// and the Mac's model identifier have no source read here yet.
    fn machine_config(&self) -> crate::MachineConfig {
        if self.gpu_api().is_none() {
            return crate::MachineConfig::default();
        }
        crate::MachineConfig {
            cpu_clusters: self.cpu_clusters().into_iter().map(|c| (c.name, c.logical_cpus as u32)).collect(),
            ..Default::default()
        }
    }

    fn secret_store(&self) -> crate::SecretStore {
        crate::SecretStore::PrivateFile
    }

    /// `MemTotal` is under the size sold (firmware's share): on the Apple
    /// chips Asahi runs on, every size sold is a multiple of 8 GB.
    fn ram_as_sold_gb(&self) -> Option<u64> {
        let gib = self.ram_installed_bytes()?.div_ceil(1 << 30);
        Some(if self.gpu_api().is_some() { sold_size_on_apple(gib) } else { gib })
    }
}

/// An M1 or M2 sold with 8, 16, 24, 32, 64, 96 or 128 GB: the next multiple of 8.
pub(crate) fn sold_size_on_apple(gib: u64) -> u64 {
    gib.div_ceil(8) * 8
}

/// `PRETTY_NAME` from an os-release file, as `Omarchy (<it>)` on Omarchy.
pub(crate) fn os_name_from_release(os_release: &str, omarchy: bool) -> Option<String> {
    let pretty = os_release.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty());
    match (omarchy, pretty) {
        (true, p) => Some(format!("Omarchy ({})", p.as_deref().unwrap_or("Linux"))),
        (false, p) => p,
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

/// `/proc/<pid>/stat`'s field 22 (start time). `comm` (field 2) is
/// parenthesized and may contain spaces; fields after the LAST `)` are
/// space-separated, starting at field 3.
pub(crate) fn start_ticks_from_proc_stat(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// `pid:[4026531836]` -> `pid:4026531836`; anything else is `None`.
pub(crate) fn pid_namespace_from_link(link: &str) -> Option<String> {
    let inode = link.trim().strip_prefix("pid:[")?.strip_suffix(']')?;
    (!inode.is_empty() && inode.bytes().all(|b| b.is_ascii_digit())).then(|| format!("pid:{inode}"))
}

#[cfg(target_os = "linux")]
fn read_link(path: &str) -> Option<String> {
    std::fs::read_link(path).ok().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn read_link(_path: &str) -> Option<String> {
    None
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
            // `affected_cpus` lists the policy's ONLINE CPUs: `/proc/stat` has a
            // line for each online CPU only, so an offline one must not count.
            let cpus: Vec<usize> = std::fs::read_to_string(e.path().join("affected_cpus"))
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


/// Tiers from `(online cpus, capacity, current kHz)` policies, the shape
/// macOS's `hw.perflevelN` reports: policies of equal capacity are one tier
/// (two P-clusters on an M1 Pro, or the one-policy-per-CPU of x86's
/// `intel_pstate`), its MHz the mean of its policies'. Fastest first; with
/// more than one capacity the highest is "Performance" and the rest
/// "Efficiency", one capacity is "CPU". Each tier names its CPUs' positions
/// in `online` (the CPU numbers `/proc/stat` lists, ascending), so a tier
/// whose CPUs are not one block (an M1 Ultra's two dies) still reads its own.
/// With no policies at all (no `cpufreq`), one "CPU" tier of every online
/// CPU, its MHz unknown.
pub(crate) fn tiers_from_policies(
    policies: Vec<(Vec<usize>, Option<u64>, Option<u64>)>,
    online: &[usize],
) -> Vec<(crate::CpuClusterInfo, Option<u32>)> {
    let info = |name: &str, tick_positions: Vec<usize>| crate::CpuClusterInfo {
        name: name.to_string(),
        logical_cpus: tick_positions.len(),
        tick_positions,
    };
    if policies.is_empty() {
        return if online.is_empty() { Vec::new() } else { vec![(info("CPU", (0..online.len()).collect()), None)] };
    }
    // capacity -> (cpus, kHz readings)
    let mut tiers: Vec<(Option<u64>, Vec<usize>, Vec<u64>)> = Vec::new();
    for (cpus, capacity, khz) in policies {
        let i = tiers.iter().position(|t| t.0 == capacity).unwrap_or_else(|| {
            tiers.push((capacity, Vec::new(), Vec::new()));
            tiers.len() - 1
        });
        tiers[i].1.extend(cpus);
        tiers[i].2.extend(khz);
    }
    tiers.retain(|t| !t.1.is_empty());
    tiers.sort_by_key(|t| std::cmp::Reverse(t.0));
    let top = tiers.first().and_then(|t| t.0);
    let mixed = tiers.len() > 1;
    tiers
        .into_iter()
        .map(|(capacity, mut cpus, khz)| {
            let name = match (mixed, capacity.is_some() && capacity == top) {
                (false, _) => "CPU",
                (true, true) => "Performance",
                (true, false) => "Efficiency",
            };
            cpus.sort_unstable();
            let positions = cpus.iter().filter_map(|c| online.iter().position(|o| o == c)).collect();
            let mhz = (!khz.is_empty()).then(|| (khz.iter().sum::<u64>() / khz.len() as u64 / 1000) as u32);
            (info(name, positions), mhz)
        })
        // A tier whose CPUs all left `/proc/stat` (a hotplug race) reads
        // nothing; drop it, as macOS drops an empty perf level.
        .filter(|(i, _)| i.logical_cpus > 0)
        .collect()
}

/// The CPU numbers `/proc/stat` lists (the online ones), in its order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn online_cpus_from_stat(stat: &str) -> Vec<usize> {
    stat.lines()
        .filter_map(|l| {
            // `cpuN ...`; the aggregate `cpu  ...` line has no number.
            let rest = l.strip_prefix("cpu").filter(|r| r.as_bytes().first().is_some_and(u8::is_ascii_digit))?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .collect()
}

/// This machine's online CPU numbers; empty when unreadable.
#[cfg(target_os = "linux")]
fn online_cpus() -> Vec<usize> {
    read_to_string("/proc/stat").map_or_else(Vec::new, |s| online_cpus_from_stat(&s))
}

#[cfg(not(target_os = "linux"))]
fn online_cpus() -> Vec<usize> {
    Vec::new()
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

/// The first present Battery supply, and whether any Mains supply is online.
#[cfg(target_os = "linux")]
fn read_battery() -> Option<crate::BatteryReading> {
    let supplies: Vec<Vec<(String, String)>> = std::fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .map(|e| {
            ["type", "present", "online", "capacity", "status", "current_now", "time_to_empty_now"]
                .iter()
                .filter_map(|k| {
                    let v = std::fs::read_to_string(e.path().join(k)).ok()?;
                    Some((k.to_string(), v.trim().to_string()))
                })
                .collect()
        })
        .collect();
    let field = |s: &Vec<(String, String)>, k: &str| s.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let on_ac = supplies
        .iter()
        .any(|s| field(s, "type").as_deref() == Some("Mains") && field(s, "online").as_deref() == Some("1"));
    supplies.iter().find_map(|s| {
        let pairs: Vec<(&str, &str)> = s.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        battery_from_supply(&pairs, on_ac)
    })
}

#[cfg(not(target_os = "linux"))]
fn read_battery() -> Option<crate::BatteryReading> {
    None
}

/// One power supply's sysfs fields to a battery reading: `None` unless it is
/// a present Battery with a charge. `status` is `Charging`, `Discharging`,
/// `Full` or `Not charging` (on AC, held below full); `current_now` is in
/// microamps; `time_to_empty_now` in seconds, 0 meaning no estimate.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn battery_from_supply(fields: &[(&str, &str)], on_ac: bool) -> Option<crate::BatteryReading> {
    let get = |k: &str| fields.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
    if get("type") != Some("Battery") || get("present") == Some("0") {
        return None;
    }
    let charge_pct = get("capacity")?.parse::<u8>().ok()?.min(100);
    let status = get("status");
    Some(crate::BatteryReading {
        charge_pct,
        on_ac,
        charging: status == Some("Charging"),
        fully_charged: status.map(|s| s == "Full"),
        current_ma: get("current_now").and_then(|v| v.parse::<i64>().ok()).map(|ua| ua / 1000),
        minutes_to_empty: get("time_to_empty_now")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|s| *s > 0)
            .map(|s| s / 60),
    })
}

/// [`read_policies`] plus each policy's current frequency (kHz).
#[cfg(target_os = "linux")]
fn read_policies_with_freq() -> Vec<(Vec<usize>, Option<u64>, Option<u64>)> {
    read_policies()
        .into_iter()
        .map(|(cpus, capacity)| {
            let khz = cpus.first().and_then(|c| {
                let path = format!("/sys/devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq");
                std::fs::read_to_string(path).ok()?.trim().parse().ok()
            });
            (cpus, capacity, khz)
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn read_policies_with_freq() -> Vec<(Vec<usize>, Option<u64>, Option<u64>)> {
    Vec::new()
}


/// `MemAvailable` from `/proc/meminfo`: the kernel's estimate of what can be
/// handed out without swapping, reclaimable cache included. `None` when the
/// kernel doesn't report it; `MemFree` would understate it, so no stand-in.
pub(crate) fn reclaimable_from_meminfo(meminfo: &str) -> Option<u64> {
    memory_from_meminfo(meminfo)?.available_bytes
}

/// One hwmon input: the chip's `name`, the input's stem (`power4`), its
/// `_label` when it has one, and the raw `_input` value in hwmon's units
/// (microwatts, millidegrees Celsius, rpm).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HwmonInput {
    pub chip: String,
    pub sensor: String,
    pub label: Option<String>,
    pub value: i64,
}

/// The label Apple's SMC gives the sensor that follows SoC power. Matched by
/// label, never by index: the index is the device tree's ordering.
const PACKAGE_POWER_LABEL: &str = "Heatpipe Power";

/// [`PACKAGE_POWER_LABEL`]'s reading in milliwatts. A negative reading is
/// not a draw, so it is no reading.
pub(crate) fn package_mw_from_hwmon(inputs: &[HwmonInput]) -> Option<f64> {
    inputs
        .iter()
        .find(|i| i.sensor.starts_with("power") && i.label.as_deref() == Some(PACKAGE_POWER_LABEL))
        .filter(|i| i.value >= 0)
        .map(|i| i.value as f64 / 1000.0)
}

/// Labeled temperatures (to 0.1 °C) and the fastest fan. A label that only
/// names the kind (`temp`) says nothing about where the sensor sits, so it
/// is skipped; so is an unlabeled one. A label two chips share is keyed
/// `chip label`. `None` when there is neither a temperature nor a fan.
pub(crate) fn board_sensors_from_hwmon(inputs: &[HwmonInput]) -> Option<crate::BoardSensors> {
    let temps: Vec<(&HwmonInput, &str)> = inputs
        .iter()
        .filter(|i| i.sensor.starts_with("temp"))
        .filter_map(|i| Some((i, i.label.as_deref().map(str::trim).filter(|l| !l.is_empty() && *l != "temp")?)))
        .collect();
    let mut temps_c = std::collections::BTreeMap::new();
    for (i, label) in &temps {
        let shared = temps.iter().filter(|(_, l)| l == label).count() > 1;
        let key = if shared { format!("{} {label}", i.chip) } else { label.to_string() };
        temps_c.insert(key, (i.value as f64 / 100.0).round() / 10.0);
    }
    let fan_rpm = inputs
        .iter()
        .filter(|i| i.sensor.starts_with("fan") && i.value >= 0)
        .filter_map(|i| u32::try_from(i.value).ok())
        .max();
    (!temps_c.is_empty() || fan_rpm.is_some()).then_some(crate::BoardSensors { temps_c, fan_rpm })
}

/// Every `*_input` under `/sys/class/hwmon`. An input that can't be read or
/// parsed is left out; that is what an absent sensor reads as.
#[cfg(target_os = "linux")]
fn read_hwmon() -> Vec<HwmonInput> {
    let Ok(chips) = std::fs::read_dir("/sys/class/hwmon") else { return Vec::new() };
    let mut out = Vec::new();
    for chip in chips.flatten() {
        let dir = chip.path();
        let Some(name) = std::fs::read_to_string(dir.join("name")).ok().map(|n| n.trim().to_string()) else { continue };
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for f in files.flatten() {
            let file = f.file_name().to_string_lossy().into_owned();
            let Some(sensor) = file.strip_suffix("_input") else { continue };
            let Some(value) = std::fs::read_to_string(f.path()).ok().and_then(|v| v.trim().parse().ok()) else { continue };
            let label = std::fs::read_to_string(dir.join(format!("{sensor}_label"))).ok().map(|l| l.trim().to_string());
            out.push(HwmonInput { chip: name.clone(), sensor: sensor.to_string(), label, value });
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn read_hwmon() -> Vec<HwmonInput> {
    Vec::new()
}
