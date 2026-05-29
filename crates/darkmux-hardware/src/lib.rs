//! Detect local hardware spec for heuristics-provider matching.
//!
//! Each `HeuristicsProvider` declares which hardware shapes it claims via
//! `matches(&HardwareSpec)`. The first provider that matches wins. The
//! `generic` provider matches anything as a fallback.
//!
//! v0.x ships providers for Apple Silicon at two RAM tiers (128GB and 64GB).
//! Other platforms get the generic provider, which prints an
//! "unvalidated platform" warning rather than pretending to know the right
//! answer.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Platform {
    AppleSilicon,
    MacIntel,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn label(&self) -> &'static str {
        match self {
            Platform::AppleSilicon => "Apple Silicon",
            Platform::MacIntel => "Intel Mac",
            Platform::Linux => "Linux",
            Platform::Windows => "Windows",
            Platform::Other => "(unknown platform)",
        }
    }
}

/// Coarse RAM tiers used by provider matching. The boundaries roughly map
/// to common Mac configurations (16/32/64/96/128/256 GB) but are platform-
/// agnostic enough to apply to non-Mac systems too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum RamTier {
    /// < 32 GB — most consumer Macs / commodity laptops.
    Small,
    /// 32-64 GB — popular MBP Pro tier.
    Medium,
    /// 64-96 GB — high-end Pro / mid Studios.
    Large,
    /// 96+ GB — M-series Max / Studio Ultra.
    Xl,
}

impl RamTier {
    pub fn label(&self) -> &'static str {
        match self {
            RamTier::Small => "small (<32 GB)",
            RamTier::Medium => "medium (32-64 GB)",
            RamTier::Large => "large (64-96 GB)",
            RamTier::Xl => "XL (96+ GB)",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HardwareSpec {
    pub platform: Platform,
    pub arch: String,
    pub total_ram_gb: u32,
    pub physical_cores: u32,
    pub performance_cores: Option<u32>,
    pub efficiency_cores: Option<u32>,
    /// `true` on systems where CPU and GPU share a single memory pool
    /// (Apple Silicon's hallmark — affects whether compactor-offload
    /// patterns make sense).
    pub has_unified_memory: bool,
}

impl HardwareSpec {
    pub fn ram_tier(&self) -> RamTier {
        match self.total_ram_gb {
            0..=32 => RamTier::Small,
            33..=64 => RamTier::Medium,
            65..=96 => RamTier::Large,
            _ => RamTier::Xl,
        }
    }

    pub fn one_line_summary(&self) -> String {
        let p_label = self.platform.label();
        let cores_label = match (self.performance_cores, self.efficiency_cores) {
            (Some(p), Some(e)) => format!(" ({}P+{}E)", p, e),
            _ => String::new(),
        };
        format!(
            "{p_label} {arch}, {ram} GB RAM ({tier}), {cores} cores{detail}{unified}",
            arch = self.arch,
            ram = self.total_ram_gb,
            tier = self.ram_tier().label(),
            cores = self.physical_cores,
            detail = cores_label,
            unified = if self.has_unified_memory { ", unified memory" } else { "" }
        )
    }
}

pub fn detect() -> HardwareSpec {
    let arch = std::env::consts::ARCH.to_string();
    let os = std::env::consts::OS;
    let platform = match (os, arch.as_str()) {
        ("macos", "aarch64") => Platform::AppleSilicon,
        ("macos", "x86_64") => Platform::MacIntel,
        ("linux", _) => Platform::Linux,
        ("windows", _) => Platform::Windows,
        _ => Platform::Other,
    };
    let total_ram_gb = read_ram_gb(platform).unwrap_or(0);
    let physical_cores = read_physical_cores(platform).unwrap_or(0);
    let (performance_cores, efficiency_cores) = read_perf_efficiency_cores(platform);
    let has_unified_memory = matches!(platform, Platform::AppleSilicon);
    HardwareSpec {
        platform,
        arch,
        total_ram_gb,
        physical_cores,
        performance_cores,
        efficiency_cores,
        has_unified_memory,
    }
}

fn read_ram_gb(platform: Platform) -> Option<u32> {
    match platform {
        Platform::AppleSilicon | Platform::MacIntel => sysctl_u64("hw.memsize")
            .map(|bytes| (bytes / (1024 * 1024 * 1024)) as u32),
        Platform::Linux => read_linux_meminfo_gb(),
        _ => None,
    }
}

fn read_physical_cores(platform: Platform) -> Option<u32> {
    match platform {
        Platform::AppleSilicon | Platform::MacIntel => sysctl_u32("hw.physicalcpu")
            .or_else(|| sysctl_u32("hw.ncpu")),
        Platform::Linux => {
            // /proc/cpuinfo lists every logical CPU; counting "processor"
            // gives logical-core count which is good enough for our purposes.
            std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .map(|t| {
                    t.lines()
                        .filter(|line| line.starts_with("processor"))
                        .count() as u32
                })
        }
        _ => None,
    }
}

fn read_perf_efficiency_cores(platform: Platform) -> (Option<u32>, Option<u32>) {
    if !matches!(platform, Platform::AppleSilicon) {
        return (None, None);
    }
    let p = sysctl_u32("hw.perflevel0.physicalcpu");
    let e = sysctl_u32("hw.perflevel1.physicalcpu");
    (p, e)
}

fn sysctl_u64(key: &str) -> Option<u64> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn sysctl_u32(key: &str) -> Option<u32> {
    sysctl_u64(key).and_then(|n| u32::try_from(n).ok())
}

fn read_linux_meminfo_gb() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // "MemTotal:      131072000 kB"
            let kb: u64 = rest
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            return Some((kb / (1024 * 1024)) as u32);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_tier_boundaries() {
        let mut hw = HardwareSpec {
            platform: Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 0,
            physical_cores: 0,
            performance_cores: None,
            efficiency_cores: None,
            has_unified_memory: true,
        };
        hw.total_ram_gb = 16;
        assert_eq!(hw.ram_tier(), RamTier::Small);
        hw.total_ram_gb = 32;
        assert_eq!(hw.ram_tier(), RamTier::Small);
        hw.total_ram_gb = 33;
        assert_eq!(hw.ram_tier(), RamTier::Medium);
        hw.total_ram_gb = 64;
        assert_eq!(hw.ram_tier(), RamTier::Medium);
        hw.total_ram_gb = 65;
        assert_eq!(hw.ram_tier(), RamTier::Large);
        hw.total_ram_gb = 96;
        assert_eq!(hw.ram_tier(), RamTier::Large);
        hw.total_ram_gb = 128;
        assert_eq!(hw.ram_tier(), RamTier::Xl);
    }

    #[test]
    fn detect_runs_without_panic() {
        // We can't strongly assert values (depends on the test machine) but
        // the call should never panic and should produce *some* arch string.
        let spec = detect();
        assert!(!spec.arch.is_empty());
    }

    #[test]
    fn one_line_summary_includes_key_fields() {
        let hw = HardwareSpec {
            platform: Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 128,
            physical_cores: 16,
            performance_cores: Some(12),
            efficiency_cores: Some(4),
            has_unified_memory: true,
        };
        let s = hw.one_line_summary();
        assert!(s.contains("Apple Silicon"));
        assert!(s.contains("128 GB"));
        assert!(s.contains("XL"));
        assert!(s.contains("12P+4E"));
        assert!(s.contains("unified memory"));
    }

    #[test]
    fn platform_label_human_readable() {
        assert_eq!(Platform::AppleSilicon.label(), "Apple Silicon");
        assert_eq!(Platform::Linux.label(), "Linux");
    }
}
