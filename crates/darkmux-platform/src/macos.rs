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

    /// `vm_stat`'s free plus inactive pages, times the page size (`pagesize`,
    /// 16K assumed when unreadable: Apple Silicon's). The reader doctor ran
    /// before #3281, moved.
    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        reclaimable_from_vm_stat(&vm_stat()?, page_size().unwrap_or(16_384))
    }
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
