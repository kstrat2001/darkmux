//! Golden test (4.0, one token truth): an older run directory still reports
//! its totals, through the one legacy path (`lab::inspect::run_trajectory`,
//! a fold of the run's trajectory).
//!
//! Each fixture under `tests/fixtures/legacy-runs/` is a real pre-4.0 run
//! directory, reduced and scrubbed: ids replaced with fake ones, tracker keys
//! and internal names replaced with placeholders, home paths
//! rewritten, long strings cut, and a stream's intermediate `model.partial`
//! lines dropped (the fold keeps only a stream's largest cumulative count).
//! `expected.json` was computed from the fixture's trajectory with `jq`,
//! independently of the fold.
//!
//! - `stale-metrics`: a killed internal-runtime run whose `metrics.json` is
//!   another run's copy (9 turns, 175,557 prompt tokens). The trajectory
//!   says 27 turns; the stale file must not be read.
//! - `openclaw`: a run of the retired openclaw runtime (#1405), whose turns
//!   are `prompt.submitted` events, whose compactions are distinct
//!   summaries inside the thread, and whose tokens are its completions'
//!   `data.usage` (input/output/total, under an ISO-string clock).

use darkmux_lab::lab::inspect::run_trajectory;
use darkmux_lab::lab::stats::compute_from_dir;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-runs").join(name)
}

fn expected(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap()).unwrap()
}

fn totals(dir: &Path) -> serde_json::Value {
    let fold = run_trajectory(dir);
    serde_json::json!({
        "turns": fold.turns(),
        "compactions": fold.compactions(),
        "prompt_tokens": fold.tokens.prompt,
        "completion_tokens": fold.tokens.completion,
        "total_tokens": fold.tokens.total,
        "rest_ms": fold.rest_ms(),
    })
}

#[test]
fn a_run_with_a_stale_metrics_json_reports_its_trajectory_totals() {
    let dir = fixture("stale-metrics");
    assert!(dir.join("metrics.json").exists(), "the fixture keeps the stale file it must ignore");
    assert_eq!(totals(&dir), expected(&dir));
    let flows = tempfile::TempDir::new().unwrap();
    let stats = compute_from_dir(&dir, flows.path()).unwrap();
    let want = expected(&dir);
    assert_eq!(stats.turns, want["turns"].as_u64().unwrap(), "run stats reads the same fold");
    assert_eq!(stats.completion_tokens, want["completion_tokens"].as_u64().unwrap());
    assert_eq!(stats.rest_ms, want["rest_ms"].as_u64().unwrap());
}

#[test]
fn an_openclaw_run_reports_its_prompts_summaries_and_tokens() {
    let dir = fixture("openclaw");
    assert_eq!(totals(&dir), expected(&dir));
}
