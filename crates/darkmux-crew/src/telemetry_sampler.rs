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
//! - `source="process"` → the HOST system load: CPU% (`top`), RAM used%
//!   (`vm_stat` + `sysctl -n hw.memsize`), GPU util% (`ioreg`
//!   IOAccelerator `"Device Utilization %"`). Wire payload:
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
//! This module holds the PURE, unit-testable parse helpers (`lms_diff`,
//! `host_cpu_percent_from_top`, `mem_percent_from_vm_stat`,
//! `gpu_percent_from_ioreg`) plus [`sample_host`], the one IMPURE entry
//! point that actually shells out to `top`/`vm_stat`/`sysctl`/`ioreg` and
//! feeds their output through the parsers above. `sample_host` is `pub`
//! (not `pub(crate)`) so a second sampler thread outside this crate can
//! reuse the exact host-reading mechanism instead of re-deriving it —
//! `darkmux-lab`'s funnel driver does this (#1247 doctrine surface) to
//! sample host load during review-funnel runs, which bypass
//! `dispatch_internal` entirely and previously had no host telemetry at
//! all. The live lms + host sampler THREAD (which additionally diffs
//! `list_loaded()` snapshots and owns the stop-flag/poll loop) still lives
//! in `dispatch_internal.rs` next to the tailer + watchdog it mirrors;
//! only the host-reading mechanism is shared here.

use darkmux_types::LoadedModel;
use std::process::Command;

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
pub(crate) fn lms_diff(prev: &[LoadedModel], cur: &[LoadedModel]) -> Vec<serde_json::Value> {
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

/// Parse host **system CPU%** out of `top -l 1 -n 0` output. The header line
/// `CPU usage: 3.82% user, 7.30% sys, 89.13% idle` → `100 - round(idle)` = 11.
/// Locates the `idle` token and reads the percent immediately before it, so
/// format shuffles (user/sys order) don't matter. `None` if the line or the
/// idle token is absent. Pure — unit-testable without shelling `top`. (#1064)
pub(crate) fn host_cpu_percent_from_top(top: &str) -> Option<u64> {
    let line = top.lines().find(|l| l.contains("CPU usage:"))?;
    let toks: Vec<&str> = line.split_whitespace().collect();
    let idle_pos = toks.iter().position(|t| t.eq_ignore_ascii_case("idle"))?;
    let num = toks.get(idle_pos.checked_sub(1)?)?.trim_end_matches('%');
    let idle = num.parse::<f64>().ok()?;
    Some((100.0 - idle).clamp(0.0, 100.0).round() as u64)
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
pub(crate) fn mem_percent_from_vm_stat(vm_stat: &str, total_bytes: u64) -> Option<u64> {
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

/// Parse host **GPU utilization%** out of `ioreg -r -d 1 -c IOAccelerator`
/// output. The `PerformanceStatistics` dict carries `"Device Utilization %"=N`;
/// a machine can expose more than one accelerator node, so take the MAX across
/// all of them. `None` when the field is absent (non-Apple-Silicon, or the key
/// isn't present). Unprivileged — no `sudo`/`powermetrics` (that deeper path is
/// #2). Pure — unit-testable without shelling `ioreg`. (#1064)
pub(crate) fn gpu_percent_from_ioreg(ioreg: &str) -> Option<u64> {
    let mut best: Option<u64> = None;
    for chunk in ioreg.split("\"Device Utilization %\"=").skip(1) {
        let digits: String = chunk
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(v) = digits.parse::<u64>() {
            best = Some(best.map_or(v, |b| b.max(v)));
        }
    }
    best.map(|v| v.min(100))
}

/// Run `cmd`, returning its stdout as a lossy UTF-8 string on a zero exit,
/// `None` on any spawn/exit failure. Keeps the host-load sampler's three
/// best-effort reads terse. (#1064; moved from `dispatch_internal.rs` when
/// [`sample_host`] was extracted so both sampler call sites — the
/// dispatch-internal thread and `darkmux-lab`'s funnel driver — share one
/// copy of the shell-out mechanism.)
fn run_ok(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One host-load reading — CPU/RAM/GPU utilization%, each best-effort and
/// independently `None` on failure. See [`sample_host`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostSample {
    pub cpu: Option<u64>,
    pub mem: Option<u64>,
    pub gpu: Option<u64>,
}

/// Shell out to `top`/`sysctl`+`vm_stat`/`ioreg` and parse one host-load
/// reading. Unprivileged, best-effort per field (a failed read yields
/// `None` for that field only, never an `Err` — see the individual parse
/// helpers' docs for the exact commands and formats). Extracted out of
/// `dispatch_internal::run_telemetry_sampler` (#1064's original site) so
/// the funnel driver's sampler (#1247 doctrine surface) reads host load
/// through the exact same mechanism rather than re-deriving it — the two
/// sampler THREADS differ (poll cadence, stop-flag ownership, sink), but
/// "what a host sample looks like" is one function.
pub fn sample_host() -> HostSample {
    let cpu = run_ok(Command::new("top").args(["-l", "1", "-n", "0"])).and_then(|s| host_cpu_percent_from_top(&s));
    let mem = run_ok(Command::new("sysctl").args(["-n", "hw.memsize"]))
        .and_then(|s| s.trim().parse::<u64>().ok())
        .and_then(|total| run_ok(&mut Command::new("vm_stat")).and_then(|s| mem_percent_from_vm_stat(&s, total)));
    let gpu = run_ok(Command::new("ioreg").args(["-r", "-d", "1", "-c", "IOAccelerator"]))
        .and_then(|s| gpu_percent_from_ioreg(&s));
    HostSample { cpu, mem, gpu }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one test that exercises the REAL `sample_host()` shell-outs —
    /// macOS-gated because every command it runs (`top -l`, `vm_stat`,
    /// `sysctl hw.memsize`, `ioreg`) is macOS-only; on Linux the shells
    /// all fail and every field is `None`, which would make this assert
    /// meaningless there. Consumers that need `sample_host` in a
    /// cross-platform test (e.g. `darkmux-lab`'s funnel telemetry tests)
    /// inject a fake sampling function instead — this test is where the
    /// real path keeps its coverage. Costs one real `top -l 1` call
    /// (~600-900ms); kept to a single invocation for that reason.
    #[test]
    #[cfg(target_os = "macos")]
    fn sample_host_reads_at_least_one_field_on_macos() {
        let s = sample_host();
        assert!(
            s.cpu.is_some() || s.mem.is_some() || s.gpu.is_some(),
            "on macOS at least one of cpu/mem/gpu must read successfully; got {s:?}"
        );
        // Parsed values are percentages — each present field must be in
        // range (the parsers clamp, so a violation means a parser change
        // broke the clamp).
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
    fn host_cpu_percent_from_top_reads_idle() {
        let top = "Processes: 700 total\n\
                   2026/07/03 10:00:00\nLoad Avg: 3.2, 3.0, 2.9\n\
                   CPU usage: 3.82% user, 7.30% sys, 89.13% idle\n\
                   SharedLibs: 500M resident\n";
        assert_eq!(host_cpu_percent_from_top(top), Some(11), "100 - 89.13 = 10.87 → 11");
    }

    #[test]
    fn host_cpu_percent_from_top_handles_missing_and_saturated() {
        assert_eq!(host_cpu_percent_from_top("no cpu line here"), None);
        // 0% idle → fully busy; clamps at 100.
        assert_eq!(
            host_cpu_percent_from_top("CPU usage: 60.00% user, 40.00% sys, 0.00% idle"),
            Some(100)
        );
        // 100% idle → nothing busy.
        assert_eq!(
            host_cpu_percent_from_top("CPU usage: 0.00% user, 0.00% sys, 100.00% idle"),
            Some(0)
        );
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

    #[test]
    fn gpu_percent_from_ioreg_takes_max_across_nodes() {
        let ioreg = "  | \"PerformanceStatistics\" = {\"Tiler Utilization %\"=3,\"Device Utilization %\"=4,\"SplitSceneCount\"=0}\n\
                      +-o AGXAccelerator\n\
                      | \"PerformanceStatistics\" = {\"Device Utilization %\"=87,\"recoveryCount\"=0}\n";
        assert_eq!(gpu_percent_from_ioreg(ioreg), Some(87), "max of 4 and 87");
    }

    #[test]
    fn gpu_percent_from_ioreg_absent_is_none() {
        assert_eq!(gpu_percent_from_ioreg("no accelerator stats here"), None);
        assert_eq!(gpu_percent_from_ioreg("\"Device Utilization %\"=0"), Some(0));
    }
}
