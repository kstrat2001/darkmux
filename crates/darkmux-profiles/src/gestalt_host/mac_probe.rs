//! [`MacProbe`] — the macOS [`ResourceProbe`] (#1274 packet 2b).
//!
//! Apple Silicon's unified memory is presented as ONE `"unified"` pool (the
//! #1274 pools-as-data decision: a CUDA box would present `system-ram` +
//! per-GPU `vram` pools through a different probe; the planning core never
//! branches on platform, only on pool math).
//!
//! Sources are kernel counters only — `sysctl -n hw.memsize` for capacity,
//! `vm_stat` for availability — per the observer-must-not-join-the-observed
//! constraint (#1286 design note): a measurement path never dispatches to a
//! model and costs zero tokens, zero Metal work.
//!
//! **`available_bytes` is CONSERVATIVE by design**: `Pages free × page size`
//! only. Inactive, speculative, and purgeable pages are reclaimable on macOS
//! and are deliberately EXCLUDED — they are not guaranteed-free at the
//! moment a multi-GB model load commits its allocation. For load planning,
//! optimistic availability that fails to materialize turns a planned Load
//! into a live refusal or memory pressure at execution time (the #1139
//! failure shape); a conservative number at worst plans an unnecessary
//! eviction the operator can see and override (operator sovereignty, #44).
//! This is the opposite tilt from the host-telemetry sampler's
//! `mem_percent_from_vm_stat` (darkmux-crew), which counts inactive +
//! speculative as available because it reports *pressure* for observation —
//! the two answer different questions, deliberately.

use darkmux_gestalt::{PoolFact, PoolId, Pools, ProbeError, ResourceProbe};

/// The single pool name this probe emits. Named per the #1274 pools-as-data
/// vocabulary ("unified" on Apple Silicon).
pub const UNIFIED_POOL: &str = "unified";

/// macOS unified-memory probe. Stateless; each [`ResourceProbe::pools`] call
/// re-reads the kernel counters.
///
/// **v1 scope (#1274): macOS only.** On any other platform `pools()` returns
/// [`ProbeError::Unavailable`] — the seam exists for a Linux/CUDA probe, but
/// no second probe ships until a real need shows up (the KISS line).
#[derive(Debug, Clone, Copy, Default)]
pub struct MacProbe;

impl ResourceProbe for MacProbe {
    fn pools(&mut self) -> Result<Pools, ProbeError> {
        probe_pools()
    }
}

#[cfg(target_os = "macos")]
fn probe_pools() -> Result<Pools, ProbeError> {
    use std::process::Command;
    let memsize = run_ok(Command::new("sysctl").args(["-n", "hw.memsize"]))
        .ok_or_else(|| ProbeError::Unavailable { detail: "`sysctl -n hw.memsize` failed".into() })?;
    let capacity_bytes = parse_memsize(&memsize).ok_or_else(|| ProbeError::Unavailable {
        detail: format!("could not parse `sysctl -n hw.memsize` output: {}", memsize.trim()),
    })?;
    let vm_stat = run_ok(&mut Command::new("vm_stat"))
        .ok_or_else(|| ProbeError::Unavailable { detail: "`vm_stat` failed".into() })?;
    let available_bytes = parse_vm_stat_free_bytes(&vm_stat).ok_or_else(|| ProbeError::Unavailable {
        detail: "could not parse `Pages free` out of vm_stat output".into(),
    })?;
    Ok(unified_pool(capacity_bytes, available_bytes))
}

#[cfg(not(target_os = "macos"))]
fn probe_pools() -> Result<Pools, ProbeError> {
    Err(ProbeError::Unavailable {
        detail: "the v1 resource probe is macOS-only (#1274 v1 scope) — no pool facts on this platform"
            .to_string(),
    })
}

/// Run `cmd`, returning stdout on a zero exit — the `run_ok` shape from the
/// host-telemetry sampler (darkmux-crew), kept local so this crate doesn't
/// grow a crew dependency for a three-line helper.
#[cfg(target_os = "macos")]
fn run_ok(cmd: &mut std::process::Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── pure parsers (canned-output tests below; compiled on every platform) ──

/// Parse `sysctl -n hw.memsize` output (a bare integer, possibly padded).
fn parse_memsize(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// Parse conservative available bytes out of `vm_stat` output:
/// `Pages free × page size`, nothing else (see the module docs for why
/// inactive/speculative/purgeable are excluded). Parsing style mirrors the
/// host-telemetry sampler's `mem_percent_from_vm_stat`: page size from
/// vm_stat's own header (`page size of N bytes`), defaulting to 16384 on
/// Apple Silicon; field values are `NNN.`-suffixed counts.
fn parse_vm_stat_free_bytes(vm_stat: &str) -> Option<u64> {
    let page = vm_stat
        .lines()
        .next()
        .and_then(|l| l.split("page size of").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(16384);
    let free = vm_stat
        .lines()
        .find(|l| l.trim_start().starts_with("Pages free"))
        .and_then(|l| l.rsplit(':').next())
        .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())?;
    Some(free.saturating_mul(page))
}

/// Assemble the one-pool `Pools` map.
fn unified_pool(capacity_bytes: u64, available_bytes: u64) -> Pools {
    Pools::from([(
        PoolId(UNIFIED_POOL.to_string()),
        PoolFact { capacity_bytes, available_bytes },
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real vm_stat shape from an Apple Silicon machine.
    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
        Pages free:                              228186.\n\
        Pages active:                           2733923.\n\
        Pages inactive:                         2115594.\n\
        Pages speculative:                        63114.\n\
        Pages throttled:                              0.\n\
        Pages wired down:                        450334.\n\
        Pages purgeable:                          11036.\n";

    #[test]
    fn free_bytes_counts_only_pages_free() {
        // Conservative by design: 228186 × 16384 — inactive (2115594),
        // speculative (63114), and purgeable (11036) are all EXCLUDED even
        // though macOS could reclaim them; they are not guaranteed-free when
        // a model load commits (see module docs).
        assert_eq!(parse_vm_stat_free_bytes(VM_STAT), Some(228_186 * 16_384));
    }

    #[test]
    fn free_bytes_defaults_page_size_when_header_absent() {
        let no_header = "Pages free: 1000.\nPages inactive: 5000.\n";
        assert_eq!(parse_vm_stat_free_bytes(no_header), Some(1000 * 16_384));
    }

    #[test]
    fn free_bytes_reads_page_size_from_header() {
        let four_k = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\n\
            Pages free: 1000.\n";
        assert_eq!(parse_vm_stat_free_bytes(four_k), Some(1000 * 4_096));
    }

    #[test]
    fn free_bytes_missing_field_is_none() {
        let no_free = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
            Pages inactive: 5000.\n";
        assert_eq!(parse_vm_stat_free_bytes(no_free), None);
    }

    #[test]
    fn memsize_parses_trimmed_integer() {
        assert_eq!(parse_memsize("137438953472\n"), Some(137_438_953_472));
        assert_eq!(parse_memsize("not-a-number"), None);
    }

    #[test]
    fn unified_pool_shape() {
        let pools = unified_pool(137_438_953_472, 3_738_599_424);
        assert_eq!(pools.len(), 1);
        assert_eq!(
            pools.get(&PoolId("unified".into())),
            Some(&PoolFact { capacity_bytes: 137_438_953_472, available_bytes: 3_738_599_424 })
        );
    }

    /// The one test exercising the REAL shell-outs — macOS-gated like the
    /// telemetry sampler's real-path test; on other platforms the probe's
    /// documented v1 behavior (Unavailable) is asserted instead.
    #[test]
    fn probe_real_path_matches_platform_contract() {
        let mut probe = MacProbe;
        let result = probe.pools();
        #[cfg(target_os = "macos")]
        {
            let pools = result.expect("macOS kernel counters are always readable");
            let fact = pools.get(&PoolId("unified".into())).expect("one unified pool");
            assert!(fact.capacity_bytes > 0);
            assert!(fact.available_bytes <= fact.capacity_bytes);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                matches!(result, Err(ProbeError::Unavailable { .. })),
                "documented v1 scope: non-macOS is Unavailable"
            );
        }
    }
}
