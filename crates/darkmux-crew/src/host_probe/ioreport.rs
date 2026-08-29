//! (#2108) Apple Silicon power + DVFS-frequency counters, read through the
//! PRIVATE IOReport framework.
//!
//! **IOReport is loaded at RUNTIME via `dlopen`/`dlsym`, never at link
//! time.** It is a private framework: its path moved between macOS releases
//! (on macOS 26 the `/System/Library/PrivateFrameworks/IOReport.framework`
//! bundle is gone entirely and only `/usr/lib/libIOReport.dylib` resolves),
//! and a link-time dependency on it would mean a darkmux binary that refuses
//! to START on a host where the symbol moved. Every failure mode here —
//! framework missing, symbol missing, channel absent, unit label unknown,
//! subscription refused — degrades to `None` fields on the sample. Nothing
//! panics, nothing aborts the sampler, and the unavailability is logged
//! ONCE per process rather than once per tick.
//!
//! Costs measured on this machine (M5 Max, macOS 26.5.1, 2026-08-29):
//! enumerating + filtering all 11,141 channels down to the 24 darkmux wants
//! is a ~75-95 ms ONE-TIME construction cost; `IOReportCreateSubscription`
//! is 0.7 ms; and each subsequent `IOReportCreateSamples` + delta is
//! ~4.4 ms. Subscribing to the unfiltered channel set instead costs 78 ms
//! PER SAMPLE — which is why the filter is applied once at construction and
//! the subscription is held for the probe's lifetime.
//!
//! Observed channel inventory on this host (recorded so a future reader can
//! tell "this chip doesn't have it" from "our filter is wrong"):
//!
//! | group | subgroup | channels | unit |
//! |---|---|---|---|
//! | `Energy Model` | (none) | `CPU Energy`, `ANE`, `GPU` | `mJ` |
//! | `Energy Model` | (none) | `GPU Energy` | `nJ` |
//! | `Energy Model` | (none) | `PCIe Port 0 Energy`, … | `uJ` |
//! | `CPU Stats` | `CPU Core Performance States` | `PCPU0`-`PCPU5` (22 states), `MCPU00`-`MCPU05`, `MCPU10`-`MCPU15` (17 states) | `24Mticks` |
//! | `GPU Stats` | `GPU Performance States` | `GPUPH` (16 states) | `24Mticks` |
//!
//! Three different energy units appear in ONE group, which is why the unit
//! label is read per channel and an unrecognized one yields `None` instead
//! of a guessed scale.

use std::collections::BTreeMap;

// ── Pure arithmetic (compiled and tested on every target) ──────────────────

/// State names that mean "this core/GPU was not executing" and therefore sit
/// BEFORE the DVFS-state residencies in a channel's state list.
const INACTIVE_STATES: [&str; 3] = ["IDLE", "DOWN", "OFF"];

/// Residency-weighted mean operating frequency, in MHz.
///
/// A perf-state channel reports `[<inactive states…>, <one residency per
/// DVFS state…>]` — e.g. `[DOWN, IDLE, V0P14, …, V14P0]` for a CPU core, or
/// `[OFF, P1, …, P15]` for the GPU. `freqs` is the SoC's frequency table for
/// that cluster, one entry per DVFS state in the same order. The result is
/// Σ(residency_i / active_total) × freq_i — the frequency the unit actually
/// ran at while it was running, not a nominal maximum.
///
/// Trailing states past `freqs.len()` are IGNORED rather than treated as an
/// error: the GPU reports 15 P-states while the SoC's table lists 13, and
/// the extra two are always zero. `res.len() > freqs.len()` is required so a
/// table that is too LONG for the channel (a mis-picked table) is rejected
/// instead of silently reading past the residencies.
///
/// A fully-idle interval (no active residency at all) reports the DVFS FLOOR
/// rather than `None` — "parked at its lowest state" is a true statement
/// about the interval, and it is what the operator sees on a quiet machine.
/// Pure.
pub fn mean_mhz_from_residency(res: &[(String, i64)], freqs: &[u32]) -> Option<u32> {
    if freqs.is_empty() || res.len() <= freqs.len() {
        return None;
    }
    let off = res
        .iter()
        .position(|(n, _)| !INACTIVE_STATES.contains(&n.as_str()))?;
    if off + freqs.len() > res.len() {
        return None;
    }
    let active = &res[off..off + freqs.len()];
    let usage: f64 = active.iter().map(|(_, v)| (*v).max(0) as f64).sum();
    if usage <= 0.0 {
        return Some(freqs[0]);
    }
    let mut avg = 0.0f64;
    for (i, f) in freqs.iter().enumerate() {
        avg += (active[i].1.max(0) as f64 / usage) * *f as f64;
    }
    Some(avg.round() as u32)
}

/// How many DVFS states a channel's residency list carries — the count used
/// to pick its frequency table. `None` when the list is all-inactive (no
/// active state to anchor on). Pure.
pub fn active_state_count(res: &[(String, i64)]) -> Option<usize> {
    let off = res
        .iter()
        .position(|(n, _)| !INACTIVE_STATES.contains(&n.as_str()))?;
    Some(res.len() - off)
}

/// Convert an IOReport Energy Model delta into milliwatts over the interval.
///
/// The unit label is read PER CHANNEL and matched against a closed set —
/// on this machine one `Energy Model` group carries `mJ`, `uJ` AND `nJ`
/// channels, so a single assumed scale would be wrong by a factor of a
/// million for some of them. **An unrecognized unit yields `None`**, never a
/// guessed scale: a silently mis-scaled watt number is worse than an absent
/// one, because it looks like data.
///
/// A negative delta (a counter reset across e.g. a sleep/wake) also yields
/// `None` rather than a negative wattage. Pure.
pub fn energy_delta_to_mw(delta: i64, unit: &str, interval_ms: u64) -> Option<f64> {
    if interval_ms == 0 || delta < 0 {
        return None;
    }
    let joules = match unit.trim() {
        "mJ" => delta as f64 / 1e3,
        // Both the ASCII and the micro-sign spellings; IOReport emits the
        // ASCII one on this host.
        "uJ" | "\u{b5}J" => delta as f64 / 1e6,
        "nJ" => delta as f64 / 1e9,
        _ => return None,
    };
    Some(joules / (interval_ms as f64 / 1000.0) * 1000.0)
}

/// Parse a `voltage-states*` IORegistry blob into its first-u32-per-8-bytes
/// values. Each entry is an 8-byte `(value, voltage)` pair, little-endian.
/// Pure.
pub fn parse_voltage_states(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(8)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Is this a plausible CPU frequency table, once scaled to MHz?
///
/// The `pmgr` node carries ~24 `voltage-states*` blobs, most of which are
/// voltages, GPU tables, or all-ones placeholders. A CPU table is
/// strictly ascending and lands in a physically sensible MHz band. Without
/// this guard the table picker would happily match a voltage table that
/// happened to have the right LENGTH. Pure.
fn plausible_cpu_mhz(t: &[u32]) -> bool {
    t.len() >= 2
        && t.iter().all(|v| (200..=10_000).contains(v))
        && t.windows(2).all(|w| w[0] < w[1])
}

/// The SoC's CPU frequency tables in MHz, keyed by table length.
///
/// On M4+ the FREQUENCIES live in the `-sram` variants (kHz) while the bare
/// `voltage-statesN` keys hold voltages; on M1-M3 they were in Hz. Both
/// scalings are attempted and whichever produces a plausible MHz table wins,
/// so this does not need a chip-family lookup that would go stale.
///
/// Keying by LENGTH is what lets a core channel find its own table without
/// any cluster-name or `acc-clusters`-ordering guesswork: a channel with N
/// DVFS states needs the N-entry table. Verified on this machine — the
/// 6 `PCPU*` cores have 20 DVFS states and `voltage-states5-sram` has 20
/// entries (1308-4608 MHz); the 12 `MCPU*` cores have 15 and
/// `voltage-states22-sram`/`23-sram` have 15 (1344-4380 MHz). Pure.
pub fn cpu_freq_tables(props: &[(String, Vec<u8>)]) -> BTreeMap<usize, Vec<u32>> {
    let mut out: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (key, bytes) in props {
        if !key.starts_with("voltage-states") {
            continue;
        }
        let raw = parse_voltage_states(bytes);
        for scale in [1_000u32, 1_000_000u32] {
            let t: Vec<u32> = raw.iter().map(|v| v / scale).collect();
            if plausible_cpu_mhz(&t) {
                out.entry(t.len()).or_insert(t);
                break;
            }
        }
    }
    out
}

/// The GPU's DVFS frequency table in MHz.
///
/// Read from `voltage-states9` (Hz) and with the leading `0` dropped — that
/// first entry is the OFF state, which has no residency slot of its own in
/// `GPUPH`'s active states. The key is named explicitly rather than
/// discovered by shape: several `voltage-states*` blobs on this machine
/// share the GPU's value range (`voltage-states8` is a 26-entry
/// 732-2472 MHz table), so a shape-matching search would be a coin flip.
/// An absent key means `gpu_mhz: null`, which is the honest outcome. Pure.
pub fn gpu_freq_table(props: &[(String, Vec<u8>)]) -> Vec<u32> {
    let Some((_, bytes)) = props.iter().find(|(k, _)| k == "voltage-states9") else {
        return Vec::new();
    };
    let t: Vec<u32> = parse_voltage_states(bytes)
        .into_iter()
        .map(|v| v / 1_000_000)
        .skip_while(|v| *v == 0)
        .collect();
    if t.len() >= 2 && t.windows(2).all(|w| w[0] < w[1]) {
        t
    } else {
        Vec::new()
    }
}

/// Assign IOReport core-channel groups to `hw.perflevelN` clusters.
///
/// IOReport names core channels by SoC cluster (`PCPU0`-`PCPU5`,
/// `MCPU00`-`MCPU15`), which is not the same partition as the perf levels —
/// this chip's 12 `Performance` cores are two IOReport clusters (`MCPU0*`
/// and `MCPU1*`). Grouping by name PREFIX (the leading run of non-digits)
/// collapses those back together, and the resulting group sizes are then
/// matched to each level's core count.
///
/// **Core count alone is not a unique key on a chip whose perf levels have
/// equal core counts** (e.g. a hypothetical 4-Performance + 4-Efficiency
/// part) — matching by count only, in `groups`' BTreeMap-sorted (i.e.
/// alphabetical, NOT tier) order, silently assigned whichever
/// same-sized group's NAME happened to sort first to the FIRST level
/// asking, regardless of which tier either one actually was. On a chip
/// with `groups = {ECPU: 4 → 1200 MHz, PCPU: 4 → 3000 MHz}` (alphabetical:
/// ECPU before PCPU) and `levels = [Performance(4), Efficiency(4)]`, the
/// Performance cluster silently got ECPU's 1200 MHz instead of PCPU's
/// 3000 MHz.
///
/// `levels` is already given in TIER order (index 0 = highest-performance
/// tier — see [`crate::host_probe::mach_cpu::PerfLevel`]), and a
/// higher-performance cluster always clocks at or above a
/// lower-performance one, so a same-count tie is broken by matching the
/// HIGHEST-mean-MHz unused candidate to the EARLIEST (highest-tier) level
/// asking for that count. A genuine tie in BOTH count and mean MHz can't be
/// told apart this way — left `None` rather than guessed, same as an
/// unmatched count.
///
/// `groups`/`group_mean_mhz` must be sorted the same way and index-aligned
/// (the caller uses a `BTreeMap`, so both come from the same sorted key
/// iteration). Returns, per level, the index into `groups` that serves it —
/// `None` when no unused group has that level's exact core count, or when
/// the tie-break above can't disambiguate, which degrades that cluster's
/// `mhz` to null rather than attaching another cluster's number. Pure.
pub fn assign_groups_to_levels(
    group_sizes: &[usize],
    group_mean_mhz: &[u32],
    level_cores: &[usize],
) -> Vec<Option<usize>> {
    let mut used = vec![false; group_sizes.len()];
    level_cores
        .iter()
        .map(|want| {
            let mut candidates: Vec<usize> = group_sizes
                .iter()
                .enumerate()
                .filter(|(i, n)| !used[*i] && *n == want)
                .map(|(i, _)| i)
                .collect();
            match candidates.len() {
                0 => None,
                1 => {
                    let idx = candidates[0];
                    used[idx] = true;
                    Some(idx)
                }
                _ => {
                    // Highest mean MHz first — matches the earliest
                    // (highest-tier) level asking for this count.
                    candidates.sort_by(|&a, &b| group_mean_mhz[b].cmp(&group_mean_mhz[a]));
                    if group_mean_mhz[candidates[0]] == group_mean_mhz[candidates[1]] {
                        // A genuine tie in both count and clock — cannot
                        // disambiguate, leave unassigned rather than guess.
                        return None;
                    }
                    let idx = candidates[0];
                    used[idx] = true;
                    Some(idx)
                }
            }
        })
        .collect()
}

/// The leading run of non-digit characters in an IOReport channel name —
/// `MCPU00` and `MCPU15` both collapse to `MCPU`. Pure.
pub fn channel_prefix(name: &str) -> String {
    name.chars().take_while(|c| !c.is_ascii_digit()).collect()
}

// ── The live probe ─────────────────────────────────────────────────────────

/// One channel's reading out of a delta sample.
#[derive(Debug, Clone)]
pub struct ChannelReading {
    pub group: String,
    pub subgroup: String,
    pub name: String,
    pub unit: String,
    /// `IOReportSimpleGetIntegerValue` — meaningful for SIMPLE channels.
    pub value: i64,
    /// `(state name, residency)` per state — empty for SIMPLE channels.
    pub states: Vec<(String, i64)>,
}

/// What one delta sample yielded: per-cluster mean MHz (keyed by the
/// channel-name prefix), GPU mean MHz, and the three power rails in mW.
#[derive(Debug, Clone, Default)]
pub struct IoReportSample {
    pub cluster_mhz: BTreeMap<String, u32>,
    /// Core count per cluster prefix, so the caller can match clusters to
    /// perf levels without re-deriving the grouping.
    pub cluster_cores: BTreeMap<String, usize>,
    pub gpu_mhz: Option<u32>,
    pub cpu_mw: Option<f64>,
    pub gpu_mw: Option<f64>,
    pub ane_mw: Option<f64>,
}

/// Fold a delta sample's channel readings into an [`IoReportSample`]. Pure —
/// this is where every number the probe reports is actually computed, so the
/// whole derivation is unit-testable from fixtures without IOReport present.
pub fn fold_readings(
    readings: &[ChannelReading],
    cpu_tables: &BTreeMap<usize, Vec<u32>>,
    gpu_freqs: &[u32],
    interval_ms: u64,
) -> IoReportSample {
    let mut out = IoReportSample::default();
    let mut per_cluster: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    // Energy rails accumulate: an Ultra part reports `DIE_0_CPU Energy` and
    // `DIE_1_CPU Energy` as separate channels, and a chip with several ANE
    // blocks reports `ANE0`, `ANE1`, … — summing is the only reading of
    // "the CPU drew N mW" that stays true across package topologies.
    let (mut cpu, mut gpu, mut ane) = (None::<f64>, None::<f64>, None::<f64>);

    for r in readings {
        if r.group == "CPU Stats" && r.subgroup == "CPU Core Performance States" {
            let Some(n_active) = active_state_count(&r.states) else {
                continue;
            };
            // A channel's own DVFS-state count picks its table; a channel
            // whose count matches no table contributes nothing (null), which
            // is why one unrecognized cluster can't poison the others.
            let Some(freqs) = cpu_tables.get(&n_active) else {
                continue;
            };
            if let Some(mhz) = mean_mhz_from_residency(&r.states, freqs) {
                per_cluster
                    .entry(channel_prefix(&r.name))
                    .or_default()
                    .push(mhz);
            }
        } else if r.group == "GPU Stats" && r.name == "GPUPH" {
            out.gpu_mhz = mean_mhz_from_residency(&r.states, gpu_freqs);
        } else if r.group == "Energy Model" {
            let rail = if r.name == "GPU Energy" {
                &mut gpu
            } else if r.name.ends_with("CPU Energy") {
                &mut cpu
            } else if r.name.starts_with("ANE") {
                &mut ane
            } else {
                continue;
            };
            if let Some(mw) = energy_delta_to_mw(r.value, &r.unit, interval_ms) {
                *rail = Some(rail.unwrap_or(0.0) + mw);
            }
        }
    }

    for (prefix, mhzs) in per_cluster {
        let n = mhzs.len();
        let mean = mhzs.iter().map(|m| *m as f64).sum::<f64>() / n as f64;
        out.cluster_cores.insert(prefix.clone(), n);
        out.cluster_mhz.insert(prefix, mean.round() as u32);
    }
    out.cpu_mw = cpu;
    out.gpu_mw = gpu;
    out.ane_mw = ane;
    out
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use imp::IoReportProbe;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod imp {
    use super::{ChannelReading, IoReportSample};
    use crate::host_probe::iokit;
    use std::collections::BTreeMap;
    use std::ffi::{c_void, CString};

    /// Where `libIOReport` has lived. macOS 26 removed the
    /// `PrivateFrameworks` bundle and ships only the `/usr/lib` dylib;
    /// earlier releases had the bundle. Tried in order, first hit wins.
    const IOREPORT_PATHS: [&str; 3] = [
        "/usr/lib/libIOReport.dylib",
        "/System/Library/PrivateFrameworks/IOReport.framework/IOReport",
        "/System/Library/PrivateFrameworks/IOReport.framework/Versions/A/IOReport",
    ];

    type FnCopyAll = unsafe extern "C" fn(u64, u64) -> *const c_void;
    type FnCreateSub = unsafe extern "C" fn(
        *const c_void,
        *const c_void,
        *mut *mut c_void,
        u64,
        *const c_void,
    ) -> *mut c_void;
    type FnCreateSamples =
        unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void) -> *const c_void;
    type FnCreateDelta =
        unsafe extern "C" fn(*const c_void, *const c_void, *const c_void) -> *const c_void;
    type FnChanStr = unsafe extern "C" fn(*const c_void) -> *const c_void;
    type FnSimpleInt = unsafe extern "C" fn(*const c_void, i32) -> i64;
    type FnStateCount = unsafe extern "C" fn(*const c_void) -> i32;
    type FnStateName = unsafe extern "C" fn(*const c_void, i32) -> *const c_void;
    type FnStateRes = unsafe extern "C" fn(*const c_void, i32) -> i64;

    struct Syms {
        copy_all: FnCopyAll,
        create_sub: FnCreateSub,
        create_samples: FnCreateSamples,
        create_delta: FnCreateDelta,
        get_group: FnChanStr,
        get_subgroup: FnChanStr,
        get_name: FnChanStr,
        get_unit: FnChanStr,
        simple_int: FnSimpleInt,
        state_count: FnStateCount,
        state_name: FnStateName,
        state_res: FnStateRes,
    }

    /// `dlsym` one symbol, transmuting to its declared signature. `None` when
    /// the symbol is absent — which makes the WHOLE probe unavailable rather
    /// than leaving a half-bound function table that would fault on use.
    unsafe fn sym<T: Copy>(handle: *mut c_void, name: &str) -> Option<T> {
        debug_assert_eq!(
            std::mem::size_of::<T>(),
            std::mem::size_of::<*const c_void>(),
            "sym() only transmutes pointer-sized fn types"
        );
        let c = CString::new(name).ok()?;
        let p = libc::dlsym(handle, c.as_ptr());
        (!p.is_null()).then(|| *(&p as *const *mut c_void as *const T))
    }

    unsafe fn load_syms() -> Option<(*mut c_void, Syms)> {
        let mut handle = std::ptr::null_mut();
        for path in IOREPORT_PATHS {
            let Ok(c) = CString::new(path) else { continue };
            let h = libc::dlopen(c.as_ptr(), libc::RTLD_LAZY);
            if !h.is_null() {
                handle = h;
                break;
            }
        }
        if handle.is_null() {
            return None;
        }
        let s = Syms {
            copy_all: sym(handle, "IOReportCopyAllChannels")?,
            create_sub: sym(handle, "IOReportCreateSubscription")?,
            create_samples: sym(handle, "IOReportCreateSamples")?,
            create_delta: sym(handle, "IOReportCreateSamplesDelta")?,
            get_group: sym(handle, "IOReportChannelGetGroup")?,
            get_subgroup: sym(handle, "IOReportChannelGetSubGroup")?,
            get_name: sym(handle, "IOReportChannelGetChannelName")?,
            get_unit: sym(handle, "IOReportChannelGetUnitLabel")?,
            simple_int: sym(handle, "IOReportSimpleGetIntegerValue")?,
            state_count: sym(handle, "IOReportStateGetCount")?,
            state_name: sym(handle, "IOReportStateGetNameForIndex")?,
            state_res: sym(handle, "IOReportStateGetResidency")?,
        };
        Some((handle, s))
    }

    /// A held IOReport subscription plus the SoC's static frequency tables.
    ///
    /// Constructed once and reused: building the filtered channel set costs
    /// ~75-95 ms and the frequency-table read ~7 ms, while each subsequent
    /// sample is ~4.4 ms.
    pub struct IoReportProbe {
        syms: Syms,
        /// The filtered channel dictionary the subscription samples against.
        chan: *mut c_void,
        subs: *mut c_void,
        prev: *const c_void,
        cpu_tables: BTreeMap<usize, Vec<u32>>,
        gpu_freqs: Vec<u32>,
    }

    // SAFETY: every field is an owned CF/IOReport handle. The probe is never
    // shared without external synchronization — the daemon and dispatch
    // samplers each construct their own inside their own thread, and the
    // process-wide `sample_host()` probe is behind a `Mutex`. `Send` is
    // needed only so that `Mutex<HostProbe>` can live in a `static`.
    unsafe impl Send for IoReportProbe {}

    impl Drop for IoReportProbe {
        fn drop(&mut self) {
            // SAFETY: each pointer was either created by us (owned) or is
            // null; `release` no-ops on null. The dlopen handle is
            // deliberately NOT closed — other probes in the process may hold
            // symbols from it, and the loader keeps one copy regardless.
            unsafe {
                iokit::release(self.prev);
                iokit::release(self.subs as *const c_void);
                iokit::release(self.chan as *const c_void);
            }
        }
    }

    impl IoReportProbe {
        /// Build the subscription, or `None` when IOReport is unavailable on
        /// this host. Never panics.
        pub fn new() -> Option<Self> {
            // SAFETY: every call below is null-checked before use, and each
            // owned CF reference is either stored on `self` (released in
            // `Drop`) or released before returning.
            unsafe {
                let (_handle, syms) = load_syms()?;
                let all = (syms.copy_all)(0, 0);
                if all.is_null() {
                    return None;
                }
                // (#2108 review finding) `all` is owned from here on, so
                // every early return below must release it first — a `?`
                // on `key`/`arr` would skip that (a one-time leak, since
                // `new()` runs once per probe construction, not once per
                // sample, but a leak all the same).
                let Some(key) = iokit::CfString::new("IOReportChannels") else {
                    iokit::release(all);
                    return None;
                };
                let Some(arr) = iokit::dict_get(all, "IOReportChannels") else {
                    iokit::release(all);
                    return None;
                };
                let n = iokit::array_count(arr);
                if n <= 0 {
                    iokit::release(all);
                    return None;
                }
                let chan = iokit::dict_mutable_copy(all);
                let sel = iokit::array_mutable(n);
                if chan.is_null() || sel.is_null() {
                    // Whichever of `chan`/`sel` DID allocate is a real,
                    // owned CF reference — `release` no-ops on the null one,
                    // so both calls are safe regardless of which failed.
                    iokit::release(chan as *const c_void);
                    iokit::release(sel as *const c_void);
                    iokit::release(all);
                    return None;
                }
                for i in 0..n {
                    let item = iokit::array_at(arr, i);
                    if item.is_null() {
                        continue;
                    }
                    let g = iokit::cfstring_to_string((syms.get_group)(item)).unwrap_or_default();
                    let sg =
                        iokit::cfstring_to_string((syms.get_subgroup)(item)).unwrap_or_default();
                    let nm = iokit::cfstring_to_string((syms.get_name)(item)).unwrap_or_default();
                    let keep = (g == "Energy Model"
                        && (nm == "GPU Energy"
                            || nm.ends_with("CPU Energy")
                            || nm.starts_with("ANE")))
                        || (g == "CPU Stats" && sg == "CPU Core Performance States")
                        || (g == "GPU Stats" && sg == "GPU Performance States" && nm == "GPUPH");
                    if keep {
                        iokit::array_append(sel, item);
                    }
                }
                iokit::dict_set(chan, key.as_ptr(), sel);
                // `chan` retained `sel` on insert, and `sel` retains each
                // channel dict it holds (see `iokit::array_mutable`), so both
                // our own reference to `sel` and the source dictionary can go
                // now — the filtered channels outlive `all`.
                iokit::release(sel as *const c_void);
                iokit::release(all);

                let mut subbed: *mut c_void = std::ptr::null_mut();
                let subs = (syms.create_sub)(
                    std::ptr::null(),
                    chan as *const c_void,
                    &mut subbed,
                    0,
                    std::ptr::null(),
                );
                if subs.is_null() {
                    iokit::release(chan as *const c_void);
                    return None;
                }
                iokit::release(subbed as *const c_void);

                let props = read_pmgr_voltage_states();
                Some(Self {
                    syms,
                    chan,
                    subs,
                    prev: std::ptr::null(),
                    cpu_tables: super::cpu_freq_tables(&props),
                    gpu_freqs: super::gpu_freq_table(&props),
                })
            }
        }

        /// Whether the SoC frequency tables resolved. When they didn't, power
        /// still reports but every `mhz` field is null.
        pub fn has_freq_tables(&self) -> bool {
            !self.cpu_tables.is_empty()
        }

        /// Take one sample. Returns `None` on the FIRST call (there is no
        /// previous sample to difference against — every number IOReport
        /// gives us is a counter delta) and on any sampling failure.
        pub fn sample(&mut self, interval_ms: u64) -> Option<IoReportSample> {
            // SAFETY: `subs`/`chan` are non-null for the probe's lifetime
            // (checked in `new`); the sample dictionaries are owned and
            // released exactly once each.
            unsafe {
                let cur = (self.syms.create_samples)(
                    self.subs,
                    self.chan as *const c_void,
                    std::ptr::null(),
                );
                if cur.is_null() {
                    return None;
                }
                if self.prev.is_null() {
                    self.prev = cur;
                    return None;
                }
                let delta = (self.syms.create_delta)(self.prev, cur, std::ptr::null());
                iokit::release(self.prev);
                self.prev = cur;
                if delta.is_null() {
                    return None;
                }
                let readings = self.read_channels(delta);
                iokit::release(delta);
                Some(super::fold_readings(
                    &readings,
                    &self.cpu_tables,
                    &self.gpu_freqs,
                    interval_ms,
                ))
            }
        }

        /// # Safety
        /// `delta` must be a valid IOReport samples dictionary.
        unsafe fn read_channels(&self, delta: *const c_void) -> Vec<ChannelReading> {
            let Some(arr) = iokit::dict_get(delta, "IOReportChannels") else {
                return Vec::new();
            };
            let n = iokit::array_count(arr);
            let mut out = Vec::with_capacity(n.max(0) as usize);
            for i in 0..n {
                let item = iokit::array_at(arr, i);
                if item.is_null() {
                    continue;
                }
                let count = (self.syms.state_count)(item);
                let states = (0..count.max(0))
                    .map(|j| {
                        let name = iokit::cfstring_to_string((self.syms.state_name)(item, j))
                            .unwrap_or_else(|| format!("S{j}"));
                        (name, (self.syms.state_res)(item, j))
                    })
                    .collect();
                out.push(ChannelReading {
                    group: iokit::cfstring_to_string((self.syms.get_group)(item))
                        .unwrap_or_default(),
                    subgroup: iokit::cfstring_to_string((self.syms.get_subgroup)(item))
                        .unwrap_or_default(),
                    name: iokit::cfstring_to_string((self.syms.get_name)(item))
                        .unwrap_or_default(),
                    unit: iokit::cfstring_to_string((self.syms.get_unit)(item))
                        .unwrap_or_default(),
                    value: (self.syms.simple_int)(item, 0),
                    states,
                });
            }
            out
        }
    }

    /// Every `voltage-states*` blob on the `pmgr` node — the SoC's DVFS
    /// frequency tables. Read ONCE at probe construction (~7 ms); these are
    /// static hardware descriptors, not counters.
    fn read_pmgr_voltage_states() -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        iokit::for_each_service("AppleARMIODevice", |props| {
            // SAFETY: `props` is a live property dictionary for the duration
            // of the callback, and every accessor type-checks before reading.
            unsafe {
                // The pmgr node is the one carrying `acc-clusters`; other
                // AppleARMIODevice nodes have no frequency tables.
                if iokit::dict_get(props, "acc-clusters").is_none() {
                    return;
                }
                for (k, v) in iokit::dict_pairs(props) {
                    if k.starts_with("voltage-states") {
                        if let Some(bytes) = iokit::value_bytes(v) {
                            out.push((k, bytes));
                        }
                    }
                }
            }
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
        pairs.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    /// Captured from this machine (M5 Max, macOS 26.5.1, 2026-08-29):
    /// `MCPU00`'s 17-state residency list over a real delta interval.
    fn mcpu00_states() -> Vec<(String, i64)> {
        res(&[
            ("DOWN", 6_118_204),
            ("IDLE", 2_377_534),
            ("V0P14", 19_646),
            ("V1P13", 0),
            ("V2P12", 1_567),
            ("V3P11", 19_522),
            ("V4P10", 20_702),
            ("V5P9", 85_367),
            ("V6P8", 79_123),
            ("V7P7", 38_773),
            ("V8P6", 0),
            ("V9P5", 0),
            ("V10P4", 0),
            ("V11P3", 0),
            ("V12P2", 0),
            ("V13P1", 0),
            ("V14P0", 498_580),
        ])
    }

    /// `voltage-states22-sram` on this machine, already scaled to MHz.
    fn mcpu_freqs() -> Vec<u32> {
        vec![
            1344, 1644, 1992, 2304, 2652, 2964, 3240, 3504, 3696, 3876, 4044, 4176, 4284, 4308,
            4380,
        ]
    }

    #[test]
    fn mean_mhz_weights_residency_by_frequency() {
        let mhz = mean_mhz_from_residency(&mcpu00_states(), &mcpu_freqs()).expect("a frequency");
        // Dominated by V14P0 (the top state, 498_580 of 763_280 active ticks)
        // with a long tail in the mid states — must land inside the table.
        assert!(
            (1344..=4380).contains(&mhz),
            "must be inside the SoC's own table, got {mhz}"
        );
        assert!(mhz > 3000, "the top state holds 65% of active residency; got {mhz}");
    }

    #[test]
    fn mean_mhz_pins_a_single_state_exactly() {
        // All active residency in one state → exactly that state's frequency.
        let states = res(&[("DOWN", 100), ("IDLE", 100), ("V0", 0), ("V1", 500), ("V2", 0)]);
        assert_eq!(mean_mhz_from_residency(&states, &[1000, 2000, 3000]), Some(2000));
    }

    #[test]
    fn mean_mhz_ignores_trailing_states_past_the_table() {
        // The real GPU case: GPUPH reports 15 P-states, the SoC table has 13.
        let mut states = vec![("OFF".to_string(), 9_168_940i64)];
        for i in 1..=15 {
            states.push((format!("P{i}"), if i == 1 { 245_733 } else { 0 }));
        }
        let freqs = vec![338, 486, 636, 796, 888, 988, 1084, 1182, 1278, 1374, 1470, 1578, 1620];
        assert_eq!(
            mean_mhz_from_residency(&states, &freqs),
            Some(338),
            "all active residency is in P1 → the table's first entry"
        );
    }

    #[test]
    fn mean_mhz_reports_the_floor_when_fully_idle() {
        let states = res(&[("DOWN", 1000), ("IDLE", 500), ("V0", 0), ("V1", 0), ("V2", 0)]);
        assert_eq!(
            mean_mhz_from_residency(&states, &[600, 1200, 1800]),
            Some(600),
            "parked at the DVFS floor is a true statement about the interval"
        );
    }

    #[test]
    fn mean_mhz_rejects_a_table_that_does_not_fit_the_channel() {
        let states = res(&[("IDLE", 10), ("V0", 5), ("V1", 5)]);
        // Table longer than the channel's active states → refuse, don't read
        // past the residencies.
        assert_eq!(mean_mhz_from_residency(&states, &[1, 2, 3]), None);
        assert_eq!(mean_mhz_from_residency(&states, &[]), None, "no table ⇒ no frequency");
        assert_eq!(
            mean_mhz_from_residency(&res(&[("IDLE", 10), ("DOWN", 5)]), &[1000]),
            None,
            "no active state at all ⇒ no frequency"
        );
    }

    #[test]
    fn active_state_count_skips_the_inactive_prefix() {
        assert_eq!(active_state_count(&mcpu00_states()), Some(15));
        assert_eq!(active_state_count(&res(&[("OFF", 1), ("P1", 2), ("P2", 3)])), Some(2));
        assert_eq!(active_state_count(&res(&[("IDLE", 1), ("DOWN", 2)])), None);
    }

    #[test]
    fn energy_units_are_matched_against_a_closed_set() {
        // 1000 mJ over 1s = 1000 mW.
        assert_eq!(energy_delta_to_mw(1000, "mJ", 1000), Some(1000.0));
        // GPU Energy is nJ on this machine: 8_069_136 nJ over 500 ms.
        let mw = energy_delta_to_mw(8_069_136, "nJ", 500).expect("nJ is known");
        assert!((mw - 16.138).abs() < 0.001, "got {mw}");
        assert_eq!(energy_delta_to_mw(1_000_000, "uJ", 1000), Some(1000.0));
        assert_eq!(energy_delta_to_mw(1_000_000, "\u{b5}J", 1000), Some(1000.0));
    }

    #[test]
    fn unknown_energy_unit_yields_none_never_a_guessed_scale() {
        assert_eq!(energy_delta_to_mw(1000, "pJ", 1000), None);
        assert_eq!(energy_delta_to_mw(1000, "", 1000), None);
        assert_eq!(energy_delta_to_mw(1000, "J", 1000), None);
        assert_eq!(energy_delta_to_mw(1000, "mW", 1000), None);
    }

    #[test]
    fn energy_rejects_a_counter_reset_and_a_zero_interval() {
        assert_eq!(energy_delta_to_mw(-5, "mJ", 1000), None, "a negative delta is a reset");
        assert_eq!(energy_delta_to_mw(1000, "mJ", 0), None, "no interval ⇒ no rate");
    }

    #[test]
    fn voltage_states_parse_takes_the_first_u32_of_each_8_byte_pair() {
        // Two entries: 1_344_000 and 1_644_000 kHz, each followed by a voltage.
        let mut b = Vec::new();
        b.extend_from_slice(&1_344_000u32.to_le_bytes());
        b.extend_from_slice(&700u32.to_le_bytes());
        b.extend_from_slice(&1_644_000u32.to_le_bytes());
        b.extend_from_slice(&720u32.to_le_bytes());
        assert_eq!(parse_voltage_states(&b), vec![1_344_000, 1_644_000]);
        // A trailing partial chunk is dropped rather than read past.
        b.push(0);
        assert_eq!(parse_voltage_states(&b), vec![1_344_000, 1_644_000]);
    }

    fn blob(vals: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in vals {
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        b
    }

    /// The `pmgr` blobs this machine actually reports (abridged), including
    /// the placeholder and voltage tables the picker must reject.
    fn pmgr_fixture() -> Vec<(String, Vec<u8>)> {
        vec![
            // All-ones placeholder.
            ("voltage-states0".into(), blob(&[1, 1, 1, 1, 1])),
            // CPU voltages (descending) — must not be mistaken for a table.
            (
                "voltage-states22".into(),
                blob(&[48761, 39863, 32899, 28444, 24711]),
            ),
            // The two 15-entry MCPU frequency tables, in kHz.
            (
                "voltage-states22-sram".into(),
                blob(&[
                    1_344_000, 1_644_000, 1_992_000, 2_304_000, 2_652_000, 2_964_000, 3_240_000,
                    3_504_000, 3_696_000, 3_876_000, 4_044_000, 4_176_000, 4_284_000, 4_308_000,
                    4_380_000,
                ]),
            ),
            // The 20-entry PCPU table, in kHz.
            (
                "voltage-states5-sram".into(),
                blob(&[
                    1_308_000, 1_620_000, 1_980_000, 2_292_000, 2_580_000, 2_880_000, 3_180_000,
                    3_432_000, 3_648_000, 3_828_000, 3_984_000, 4_104_000, 4_188_000, 4_236_000,
                    4_284_000, 4_308_000, 4_332_000, 4_428_000, 4_512_000, 4_608_000,
                ]),
            ),
            // The GPU table, in Hz, with its leading OFF entry.
            (
                "voltage-states9".into(),
                blob(&[
                    0, 338_000_000, 486_000_000, 636_000_000, 796_000_000, 888_000_000,
                    988_000_000, 1_084_000_000, 1_182_000_000, 1_278_000_000, 1_374_000_000,
                    1_470_000_000, 1_578_000_000, 1_620_000_000,
                ]),
            ),
        ]
    }

    #[test]
    fn cpu_freq_tables_key_by_length_and_reject_voltages_and_placeholders() {
        let t = cpu_freq_tables(&pmgr_fixture());
        assert_eq!(t.len(), 2, "exactly the two real CPU tables; got {:?}", t.keys());
        assert_eq!(t[&15].first(), Some(&1344));
        assert_eq!(t[&15].last(), Some(&4380));
        assert_eq!(t[&20].first(), Some(&1308));
        assert_eq!(t[&20].last(), Some(&4608));
        assert!(!t.contains_key(&5), "the all-ones placeholder must be rejected");
    }

    #[test]
    fn gpu_freq_table_drops_the_leading_off_entry() {
        let g = gpu_freq_table(&pmgr_fixture());
        assert_eq!(g.len(), 13, "14 entries minus the leading 0");
        assert_eq!(g.first(), Some(&338));
        assert_eq!(g.last(), Some(&1620));
    }

    #[test]
    fn gpu_freq_table_absent_key_is_empty_not_a_guess() {
        let props: Vec<(String, Vec<u8>)> = pmgr_fixture()
            .into_iter()
            .filter(|(k, _)| k != "voltage-states9")
            .collect();
        assert!(gpu_freq_table(&props).is_empty());
    }

    #[test]
    fn channel_prefix_collapses_soc_clusters() {
        assert_eq!(channel_prefix("MCPU00"), "MCPU");
        assert_eq!(channel_prefix("MCPU15"), "MCPU");
        assert_eq!(channel_prefix("PCPU5"), "PCPU");
        assert_eq!(channel_prefix("GPUPH"), "GPUPH");
    }

    #[test]
    fn groups_map_to_perf_levels_by_core_count() {
        // This machine: groups {MCPU: 12, PCPU: 6} (BTreeMap order → MCPU
        // first), levels [Super(6), Performance(12)]. Counts alone
        // disambiguate here (6 ≠ 12), so mean MHz is irrelevant.
        let got = assign_groups_to_levels(&[12, 6], &[3200, 4200], &[6, 12]);
        assert_eq!(got, vec![Some(1), Some(0)], "Super→PCPU(6), Performance→MCPU(12)");
    }

    // (#2108 review finding) A hypothetical 4-Performance + 4-Efficiency
    // chip: two IOReport groups tie on core count, so matching by count
    // ALONE can't tell them apart — the original bug matched whichever
    // same-sized group's NAME sorted first (alphabetical BTreeMap order),
    // regardless of tier, silently swapping Performance's and Efficiency's
    // MHz. BTreeMap-sorted group order is alphabetical → keys = [ECPU,
    // PCPU], sizes = [4, 4], mean_mhz = [1200, 3000]. `levels` is tier
    // order: [Performance(4), Efficiency(4)] (index 0 = highest tier). The
    // fix ties-break by mean MHz: the highest-tier level must get the
    // highest-clocked candidate.
    #[test]
    fn groups_with_equal_counts_are_consumed_once_each() {
        let got = assign_groups_to_levels(&[4, 4], &[1200, 3000], &[4, 4]);
        assert_eq!(
            got,
            vec![Some(1), Some(0)],
            "Performance(4) ← PCPU(3000 MHz), Efficiency(4) ← ECPU(1200 MHz), not the reverse"
        );
    }

    #[test]
    fn groups_with_equal_count_and_equal_mhz_are_left_unassigned() {
        // Count ties AND the clocks tie too — no signal left to
        // disambiguate. Guessing would be wrong half the time; `None`
        // degrades both clusters' mhz to null instead, same as an
        // unmatched count.
        let got = assign_groups_to_levels(&[4, 4], &[1200, 1200], &[4, 4]);
        assert_eq!(got, vec![None, None], "an exact count+clock tie can't be told apart");
    }

    #[test]
    fn a_level_with_no_matching_group_gets_none() {
        let got = assign_groups_to_levels(&[12], &[3200], &[6, 12]);
        assert_eq!(got, vec![None, Some(0)], "the unmatched level degrades to null mhz");
    }

    fn simple(name: &str, unit: &str, value: i64) -> ChannelReading {
        ChannelReading {
            group: "Energy Model".into(),
            subgroup: String::new(),
            name: name.into(),
            unit: unit.into(),
            value,
            states: Vec::new(),
        }
    }

    fn core(name: &str, states: Vec<(String, i64)>) -> ChannelReading {
        ChannelReading {
            group: "CPU Stats".into(),
            subgroup: "CPU Core Performance States".into(),
            name: name.into(),
            unit: "24Mticks".into(),
            value: 0,
            states,
        }
    }

    #[test]
    fn fold_readings_computes_clusters_and_power() {
        let tables = cpu_freq_tables(&pmgr_fixture());
        let gpu = gpu_freq_table(&pmgr_fixture());
        let readings = vec![
            core("MCPU00", mcpu00_states()),
            core("MCPU01", mcpu00_states()),
            simple("CPU Energy", "mJ", 1674),
            simple("GPU Energy", "nJ", 8_069_136),
            simple("ANE", "mJ", 0),
            // Not a rail we report — must be ignored, not summed into CPU.
            simple("DRAM", "mJ", 391),
        ];
        let s = fold_readings(&readings, &tables, &gpu, 1000);
        assert_eq!(s.cluster_cores.get("MCPU"), Some(&2));
        assert!(s.cluster_mhz.contains_key("MCPU"));
        assert_eq!(s.cpu_mw, Some(1674.0), "1674 mJ over 1s");
        assert_eq!(s.ane_mw, Some(0.0));
        let gpu_mw = s.gpu_mw.expect("gpu rail");
        assert!((gpu_mw - 8.069).abs() < 0.001, "got {gpu_mw}");
    }

    #[test]
    fn fold_readings_sums_multi_die_rails() {
        // An Ultra part reports one CPU Energy channel per die.
        let readings = vec![
            simple("DIE_0_CPU Energy", "mJ", 1000),
            simple("DIE_1_CPU Energy", "mJ", 500),
            simple("ANE0", "mJ", 10),
            simple("ANE1", "mJ", 5),
        ];
        let s = fold_readings(&readings, &BTreeMap::new(), &[], 1000);
        assert_eq!(s.cpu_mw, Some(1500.0), "both dies sum into one CPU rail");
        assert_eq!(s.ane_mw, Some(15.0));
    }

    #[test]
    fn fold_readings_leaves_rails_none_when_the_unit_is_unknown() {
        let readings = vec![simple("CPU Energy", "pJ", 1000)];
        let s = fold_readings(&readings, &BTreeMap::new(), &[], 1000);
        assert_eq!(s.cpu_mw, None, "an unknown unit must not become a guessed wattage");
    }

    #[test]
    fn fold_readings_leaves_mhz_absent_when_no_table_fits() {
        let readings = vec![core("MCPU00", mcpu00_states())];
        // Tables present, but none with 15 entries.
        let mut tables = BTreeMap::new();
        tables.insert(3usize, vec![1000u32, 2000, 3000]);
        let s = fold_readings(&readings, &tables, &[], 1000);
        assert!(s.cluster_mhz.is_empty(), "no fitting table ⇒ no frequency claim");
    }
}
