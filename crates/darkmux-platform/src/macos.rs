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
        let version = command_stdout("sw_vers", &["-productVersion"])?.trim().to_string();
        if version.is_empty() {
            return None;
        }
        let history = command_stdout("system_profiler", &["SPInstallHistoryDataType", "-json"]).unwrap_or_default();
        Some(crate::OsRelease { installed_at_unix_ms: macos_installed_at(&history, &version), name: format!("macOS {version}") })
    }

    /// Apple Silicon runs local models on Metal.
    fn gpu_api(&self) -> Option<&'static str> {
        self.cpu_brand().filter(|b| b.starts_with("Apple M")).map(|_| "Metal")
    }

    fn secret_store(&self) -> crate::SecretStore {
        crate::SecretStore::Keychain
    }
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
    for line in ioreg_output.lines() {
        // Match the quoted key so a hypothetical sibling like
        // `"IOPlatformUUIDExtra"` can't grab the value.
        if line.contains("\"IOPlatformUUID\"") {
            if let Some(eq) = line.find('=') {
                let uuid = line[eq + 1..].trim().trim_matches('"').trim();
                if !uuid.is_empty() {
                    return Some(uuid.to_string());
                }
            }
        }
    }
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
