//! macOS: the platform darkmux was built on.

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct MacPlatform;

impl Platform for MacPlatform {
    fn id(&self) -> &'static str {
        "macos"
    }

    /// Docker Desktop defines `host.docker.internal`.
    fn container_host_access(&self) -> ContainerHostAccess {
        ContainerHostAccess::Builtin
    }

    /// Docker Desktop on macOS maps ownership across the mounts, so the image's own
    /// user can use them.
    fn container_user(&self) -> ContainerUser {
        ContainerUser::Image
    }

    /// The `IOPlatformUUID` (`ioreg -rd1 -c IOPlatformExpertDevice`).
    /// Survives renames, hostname changes and OS reinstalls; only a
    /// logic-board swap resets it.
    fn machine_uid(&self) -> Option<String> {
        parse_io_platform_uuid(&ioreg_platform_expert()?)
    }

    /// The `IOPlatformUUID` and `IOPlatformSerialNumber`, the names `scutil`
    /// knows the Mac by (`ComputerName`, `LocalHostName`, `HostName`), the
    /// kernel's host name (`kern.hostname`, which a network may assign) and
    /// the user's full name (`id -F`).
    fn identifying_strings(&self) -> Vec<String> {
        let expert = ioreg_platform_expert().unwrap_or_default();
        let mut out: Vec<String> = ["IOPlatformUUID", "IOPlatformSerialNumber"]
            .iter()
            .filter_map(|k| parse_io_platform_value(&expert, k))
            .collect();
        out.extend(["ComputerName", "LocalHostName", "HostName"].iter().filter_map(|k| scutil_get(k)));
        out.extend([run_trimmed("sysctl", &["-n", "kern.hostname"]), run_trimmed("id", &["-F"])].into_iter().flatten());
        out
    }

    /// `sysctl machdep.cpu.brand_string`.
    fn cpu_brand(&self) -> Option<String> {
        parse_brand_string(&sysctl_brand_string()?)
    }

    /// `sysctl hw.memsize`: the installed memory, exactly.
    fn ram_installed_bytes(&self) -> Option<u64> {
        parse_memsize(&sysctl_memsize()?)
    }

    /// Read through darkmux-crew's mach probe (`host_probe::mach_cpu`) until
    /// that FFI moves here; answering nothing keeps a Mac's lens unchanged.
    fn cpu_clusters(&self) -> Vec<crate::CpuClusterInfo> {
        Vec::new()
    }

    /// See [`Self::cpu_clusters`].
    fn cpu_ticks(&self) -> Option<Vec<crate::CpuTicks>> {
        None
    }

    /// See [`Self::cpu_clusters`]: macOS's memory figures still come from
    /// its own `vm_stat` and mach readers.
    fn memory_snapshot(&self) -> Option<crate::MemorySnapshot> {
        None
    }

    /// Read through darkmux-crew's IOKit battery probe until that moves here.
    fn battery(&self) -> Option<crate::BatteryReading> {
        None
    }

    /// IOReport fills a Mac's cluster frequencies in darkmux-crew's probe.
    fn cpu_cluster_mhz(&self) -> Vec<Option<u32>> {
        Vec::new()
    }

    /// IOReport reads a Mac's power, by rail, in darkmux-crew's probe.
    fn package_power_mw(&self) -> Option<f64> {
        None
    }

    /// A Mac's thermal reading is its pressure state (darkmux-crew's
    /// `thermal` probe); its board sensors are not read.
    fn board_sensors(&self) -> Option<crate::BoardSensors> {
        None
    }

    /// `vm_stat`'s free plus inactive pages, times the page size (`pagesize`,
    /// 16K assumed when unreadable: Apple Silicon's). The reader doctor ran
    /// before #3281, moved.
    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        reclaimable_from_vm_stat(&vm_stat()?, page_size().unwrap_or(16_384))
    }
    /// `sysctl(CTL_KERN, KERN_PROC, KERN_PROC_PID, pid)`'s
    /// `kinfo_proc.kp_proc.p_starttime`, in microseconds since the epoch.
    /// Works for a process owned by ANY user, which is the point: a reused
    /// pid after a reboot lands on launchd (pid 1) or a root daemon. (#2917
    /// re-review M-A: `proc_pidinfo(PROC_PIDTBSDINFO)` answers nothing for
    /// another user's process.)
    fn process_start_stamp(&self, pid: u32) -> Option<u64> {
        kinfo_start_stamp(pid)
    }

    /// macOS has one pid namespace per machine.
    fn pid_namespace(&self) -> Option<String> {
        Some("macos".to_string())
    }

    /// `sw_vers -productVersion`, installed when `system_profiler`'s install
    /// history last lists `macOS <version>`. The system files' own times are
    /// Apple's build times on the sealed volume, not when an update landed.
    fn os_release(&self) -> Option<crate::OsRelease> {
        let version = command_stdout("sw_vers", &["-productVersion"])?;
        macos_release(&version, || command_stdout("system_profiler", &["SPInstallHistoryDataType", "-json"]).unwrap_or_default())
    }

    /// Apple Silicon runs local models on Metal.
    fn gpu_api(&self) -> Option<&'static str> {
        self.cpu_brand().filter(|b| b.starts_with("Apple M")).map(|_| "Metal")
    }

    /// `hw.perflevelN.name` / `.physicalcpu` (level 0 is the fastest),
    /// `hw.model`, and the GPU's `gpu-core-count` from the AGX accelerator
    /// in the IORegistry. None of them needs root.
    fn machine_config(&self) -> crate::MachineConfig {
        // Separate calls: sysctl exits non-zero when any one key is unknown.
        let model = command_stdout("sysctl", &["hw.model"]).unwrap_or_default();
        let head = command_stdout("sysctl", &["hw.nperflevels"]).unwrap_or_default();
        let levels = sysctl_value(&head, "hw.nperflevels").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(8);
        let keys: Vec<String> =
            (0..levels).flat_map(|i| [format!("hw.perflevel{i}.name"), format!("hw.perflevel{i}.physicalcpu")]).collect();
        let per_level = if levels == 0 {
            String::new()
        } else {
            command_stdout("sysctl", &keys.iter().map(String::as_str).collect::<Vec<_>>()).unwrap_or_default()
        };
        crate::MachineConfig {
            cpu_clusters: perf_levels_from_sysctl(&per_level, levels),
            gpu_cores: command_stdout("ioreg", &["-rc", "AGXAccelerator", "-d1"]).as_deref().and_then(gpu_cores_from_ioreg),
            model_id: sysctl_value(&model, "hw.model").map(str::to_string),
        }
    }

    fn secret_store(&self) -> crate::SecretStore {
        crate::SecretStore::Keychain
    }

    /// `security -i` reads the command from stdin, so the secret is never on
    /// an argv. It can exit 0 when the command failed: the caller reads back.
    fn store_secret(&self, item: &str, account: &str, secret: &str) -> Result<(), String> {
        security_interactive(&format!("add-generic-password -U -a \"{account}\" -s {item} -w \"{secret}\"\n"))
    }

    fn forget_secret(&self, item: &str, account: &str) {
        let _ = command_stdout("security", &["delete-generic-password", "-a", account, "-s", item]);
    }
}

/// Runs one `security -i` command line, fed on stdin.
#[cfg(target_os = "macos")]
fn security_interactive(line: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running `security`: {e}"))?;
    child.stdin.take().expect("piped").write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn security_interactive(_line: &str) -> Result<(), String> {
    Err("there is no login keychain on this machine".to_string())
}

#[cfg(target_os = "macos")]
fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn command_stdout(_cmd: &str, _args: &[&str]) -> Option<String> {
    None
}

/// `macOS <version>` from `sw_vers`' answer, installed when the install
/// history (read only for a version there is) says.
pub(crate) fn macos_release(version: &str, history: impl FnOnce() -> String) -> Option<crate::OsRelease> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }
    Some(crate::OsRelease { installed_at_unix_ms: macos_installed_at(&history(), version), name: format!("macOS {version}") })
}

/// The newest `install_date` among `system_profiler SPInstallHistoryDataType
/// -json` items named `macOS <version>`. Read by scanning each item's two
/// string fields; this crate parses no JSON otherwise.
pub(crate) fn macos_installed_at(json: &str, version: &str) -> Option<u64> {
    let want = format!("macOS {version}");
    json.split('{')
        .filter(|item| string_field(item, "_name").as_deref() == Some(want.as_str()))
        .filter_map(|item| crate::dates::rfc3339_utc_ms(&string_field(item, "install_date")?))
        .max()
}

/// `"key" : "value"` in a fragment of JSON, the value unescaped of nothing (dates and names here have no escapes).
fn string_field(item: &str, key: &str) -> Option<String> {
    let at = item.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = item[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// (#2917, moved here by #3200) See [`MacPlatform::process_start_stamp`].
#[cfg(target_os = "macos")]
fn kinfo_start_stamp(pid: u32) -> Option<u64> {
    // `struct kinfo_proc` begins with `struct extern_proc kp_proc`, whose
    // first member is the union `p_un`; `p_starttime` is that union's
    // `struct timeval` arm, so it sits at offset 0 of the whole record:
    // `tv_sec` (i64) at 0, `tv_usec` (i32 on Darwin) at 8. The libc crate
    // does not declare `kinfo_proc` for Apple targets, so the buffer is
    // sized by the kernel's own size probe and read at those offsets.
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut len: libc::size_t = 0;
    // SAFETY: a null destination with a valid `len` pointer is sysctl's
    // documented "how big is it" probe.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 || len < 16 {
        return None;
    }
    // u64 words so the i64 at offset 0 is aligned.
    let mut buf = vec![0u64; len.div_ceil(8)];
    let mut got = buf.len() * 8;
    // SAFETY: `buf` is at least `len` bytes (rounded up to whole words) and
    // `got` says exactly that; the return value and filled length are
    // checked before anything is read.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, buf.as_mut_ptr().cast(), &mut got, std::ptr::null_mut(), 0)
    };
    // A pid with no process answers rc 0 with nothing filled in.
    if rc != 0 || got < 16 {
        return None;
    }
    let head: Vec<u8> = buf[..2].iter().flat_map(|w| w.to_ne_bytes()).collect();
    let sec = i64::from_ne_bytes(head[0..8].try_into().ok()?);
    let usec = i32::from_ne_bytes(head[8..12].try_into().ok()?);
    if sec <= 0 || usec < 0 {
        return None;
    }
    Some((sec as u64).saturating_mul(1_000_000).saturating_add(usec as u64))
}

#[cfg(not(target_os = "macos"))]
fn kinfo_start_stamp(_pid: u32) -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn sysctl_memsize() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_memsize() -> Option<String> {
    None
}

/// The value of `key` in `sysctl <key>...` output (`key: value` lines).
pub(crate) fn sysctl_value<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).map(str::trim).filter(|v| !v.is_empty())
}

/// `(name, physical cores)` per perf level, level 0 first. When any level
/// reads no name or no cores, nothing: part of a machine is not its config.
pub(crate) fn perf_levels_from_sysctl(output: &str, levels: usize) -> Vec<(String, u32)> {
    (0..levels)
        .map(|i| {
            let name = sysctl_value(output, &format!("hw.perflevel{i}.name"))?;
            let cores = sysctl_value(output, &format!("hw.perflevel{i}.physicalcpu"))?.parse::<u32>().ok()?;
            (cores > 0).then(|| (name.to_string(), cores))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// `"gpu-core-count" = 40` in `ioreg -rc AGXAccelerator -d1` output.
pub(crate) fn gpu_cores_from_ioreg(output: &str) -> Option<u32> {
    output.lines().find_map(|l| l.trim().strip_prefix("\"gpu-core-count\" = ")?.trim().parse().ok()).filter(|n| *n > 0)
}

/// `sysctl -n hw.memsize` output: installed bytes.
pub(crate) fn parse_memsize(output: &str) -> Option<u64> {
    output.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn sysctl_brand_string() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_brand_string() -> Option<String> {
    None
}

/// `sysctl -n machdep.cpu.brand_string` output, trimmed; empty is `None`.
pub(crate) fn parse_brand_string(output: &str) -> Option<String> {
    let s = output.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(target_os = "macos")]
fn ioreg_platform_expert() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn ioreg_platform_expert() -> Option<String> {
    None
}

/// Pull the `IOPlatformUUID` value out of `ioreg -rd1 -c
/// IOPlatformExpertDevice` output (a line shaped like
/// `    "IOPlatformUUID" = "XXXXXXXX-...."`). Pure — testable without
/// shelling out.
pub(crate) fn parse_io_platform_uuid(ioreg_output: &str) -> Option<String> {
    parse_io_platform_value(ioreg_output, "IOPlatformUUID")
}

/// A string property of that output by its key. The quoted key is matched,
/// so a sibling like `"IOPlatformUUIDExtra"` can't grab the value.
pub(crate) fn parse_io_platform_value(ioreg_output: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let line = ioreg_output.lines().find(|l| l.contains(&quoted))?;
    let eq = line.find('=')?;
    crate::trimmed(line[eq + 1..].trim().trim_matches('"'))
}

fn scutil_get(key: &str) -> Option<String> {
    run_trimmed("scutil", &["--get", key])
}

/// A command's output, trimmed; nothing when it fails or says nothing.
#[cfg(target_os = "macos")]
fn run_trimmed(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    crate::trimmed(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn run_trimmed(_program: &str, _args: &[&str]) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn vm_stat() -> Option<String> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn vm_stat() -> Option<String> {
    None
}

/// The page size: 16K on Apple Silicon, 4K on Intel.
#[cfg(target_os = "macos")]
fn page_size() -> Option<u64> {
    let out = std::process::Command::new("pagesize").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(target_os = "macos"))]
fn page_size() -> Option<u64> {
    None
}

/// Free plus inactive pages from `vm_stat` output, in bytes. A field that is
/// present but unparseable is no figure at all.
pub(crate) fn reclaimable_from_vm_stat(vm_stat: &str, page_size: u64) -> Option<u64> {
    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    for line in vm_stat.lines() {
        if let Some(rest) = line.strip_prefix("Pages free:") {
            free_pages = parse_pages_field(rest)?;
        } else if let Some(rest) = line.strip_prefix("Pages inactive:") {
            inactive_pages = parse_pages_field(rest)?;
        }
    }
    Some((free_pages + inactive_pages).saturating_mul(page_size))
}

/// A `vm_stat` page count (`"   1234567."`), separators dropped.
pub(crate) fn parse_pages_field(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    cleaned.parse().ok()
}
