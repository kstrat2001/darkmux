//! (#557 slice 4 · #1064) Always-on lms + host-load telemetry sampler.
//!
//! While an internal-runtime dispatch runs, a background thread (spawned
//! in `dispatch_internal::dispatch`, alongside the trajectory tailer)
//! samples two surfaces on a fixed cadence and forwards each observation
//! into the one flow stream as a `category=telemetry` record the
//! observability viewer renders:
//!
//! - `source="lms"` → model load/unload deltas, derived from
//!   `darkmux_profiles::lms::list_loaded()` (the lms-ps source). Wire
//!   payload shapes the served demo viewer consumes:
//!   `{event:"load", model:<id>, gb:<N>}` (load) /
//!   `{event:"unload", model:<id>}` (unload — no `gb`).
//! - `source="process"` → the HOST system load: CPU%, RAM used%, GPU util%,
//!   read in-process through `crate::host_probe` (#2108 — mach tick
//!   counters, `host_statistics64`, and the `IOAccelerator` IORegistry
//!   node; before that, four shell-outs costing ~780 ms). Wire payload:
//!   `{cpu, mem, gpu}` (integer %, each best-effort / omitted-on-failure).
//!   The per-dispatch container is NOT sampled — inference runs in
//!   LMStudio off-container, so container CPU reads ~0 (#814/#1064).
//!
//! ALWAYS-ON: cross-layer telemetry is captured automatically, never
//! behind a flag. (The `source:process` signal originally sampled the
//! per-dispatch container's CPU via `docker stats`; #1064 moved it to the
//! host system because container CPU answered the wrong question. Further
//! back it replaced an OpenClaw-gateway CPU sampler; the lab-side
//! `instrument.rs` sidecar + `--instrument` flag were retired in #557.)
//!
//! This module holds the PURE, unit-testable helpers (`lms_diff`,
//! `mem_percent_from_vm_stat`) plus [`sample_host`], the one IMPURE entry
//! point, and the [`reduce_host_stats`] window reduction. (#2108 removed
//! `host_cpu_percent_from_top`, `gpu_percent_from_ioreg` and the `run_ok`
//! shell-out helper along with the commands they parsed;
//! `mem_percent_from_vm_stat` survives as the REFERENCE DEFINITION of
//! darkmux's memory-pressure semantics — `host_probe::mach_cpu::mem_pct`
//! reproduces exactly this number from kernel counters, and
//! `darkmux-profiles`' `gestalt_host::mac_probe` cites it for the same
//! reason.) `sample_host` is `pub`
//! (not `pub(crate)`) so a second sampler thread outside this crate can
//! reuse the exact host-reading mechanism instead of re-deriving it —
//! `darkmux-lab`'s review driver does this (#1247 doctrine surface) to
//! sample host load during review runs, which bypass
//! `dispatch_internal` entirely and previously had no host telemetry at
//! all. The live lms + host sampler THREAD (which additionally diffs
//! `list_loaded()` snapshots and owns the stop-flag/poll loop) still lives
//! in `dispatch_internal.rs` next to the tailer + watchdog it mirrors;
//! only the host-reading mechanism is shared here.

use darkmux_types::LoadedModel;

/// Compare two loaded-model snapshots and emit one telemetry payload per
/// change. Comparison key is `LoadedModel::model` — the bare LMStudio
/// model id (modelKey-derived), which matches the `model` field the demo
/// viewer renders.
///
/// Each model in `cur` not present in `prev` (by `model`) yields a
/// `{"event":"load","model":<id>,"gb":<gb>}` payload, where `gb` is the
/// model's size parsed out of the formatted `LoadedModel::size` string
/// (e.g. `"21.00 GB"` → `21`); a model whose size doesn't parse emits
/// `gb:0`. Each model in `prev` not present in `cur` yields a
/// `{"event":"unload","model":<id>}` payload (no `gb` on unload — matches
/// the viewer's expectation).
///
/// Empty when `prev` and `cur` carry the same set of model ids. Pure:
/// no IO, no global sink, so the load/unload-diff rule is unit-testable
/// without touching LMStudio.
///
/// `pub` (not `pub(crate)`) so `darkmux-lab`'s review driver can reuse it
/// too (#1247 doctrine surface, mirroring [`sample_host`]'s reuse) — the
/// review pipeline's own `HostTelemetrySampler` needs the exact same
/// load/unload-diff rule to emit `telemetry.lms` records, not just the
/// host cpu/mem/gpu family this module already shares.
pub fn lms_diff(prev: &[LoadedModel], cur: &[LoadedModel]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();

    // Loads: in `cur`, not in `prev`.
    for m in cur {
        if !prev.iter().any(|p| p.model == m.model) {
            out.push(serde_json::json!({
                "event": "load",
                "model": m.model,
                "gb": gb_from_size_string(&m.size),
            }));
        }
    }
    // Unloads: in `prev`, not in `cur`.
    for p in prev {
        if !cur.iter().any(|m| m.model == p.model) {
            out.push(serde_json::json!({
                "event": "unload",
                "model": p.model,
            }));
        }
    }

    out
}

/// Parse the integer-GB size out of a `LoadedModel::size` string. The
/// loaded-model wrapper formats sizes as decimal GB (e.g. `"21.00 GB"`,
/// `"4.50 GB"`); we take the leading float token and round to the nearest
/// integer. Unparseable / empty strings yield `0` — the viewer renders
/// `gb || '?'`, so a 0 reads as "unknown" rather than crashing the diff.
fn gb_from_size_string(size: &str) -> u64 {
    size.split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<f64>().ok())
        .map(|gb| gb.round() as u64)
        .unwrap_or(0)
}

/// Parse host **RAM used%** out of `vm_stat` output plus the machine's total
/// bytes (from `sysctl -n hw.memsize`). Available ≈ (`Pages free` +
/// `Pages inactive` + `Pages speculative`) × page-size — inactive + speculative
/// (read-ahead cache) pages are reclaimable on macOS, so counting them as
/// available yields the memory-*pressure* number the operator wants (not an
/// inflated "used" that folds in reclaimable cache). `Pages speculative` is
/// optional (absent on some builds → treated as 0).
/// `used% = 100 * (total - avail) / total`. The page size is read from
/// vm_stat's own header (`page size of N bytes`), defaulting to 16384 on
/// Apple Silicon. `None` if total is 0 or the page fields are missing. Pure.
/// (#1064)
pub fn mem_percent_from_vm_stat(vm_stat: &str, total_bytes: u64) -> Option<u64> {
    if total_bytes == 0 {
        return None;
    }
    let page = vm_stat
        .lines()
        .next()
        .and_then(|l| l.split("page size of").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(16384);
    let field = |name: &str| -> Option<u64> {
        vm_stat
            .lines()
            .find(|l| l.trim_start().starts_with(name))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
    };
    // `Pages speculative` (read-ahead cache) are also reclaimable, so count them
    // as available when present — tracks the real pressure number closer than
    // free+inactive alone. Optional: `unwrap_or(0)` if the line is absent.
    let avail = field("Pages free")?
        .saturating_add(field("Pages inactive")?)
        .saturating_add(field("Pages speculative").unwrap_or(0))
        .saturating_mul(page);
    let used = total_bytes.saturating_sub(avail);
    Some(((used as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0).round() as u64)
}

/// One host-load reading — CPU/RAM/GPU utilization%, each best-effort and
/// independently `None` on failure. See [`sample_host`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostSample {
    pub cpu: Option<u64>,
    pub mem: Option<u64>,
    pub gpu: Option<u64>,
}

/// Read one host-load sample.
///
/// **(#2108) The mechanism underneath changed; the signature did not.** This
/// used to spawn `top -l 1 -n 0`, `sysctl -n hw.memsize`, `vm_stat` and
/// `ioreg -r -d 1 -c IOAccelerator` — four processes, ~780 ms, and a CPU
/// number that was a SINCE-BOOT average rather than a reading of the
/// interval (`top -l 1`'s first and only sample always is, which made every
/// CPU figure darkmux recorded before #2108 both expensive and wrong-ish).
/// It now reads [`crate::host_probe::HostProbe`], which takes the same three
/// numbers from mach kernel counters and the IORegistry in ~5-10 ms, with
/// `cpu` a TRUE mean over the interval since the previous call.
///
/// Two consequences worth knowing at the call sites:
///
/// - **`cpu` is `None` on the process's FIRST call.** A tick-counter delta
///   needs two reads. Every subsequent call has one. `mem`/`gpu` are
///   unaffected (neither is a delta).
/// - **The probe is process-wide and `Mutex`-guarded**, so `cpu` is the mean
///   since whoever called last — which, for a single sampler thread, is
///   exactly its own cadence. A caller wanting a private interval (and the
///   per-cluster frequency / power / thermal fields this triple cannot
///   carry) owns a [`crate::host_probe::HostProbe`] directly, as
///   `dispatch_internal`'s and `darkmux-serve`'s samplers now do.
///
/// `pub` (not `pub(crate)`) for the same reason as before: `darkmux-lab`'s
/// review driver (#1247 doctrine surface) samples host load through the
/// exact same mechanism rather than re-deriving it.
pub fn sample_host() -> HostSample {
    static PROBE: std::sync::OnceLock<std::sync::Mutex<crate::host_probe::HostProbe>> =
        std::sync::OnceLock::new();
    let mut guard = match PROBE
        .get_or_init(|| std::sync::Mutex::new(crate::host_probe::HostProbe::new()))
        .lock()
    {
        Ok(g) => g,
        // A panic in another caller must not take the sampler down with it —
        // the probe's state is just counter snapshots, so the worst a
        // poisoned lock costs is one stale delta.
        Err(poisoned) => poisoned.into_inner(),
    };
    let s = guard.sample();
    HostSample {
        cpu: s.cpu_pct,
        mem: s.mem_pct,
        gpu: s.gpu_pct,
    }
}

/// (#2107, moved from `dispatch_internal.rs` where it originated) One
/// host-load reading, timestamped relative to whichever clock the caller's
/// sampler uses — `dispatch_internal`'s dispatch-scoped sampler uses a
/// clock relative to its own start (`Instant::now()` since the sampler
/// spawned); `darkmux-serve`'s daemon-side continuous sampler (#2107,
/// #1833 — the machine stats drawer's live feed) uses wall-clock epoch ms
/// instead, since it has no single dispatch start to be relative TO. Both
/// are valid: [`reduce_metric`]'s `above_80_ms` duty measure only needs the
/// GAP between consecutive `at_ms` values, and that gap is identical
/// whether the clock's zero point is a dispatch start or the Unix epoch.
/// `cpu`/`mem`/`gpu` stay `Option`: each is independently best-effort per
/// tick (see [`sample_host`]'s own doc), so one metric's read failing on a
/// given tick must not corrupt another's.
#[derive(Debug, Clone, Copy)]
pub struct HostSampleAt {
    pub at_ms: u64,
    pub cpu: Option<u64>,
    pub mem: Option<u64>,
    pub gpu: Option<u64>,
}

/// (#2107) One metric's (cpu, mem, or gpu) reduction over a set of samples.
///
/// `mean_pct` is rounded to ONE DECIMAL — enough resolution to distinguish
/// "sat near idle" from "sat near saturated" across a run with only a
/// handful of samples, without asserting false precision the sample
/// cadence never measured. `p95_pct` uses the nearest-rank method
/// (`ceil(0.95 * n)`th smallest value, 1-indexed) — the standard convention
/// for a small, unweighted sample set. `above_80_ms` is a DUTY measure — the
/// wall-clock time this metric's own consecutive samples spent above 80%,
/// using each sample's OWN measured gap to the next (see [`HostSampleAt`]),
/// not a synthetic `samples_above_80 × <nominal interval>` count that would
/// silently assume a constant cadence.
#[derive(Default, Debug, Clone, Copy)]
pub struct MetricStats {
    pub peak_pct: Option<u64>,
    pub mean_pct: Option<f64>,
    pub p95_pct: Option<u64>,
    pub above_80_ms: u64,
}

/// (#1955, revised #2107) The host-telemetry reduction. Originated as the
/// dispatch-envelope's `host` block reduction; #2107/#1833 additionally
/// reuses it — unmodified — for the daemon-side continuous sampler's
/// `/machine/resources` `load.window` block, so the two surfaces can never
/// silently disagree on what "peak"/"mean"/"p95" mean.
///
/// This is a pure REDUCTION over samples that are already being taken — it
/// adds no probe, no syscall, and no cost to the measured system, which is
/// what the observer-must-not-join-the-observed rule (CLAUDE.md) requires
/// of anything on this path.
///
/// `samples` is carried deliberately: a peak of 0 and "we never sampled" are
/// different claims. `sample_interval_ms` is the MEASURED mean gap between
/// ticks (not a nominal configured cadence) — `None` when fewer than two
/// samples landed, since an interval needs two points.
#[derive(Default, Debug, Clone, Copy)]
pub struct HostStats {
    pub cpu: MetricStats,
    pub mem: MetricStats,
    pub gpu: MetricStats,
    pub samples: u32,
    pub sample_interval_ms: Option<u64>,
}

/// Reduce one metric's `(at_ms, pct)` readings — already filtered to the
/// ticks where THIS metric actually read a value — into peak/mean/p95/duty.
/// `pairs` must be in chronological order (the order ticks were taken);
/// empty input yields an all-`None`/`0` [`MetricStats`], the same "not
/// measured" default [`HostStats`] uses.
pub fn reduce_metric(pairs: &[(u64, u64)]) -> MetricStats {
    if pairs.is_empty() {
        return MetricStats::default();
    }
    let peak_pct = pairs.iter().map(|(_, v)| *v).max();
    let sum: u64 = pairs.iter().map(|(_, v)| *v).sum();
    let mean_pct = Some((sum as f64 / pairs.len() as f64 * 10.0).round() / 10.0);
    let p95_pct = {
        let mut sorted: Vec<u64> = pairs.iter().map(|(_, v)| *v).collect();
        sorted.sort_unstable();
        // Nearest-rank: the `ceil(0.95 * n)`th smallest (1-indexed), clamped
        // into range so a single-sample list resolves to that sample.
        let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[idx])
    };
    // Duty: for each consecutive pair, the FIRST sample's value is assumed
    // to hold until the next one is taken (left-Riemann) — the only honest
    // assumption between two point-in-time readings. A trailing sample past
    // 80% with no successor to bound it contributes nothing (there is no
    // measured interval to attribute to it), which undercounts a run that
    // was cut off mid-spike rather than overcounting one that wasn't.
    let above_80_ms = pairs
        .windows(2)
        .filter(|w| w[0].1 > 80)
        .map(|w| w[1].0.saturating_sub(w[0].0))
        .sum();
    MetricStats { peak_pct, mean_pct, p95_pct, above_80_ms }
}

/// Reduce a full set of host readings into [`HostStats`]. Pure and
/// independently testable — a scripted list of [`HostSampleAt`] in, exact
/// stats out, with no sampler thread / clock / shell-out involved.
pub fn reduce_host_stats(raw: &[HostSampleAt]) -> HostStats {
    let samples = raw.len() as u32;
    let sample_interval_ms = if raw.len() >= 2 {
        let span = raw.last().unwrap().at_ms.saturating_sub(raw.first().unwrap().at_ms);
        Some(span / (raw.len() as u64 - 1))
    } else {
        None
    };
    let pairs_of = |get: fn(&HostSampleAt) -> Option<u64>| -> Vec<(u64, u64)> {
        raw.iter().filter_map(|s| get(s).map(|v| (s.at_ms, v))).collect()
    };
    HostStats {
        cpu: reduce_metric(&pairs_of(|s| s.cpu)),
        mem: reduce_metric(&pairs_of(|s| s.mem)),
        gpu: reduce_metric(&pairs_of(|s| s.gpu)),
        samples,
        sample_interval_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one test that exercises the REAL `sample_host()` reads —
    /// macOS-gated because every source it touches (mach CPU/VM counters,
    /// the `IOAccelerator` IORegistry node) is macOS-only; elsewhere every
    /// field is `None`, which would make this assert meaningless.
    /// Consumers that need `sample_host` in a cross-platform test (e.g.
    /// `darkmux-lab`'s review telemetry tests) inject a fake sampling
    /// function instead — this test is where the real path keeps its
    /// coverage.
    ///
    /// TWO calls, deliberately: `cpu` is a tick-counter DELTA (#2108), so
    /// the first call in a process can only report `mem`/`gpu`. Asserting
    /// on the second call is what proves the delta path works rather than
    /// just the seeding path. Costs ~10 ms now (it was ~780 ms).
    #[test]
    #[cfg(target_os = "macos")]
    #[serial_test::serial]
    fn sample_host_reads_at_least_one_field_on_macos() {
        let first = sample_host();
        assert!(
            first.mem.is_some() || first.gpu.is_some(),
            "the seeding call still reports the non-delta fields; got {first:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        let s = sample_host();
        assert!(
            s.cpu.is_some() || s.mem.is_some() || s.gpu.is_some(),
            "on macOS at least one of cpu/mem/gpu must read successfully; got {s:?}"
        );
        // Values are percentages — each present field must be in range (the
        // readers clamp, so a violation means a clamp was lost).
        for v in [s.cpu, s.mem, s.gpu].into_iter().flatten() {
            assert!(v <= 100, "percent field out of range: {v}");
        }
    }

    fn loaded(model: &str, size: &str) -> LoadedModel {
        LoadedModel {
            identifier: format!("darkmux:{model}"),
            model: model.to_string(),
            status: "loaded".to_string(),
            size: size.to_string(),
            context: 32_768,
        }
    }

    #[test]
    fn lms_diff_emits_load_and_unload_on_change() {
        // prev=[A,B], cur=[B,C] → one `load` C (with gb) + one `unload` A.
        let prev = vec![loaded("A", "10.00 GB"), loaded("B", "20.00 GB")];
        let cur = vec![loaded("B", "20.00 GB"), loaded("C", "19.40 GB")];
        let diff = lms_diff(&prev, &cur);
        assert_eq!(diff.len(), 2, "exactly one load + one unload; got {diff:?}");

        let load = diff
            .iter()
            .find(|p| p["event"] == "load")
            .expect("a load event");
        assert_eq!(load["model"], "C");
        assert_eq!(load["gb"], 19, "19.40 rounds down to 19");

        let unload = diff
            .iter()
            .find(|p| p["event"] == "unload")
            .expect("an unload event");
        assert_eq!(unload["model"], "A");
        assert!(unload.get("gb").is_none(), "unload carries no gb field");
    }

    #[test]
    fn lms_diff_empty_when_unchanged() {
        // prev == cur (by model id) → no events. Sizes can differ without
        // emitting — we key on the model id, not the size.
        let prev = vec![loaded("A", "10.00 GB"), loaded("B", "20.00 GB")];
        let cur = vec![loaded("A", "10.00 GB"), loaded("B", "20.00 GB")];
        assert!(lms_diff(&prev, &cur).is_empty(), "no change ⇒ no events");
    }

    #[test]
    fn lms_diff_first_load_from_empty() {
        // prev=[] cur=[A] → one load A (the seed-from-empty case).
        let prev: Vec<LoadedModel> = vec![];
        let cur = vec![loaded("A", "21.00 GB")];
        let diff = lms_diff(&prev, &cur);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0]["event"], "load");
        assert_eq!(diff[0]["model"], "A");
        assert_eq!(diff[0]["gb"], 21);
    }

    #[test]
    fn lms_diff_from_empty_emits_every_resident_model_as_a_baseline_load() {
        // The dispatch's first sample diffs the resident stack against an empty
        // prev (the "no telemetry yet" fix): every resident model — the selected
        // primary AND the compactor — surfaces as a baseline load so the model
        // section reflects what's serving the run.
        let cur = vec![loaded("primary", "18.00 GB"), loaded("compactor", "2.00 GB")];
        let diff = lms_diff(&[], &cur);
        assert_eq!(diff.len(), 2, "both resident models emit as loads; got {diff:?}");
        assert!(diff.iter().all(|p| p["event"] == "load"));
        let models: std::collections::HashSet<&str> =
            diff.iter().map(|p| p["model"].as_str().unwrap()).collect();
        assert!(models.contains("primary") && models.contains("compactor"));
    }



    #[test]
    fn mem_percent_from_vm_stat_computes_pressure() {
        // page size 16384; total = 137_438_953_472 (128 GiB).
        // free=2_000_000 + inactive=2_500_000 = 4_500_000 pages avail
        //   × 16384 = 73_728_000_000 bytes avail
        // used = 137_438_953_472 - 73_728_000_000 = 63_710_953_472
        // used% = 46.35 → 46
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                  Pages free:                             2000000.\n\
                  Pages active:                           3000000.\n\
                  Pages inactive:                         2500000.\n\
                  Pages wired down:                        500000.\n";
        assert_eq!(mem_percent_from_vm_stat(vm, 137_438_953_472), Some(46));
    }

    #[test]
    fn mem_percent_from_vm_stat_counts_speculative_as_available() {
        // Same totals as above + 1_000_000 speculative (read-ahead cache) pages,
        // which are reclaimable and should reduce the pressure number:
        // avail = (2_000_000 + 2_500_000 + 1_000_000) × 16384 = 90_112_000_000
        // used = 137_438_953_472 - 90_112_000_000 = 47_326_953_472 → 34.4% → 34
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                  Pages free:                             2000000.\n\
                  Pages active:                           3000000.\n\
                  Pages speculative:                      1000000.\n\
                  Pages inactive:                         2500000.\n\
                  Pages wired down:                        500000.\n";
        assert_eq!(
            mem_percent_from_vm_stat(vm, 137_438_953_472),
            Some(34),
            "speculative counts as available → lower used% than free+inactive alone"
        );
    }

    #[test]
    fn mem_percent_from_vm_stat_handles_zero_total_and_missing_fields() {
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                  Pages free:                             2000000.\n\
                  Pages inactive:                         2500000.\n";
        assert_eq!(mem_percent_from_vm_stat(vm, 0), None, "zero total → None");
        assert_eq!(
            mem_percent_from_vm_stat("Pages free: 100.\n", 1_000_000),
            None,
            "no inactive line → None"
        );
    }


}
