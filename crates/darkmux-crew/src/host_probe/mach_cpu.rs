//! (#2108) Mach kernel-counter reads: per-core CPU tick deltas, perf-level
//! (cluster) topology, and physical-memory pressure.
//!
//! Everything here is a KERNEL COUNTER READ — no process spawn, no model
//! dispatch, no Metal work (CLAUDE.md "the observer must not join the
//! observed", constraint 1). Measured on an M5 Max / macOS 26:
//! `host_processor_info` ≈ 0.009 ms, `host_statistics64` ≈ 0.001 ms. The
//! `top -l 1 -n 0` shell-out this replaces cost ~700-900 ms per sample AND
//! reported a since-boot average on its first (only) reading, so every CPU
//! number darkmux recorded before #2108 was both expensive and wrong-ish.
//!
//! The pure parts (cluster→core-index mapping, tick-delta arithmetic,
//! memory-pressure arithmetic) are separated from the two `unsafe` reads so
//! they can be unit-tested with plain integers.

/// One `hw.perflevelN` entry. `index` is the sysctl's own N — **0 is the
/// HIGHEST-performance tier** ("Super" on M5, "Performance" on M1-M4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfLevel {
    pub index: usize,
    pub name: String,
    pub logical_cpus: usize,
}

/// One core's four mach tick counters (`PROCESSOR_CPU_LOAD_INFO`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreTicks {
    pub user: u32,
    pub system: u32,
    pub idle: u32,
    pub nice: u32,
}

/// Map perf levels onto the mach core-index blocks they occupy.
///
/// **Core indices run LOWEST tier first, which is the REVERSE of the
/// `hw.perflevelN` numbering.** Verified empirically on this M5 Max
/// (2026-08-29): with `hw.perflevel0 = Super(6)` and `hw.perflevel1 =
/// Performance(12)`, six `QOS_CLASS_USER_INTERACTIVE` spinners saturated
/// mach cores 12-17 while six `QOS_CLASS_BACKGROUND` spinners saturated
/// cores 0-5. The same rule reproduces the long-known M1 layout (perflevel0
/// = P(8), perflevel1 = E(2) → the two E cores are cpu0/cpu1), so it is a
/// property of the ordering convention rather than of this one chip.
///
/// Returns one range per level, ALIGNED WITH `levels` (so `ranges[i]` is
/// `levels[i]`'s block), not in core order. Pure — no syscall.
pub fn core_ranges(levels: &[PerfLevel]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = vec![0usize..0usize; levels.len()];
    let mut base = 0usize;
    // Lowest tier (highest sysctl index) claims the lowest core indices.
    for i in (0..levels.len()).rev() {
        let n = levels[i].logical_cpus;
        ranges[i] = base..(base + n);
        base += n;
    }
    ranges
}

/// Busy percent over one core-index range, from two tick snapshots.
///
/// `busy = user + system + nice`, `total = busy + idle`, both measured as
/// the DELTA between snapshots — so the result is a TRUE mean over the
/// interval between the two reads, not a since-boot average (which is
/// exactly what `top -l 1`'s single sample reported). `None` when the range
/// is out of bounds for either snapshot or when no ticks elapsed (a zero
/// denominator is "not measured", never 0%).
///
/// Tick counters are `u32` and wrap; `wrapping_sub` is correct across a wrap
/// as long as fewer than 2^32 ticks (≈497 days at 100 Hz) elapsed between
/// snapshots. Pure.
pub fn range_busy_pct(
    prev: &[CoreTicks],
    cur: &[CoreTicks],
    range: &std::ops::Range<usize>,
) -> Option<u64> {
    if range.end > prev.len() || range.end > cur.len() || range.is_empty() {
        return None;
    }
    let (mut busy, mut total) = (0u64, 0u64);
    for i in range.clone() {
        let du = cur[i].user.wrapping_sub(prev[i].user) as u64;
        let ds = cur[i].system.wrapping_sub(prev[i].system) as u64;
        let dn = cur[i].nice.wrapping_sub(prev[i].nice) as u64;
        let di = cur[i].idle.wrapping_sub(prev[i].idle) as u64;
        busy += du + ds + dn;
        total += du + ds + dn + di;
    }
    if total == 0 {
        return None;
    }
    Some(((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0).round() as u64)
}

/// Memory-pressure percent from raw `vm_statistics64` page counts.
///
/// **Deliberately byte-identical in semantics to the `vm_stat`-parsing
/// [`crate::telemetry_sampler::mem_percent_from_vm_stat`] it replaces**, so
/// the number the viewer renders does not move when the SOURCE moves from a
/// shell-out to a kernel counter. `vm_stat` prints `Pages free` as
/// `free_count - speculative_count` and `Pages speculative` as
/// `speculative_count`; that parser then counts free + inactive +
/// speculative as available, which reduces to `free_count + inactive_count`
/// here. Cross-checked live on this machine (2026-08-29): both paths
/// reported 51%.
///
/// `None` when `total_bytes` or `page_size` is 0. Pure.
pub fn mem_pct_from_pages(
    free_count: u64,
    inactive_count: u64,
    page_size: u64,
    total_bytes: u64,
) -> Option<u64> {
    if total_bytes == 0 || page_size == 0 {
        return None;
    }
    let avail = free_count
        .saturating_add(inactive_count)
        .saturating_mul(page_size);
    let used = total_bytes.saturating_sub(avail);
    Some(((used as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0).round() as u64)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
// `libc` deprecates `mach_host_self`/`mach_task_self` in favor of the `mach2`
// crate. Taking that advice means a new direct dependency for two symbols
// that are stable Darwin ABI and are not going anywhere; #2108 adds `libc`
// and nothing else on purpose (CLAUDE.md: "don't add dependencies casually").
// The deprecation is a packaging opinion, not an ABI warning.
#[allow(deprecated)]
mod imp {
    use super::{CoreTicks, PerfLevel};
    use std::ffi::CString;

    fn sysctl_u32(name: &str) -> Option<u32> {
        let cname = CString::new(name).ok()?;
        let mut out: u32 = 0;
        let mut len = std::mem::size_of::<u32>();
        // SAFETY: `out`/`len` are a correctly-sized u32 destination for an
        // integer sysctl; a non-zero return leaves `out` untouched and we
        // discard it.
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                &mut out as *mut u32 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<u32>()).then_some(out)
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        let cname = CString::new(name).ok()?;
        let mut out: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: as `sysctl_u32`, with a u64 destination.
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                &mut out as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<u64>()).then_some(out)
    }

    fn sysctl_string(name: &str) -> Option<String> {
        let cname = CString::new(name).ok()?;
        let mut len: usize = 0;
        // SAFETY: a null destination with a valid `len` pointer is the
        // documented "how big is it" probe form of sysctlbyname.
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` is `len` bytes, exactly what the probe above asked for.
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        let s = buf.split(|b| *b == 0).next()?;
        Some(String::from_utf8_lossy(s).into_owned())
    }

    /// Enumerate `hw.perflevelN` — index 0 is the highest-performance tier.
    /// Empty when the host reports no perf levels (Intel, or a kernel that
    /// predates the sysctl), which the caller renders as `cpu_clusters: null`.
    pub fn perf_levels() -> Vec<PerfLevel> {
        let n = sysctl_u32("hw.nperflevels").unwrap_or(0) as usize;
        (0..n)
            .filter_map(|i| {
                let logical = sysctl_u32(&format!("hw.perflevel{i}.logicalcpu"))? as usize;
                let name = sysctl_string(&format!("hw.perflevel{i}.name"))
                    .unwrap_or_else(|| format!("level{i}"));
                (logical > 0).then_some(PerfLevel { index: i, name, logical_cpus: logical })
            })
            .collect()
    }

    /// One `host_processor_info(PROCESSOR_CPU_LOAD_INFO)` read — per-core
    /// cumulative tick counters, in mach core-index order. `None` on any
    /// kernel error (which degrades the whole CPU block to null rather than
    /// substituting a fabricated reading).
    pub fn per_core_ticks() -> Option<Vec<CoreTicks>> {
        let mut cpu_count: libc::natural_t = 0;
        let mut info: libc::processor_info_array_t = std::ptr::null_mut();
        let mut info_count: libc::mach_msg_type_number_t = 0;
        // SAFETY: the out-params are the shapes host_processor_info documents;
        // on KERN_SUCCESS it hands back a vm_allocate'd array we own and free
        // below, and on failure it writes nothing we read.
        let kr = unsafe {
            libc::host_processor_info(
                libc::mach_host_self(),
                libc::PROCESSOR_CPU_LOAD_INFO,
                &mut cpu_count,
                &mut info,
                &mut info_count,
            )
        };
        if kr != 0 || info.is_null() {
            return None;
        }
        let stride = libc::CPU_STATE_MAX as usize;
        let mut out = Vec::with_capacity(cpu_count as usize);
        for i in 0..cpu_count as usize {
            let base = i * stride;
            if base + stride > info_count as usize {
                break;
            }
            // SAFETY: `base + stride` is bounds-checked against the array
            // length the kernel reported.
            let s = unsafe { std::slice::from_raw_parts(info.add(base), stride) };
            out.push(CoreTicks {
                user: s[libc::CPU_STATE_USER as usize] as u32,
                system: s[libc::CPU_STATE_SYSTEM as usize] as u32,
                idle: s[libc::CPU_STATE_IDLE as usize] as u32,
                nice: s[libc::CPU_STATE_NICE as usize] as u32,
            });
        }
        // SAFETY: freeing exactly the allocation host_processor_info handed us,
        // with the byte length it reported. Not freeing it leaks ~300 bytes per
        // sample, which at a 2s cadence is a real leak over a long dispatch.
        unsafe {
            libc::vm_deallocate(
                libc::mach_task_self(),
                info as libc::vm_address_t,
                (info_count as usize * std::mem::size_of::<libc::integer_t>()) as libc::vm_size_t,
            );
        }
        (!out.is_empty()).then_some(out)
    }

    /// Memory pressure via `host_statistics64(HOST_VM_INFO64)` + `hw.memsize`
    /// — the kernel-counter twin of the `vm_stat` + `sysctl` shell-out pair.
    pub fn mem_pct() -> Option<u64> {
        let total = sysctl_u64("hw.memsize")?;
        let mut st: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = (std::mem::size_of::<libc::vm_statistics64>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        // SAFETY: `st` is a zeroed vm_statistics64 and `count` is its size in
        // integer_t units, which is exactly the contract HOST_VM_INFO64 takes.
        let kr = unsafe {
            libc::host_statistics64(
                libc::mach_host_self(),
                libc::HOST_VM_INFO64,
                &mut st as *mut libc::vm_statistics64 as *mut libc::integer_t,
                &mut count,
            )
        };
        if kr != 0 {
            return None;
        }
        let page = sysctl_u64("hw.pagesize").unwrap_or(16384);
        super::mem_pct_from_pages(st.free_count as u64, st.inactive_count as u64, page, total)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use imp::{mem_pct, per_core_ticks, perf_levels};

// Non-Apple-Silicon fallback: every reader reports "not measured" rather
// than a fabricated zero. The pure helpers above stay compiled and tested
// everywhere so the arithmetic is covered on any CI host.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn perf_levels() -> Vec<PerfLevel> {
    Vec::new()
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn per_core_ticks() -> Option<Vec<CoreTicks>> {
    None
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn mem_pct() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvl(index: usize, name: &str, n: usize) -> PerfLevel {
        PerfLevel { index, name: name.to_string(), logical_cpus: n }
    }

    #[test]
    fn core_ranges_put_the_lowest_tier_at_the_lowest_core_indices() {
        // This machine (M5 Max): perflevel0 = Super(6), perflevel1 =
        // Performance(12). Measured with QoS-pinned spinners: USER_INTERACTIVE
        // saturated cores 12-17, BACKGROUND saturated cores 0-5.
        let levels = vec![lvl(0, "Super", 6), lvl(1, "Performance", 12)];
        let r = core_ranges(&levels);
        assert_eq!(r[0], 12..18, "Super (perflevel0) occupies the HIGH indices");
        assert_eq!(r[1], 0..12, "Performance (perflevel1) occupies the LOW indices");
    }

    #[test]
    fn core_ranges_reproduce_the_m1_layout() {
        // M1 Max: perflevel0 = Performance(8), perflevel1 = Efficiency(2), and
        // the two E cores are long known to be cpu0/cpu1.
        let levels = vec![lvl(0, "Performance", 8), lvl(1, "Efficiency", 2)];
        let r = core_ranges(&levels);
        assert_eq!(r[1], 0..2, "the E cores are cpu0/cpu1");
        assert_eq!(r[0], 2..10);
    }

    #[test]
    fn core_ranges_handle_a_single_level_and_no_levels() {
        assert_eq!(core_ranges(&[lvl(0, "CPU", 4)]), vec![0..4]);
        assert!(core_ranges(&[]).is_empty());
    }

    fn ticks(user: u32, system: u32, idle: u32, nice: u32) -> CoreTicks {
        CoreTicks { user, system, idle, nice }
    }

    #[test]
    fn range_busy_pct_is_a_true_delta_mean() {
        // Two cores, each +25 busy / +75 idle over the interval → 25%.
        let prev = vec![ticks(100, 0, 900, 0), ticks(100, 0, 900, 0)];
        let cur = vec![ticks(120, 5, 975, 0), ticks(120, 5, 975, 0)];
        assert_eq!(range_busy_pct(&prev, &cur, &(0..2)), Some(25));
    }

    #[test]
    fn range_busy_pct_counts_nice_as_busy() {
        let prev = vec![ticks(0, 0, 0, 0)];
        let cur = vec![ticks(0, 0, 50, 50)];
        assert_eq!(range_busy_pct(&prev, &cur, &(0..1)), Some(50), "nice is busy, not idle");
    }

    #[test]
    fn range_busy_pct_none_when_nothing_elapsed_or_out_of_bounds() {
        let same = vec![ticks(10, 10, 10, 10)];
        assert_eq!(
            range_busy_pct(&same, &same, &(0..1)),
            None,
            "zero elapsed ticks is 'not measured', never 0%"
        );
        assert_eq!(range_busy_pct(&same, &same, &(0..5)), None, "range past the snapshot");
        assert_eq!(range_busy_pct(&same, &same, &(0..0)), None, "empty range");
    }

    #[test]
    fn range_busy_pct_survives_a_u32_counter_wrap() {
        let prev = vec![ticks(u32::MAX - 10, 0, 0, 0)];
        let cur = vec![ticks(9, 0, 90, 0)];
        // user delta = 20 (wrapping), idle delta = 90 → 20/110 = 18.18 → 18
        assert_eq!(range_busy_pct(&prev, &cur, &(0..1)), Some(18));
    }

    #[test]
    fn mem_pct_from_pages_matches_the_vm_stat_parser_semantics() {
        // Same scenario as telemetry_sampler's
        // `mem_percent_from_vm_stat_counts_speculative_as_available`:
        // vm_stat would print free=2_000_000, speculative=1_000_000,
        // inactive=2_500_000, so free_count = 3_000_000 here.
        let got = mem_pct_from_pages(3_000_000, 2_500_000, 16384, 137_438_953_472);
        let want = crate::telemetry_sampler::mem_percent_from_vm_stat(
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
             Pages free:                             2000000.\n\
             Pages active:                           3000000.\n\
             Pages speculative:                      1000000.\n\
             Pages inactive:                         2500000.\n\
             Pages wired down:                        500000.\n",
            137_438_953_472,
        );
        assert_eq!(got, want, "the mach path must not move the number the shell-out reported");
        assert_eq!(got, Some(34));
    }

    #[test]
    fn mem_pct_from_pages_none_on_zero_total_or_page_size() {
        assert_eq!(mem_pct_from_pages(1, 1, 16384, 0), None);
        assert_eq!(mem_pct_from_pages(1, 1, 0, 1_000_000), None);
    }
}
