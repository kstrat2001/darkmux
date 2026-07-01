//! (#557 slice 4) Always-on lms + container-CPU telemetry sampler.
//!
//! While an internal-runtime dispatch's container runs, a background
//! thread (spawned in `dispatch_internal::dispatch`, alongside the
//! trajectory tailer) samples two host-side surfaces on a fixed cadence
//! and forwards each observation into the one flow stream as a
//! `category=telemetry` record the observability viewer renders:
//!
//! - `source="lms"` → model load/unload deltas, derived from
//!   `darkmux_profiles::lms::list_loaded()` (the lms-ps source). Wire
//!   payload shapes the served demo viewer consumes:
//!   `{event:"load", model:<id>, gb:<N>}` (load) /
//!   `{event:"unload", model:<id>}` (unload — no `gb`).
//! - `source="process"` → the per-dispatch `darkmux-runtime` container's
//!   CPU%, via `docker stats <name> --no-stream`. Wire payload:
//!   `{cpu:<N>}` (integer percent).
//!
//! ALWAYS-ON: cross-layer telemetry is captured automatically, never
//! behind a flag. The `source:process` CPU signal is sourced from the
//! per-dispatch container. (It originally replaced an OpenClaw-gateway CPU
//! sampler that returned `None` on internal-runtime dispatches; the
//! lab-side `instrument.rs` sidecar and the `--instrument` flag it lived
//! behind were retired in #557.)
//!
//! This module holds the two PURE, unit-testable helpers (`lms_diff`,
//! `parse_cpu_percent`). The live sampler thread (which calls
//! `list_loaded` + shells out to `docker stats`) lives in
//! `dispatch_internal.rs` next to the tailer + watchdog it mirrors.

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

/// Parse a `docker stats --format "{{.CPUPerc}}"` value into an integer
/// CPU percent. `"38.00%"` → `Some(38)`, `"0.00%"` → `Some(0)`,
/// `"71.6%"` → `Some(72)` (rounded). `"--"` (docker's placeholder for a
/// just-started / exited container), the empty string, and any other
/// garbage → `None`. Pure — unit-testable without a real container.
pub(crate) fn parse_cpu_percent(s: &str) -> Option<u64> {
    let trimmed = s.trim().trim_end_matches('%').trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok().map(|v| v.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_cpu_percent_handles_values_and_garbage() {
        assert_eq!(parse_cpu_percent("38.00%"), Some(38));
        assert_eq!(parse_cpu_percent("0.00%"), Some(0));
        assert_eq!(parse_cpu_percent("71.6%"), Some(72), "rounds to nearest");
        assert_eq!(parse_cpu_percent("--"), None);
        assert_eq!(parse_cpu_percent(""), None);
    }
}
