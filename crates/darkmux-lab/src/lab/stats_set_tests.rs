//! (#2855) Tests for set summaries. Each encodes a way an arm comparison
//! reads wrong while every per-run number is right.

use super::*;
use crate::lab::stats::{RunChecks, RunStats};

/// A run whose every check holds, so a test sets only what it is about.
fn run(name: &str, verify: &str) -> RunStats {
    RunStats {
        run: name.into(),
        model: Some("m".into()),
        verify: Some(verify.into()),
        checks: RunChecks {
            tokens_reconcile: true,
            rest_within_wall: true,
            have_telemetry_samples: true,
            have_flow_records: true,
            checkpoint_parse_consistent: true,
            verdict_matches_ratio: true,
            all_streams_billed: true,
            frames_match_streams: true,
            streams_terminated: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn costed(name: &str, verify: &str, active_ms: u64, busy_ms: u64, joules: f64) -> RunStats {
    RunStats {
        active_ms,
        busy_ms: Some(busy_ms),
        pkg_j_busy: Some(joules),
        ..run(name, verify)
    }
}

/// The median and spread are both reported. A mean over these five reads
/// 136 and describes no run; the median says typical, and the max says one
/// run went sideways.
#[test]
fn a_range_carries_the_spread_not_just_a_centre() {
    let r = Range::of([100.0, 110.0, 120.0, 130.0, 220.0]).unwrap();
    assert_eq!(r, Range { n: 5, median: 120.0, min: 100.0, max: 220.0 });
    assert_eq!(Range::of([1.0, 2.0, 3.0, 4.0]).unwrap().median, 2.5);
    assert_eq!(Range::of(std::iter::empty()), None, "no values is no range, not zeros");
}

/// A failed run still consumed the GPU, so its cost counts in the
/// numerator while only passing runs count in the denominator. This is the
/// asymmetry that reversed a headline: the faster engine per token was the
/// dearer engine per finished result.
#[test]
fn cost_per_success_charges_failed_runs_to_the_runs_that_passed() {
    let set = [
        costed("a", "pass", 100_000, 90_000, 3_000.0),
        costed("b", "fail", 200_000, 180_000, 6_000.0),
        costed("c", "pass", 100_000, 90_000, 3_000.0),
    ];
    let s = summarize(&set);
    assert_eq!((s.n, s.passed, s.failed), (3, 2, 1));
    // 400s of active time bought two successes.
    assert_eq!(s.cost_per_success.active_ms, Some(200_000.0));
    assert_eq!(s.cost_per_success.gpu_busy_ms, Some(180_000.0));
    assert_eq!(s.cost_per_success.pkg_joules, Some(6_000.0));
    assert!(s.cost_per_success.withheld.is_empty());
}

/// Summing only the runs that recorded energy leaves the total short by the
/// one that did not, and a short total reads as a cheaper arm. The figure is
/// withheld with its reason instead of printed low.
#[test]
fn a_cost_with_a_run_missing_is_withheld_not_undercounted() {
    let mut no_telem = costed("b", "pass", 100_000, 0, 0.0);
    no_telem.busy_ms = None;
    no_telem.pkg_j_busy = None;
    let s = summarize(&[costed("a", "pass", 100_000, 90_000, 3_000.0), no_telem]);
    assert_eq!(s.cost_per_success.pkg_joules, None);
    assert_eq!(s.cost_per_success.gpu_busy_ms, None);
    assert_eq!(s.cost_per_success.active_ms, Some(100_000.0), "active time is on every run");
    assert!(s.cost_per_success.withheld.iter().any(|w| w.contains("package energy")));
}

/// No success, no cost per success — and it says why, rather than dividing
/// by zero or reporting nothing.
#[test]
fn a_set_with_no_passes_has_no_cost_per_success_and_says_so() {
    let s = summarize(&[costed("a", "fail", 100_000, 90_000, 3_000.0)]);
    assert_eq!(s.cost_per_success.active_ms, None);
    assert!(s.cost_per_success.withheld.iter().any(|w| w.contains("no run in the set passed")));
}

/// A run with no verify outcome is neither a pass nor a failure. Counting it
/// as a failure would inflate cost per success; as a pass, deflate it.
#[test]
fn an_unverified_run_is_neither_a_pass_nor_a_failure() {
    let mut r = run("a", "");
    r.verify = None;
    let s = summarize(&[r, run("b", "pass")]);
    assert_eq!((s.passed, s.failed, s.unverified), (1, 0, 1));
}

/// A run that did not reconcile stays in the set — dropping it is choosing
/// the answer — and the summary names it with the reason.
#[test]
fn a_run_that_did_not_reconcile_stays_in_the_set_and_is_named() {
    let mut bad = run("b", "pass");
    bad.checks.tokens_reconcile = false;
    bad.checks.all_streams_billed = false;
    let s = summarize(&[run("a", "pass"), bad]);
    assert_eq!(s.n, 2);
    assert_eq!(s.flagged, vec![("b".to_string(), vec!["TOKENS", "UNBILLED"])]);
}

/// Every check maps to a flag. A check with no flag is a failure the table
/// cannot show.
#[test]
fn every_failed_check_raises_a_flag() {
    let mut r = run("a", "pass");
    r.checks = RunChecks::default(); // every check false
    r.checks.missing_required_events = vec!["model.completed".into()];
    r.thermal_ratchet_fired = true;
    r.throttled_samples = 1;
    r.result = Some("error".into());
    assert_eq!(
        flags(&r),
        vec!["RUNTIME-ERROR", "TOKENS", "PARSE", "VERDICT", "UNBILLED", "STREAMS", "REST", "NO-TELEM", "RATCHET", "THROTTLE"]
    );
    assert!(flags(&run("clean", "pass")).is_empty());
}

/// Both gates count toward degeneracy, and cuts come from both.
#[test]
fn degeneracy_and_cuts_count_both_gates() {
    let mut a = run("a", "pass");
    a.gates.stream.degenerate_turns = vec![2];
    a.gates.stream.aborts = 1;
    let mut b = run("b", "pass");
    b.gates.checkpoint.degenerate_turns = vec![4];
    b.gates.checkpoint.concluded_turns = vec![4];
    let s = summarize(&[a, b, run("c", "pass")]);
    assert_eq!(s.runs_with_degeneracy, 2);
    assert_eq!(s.turns_cut, 2);
}

/// A set naming two models is two arms mixed together; listing them makes
/// that visible instead of averaging across engines.
#[test]
fn a_set_lists_every_model_it_contains() {
    let mut b = run("b", "pass");
    b.model = Some("other".into());
    let s = summarize(&[run("a", "pass"), b, run("c", "pass")]);
    assert_eq!(s.models, vec!["m".to_string(), "other".to_string()]);
}

#[test]
fn a_ratio_never_divides_by_zero_or_missing() {
    assert_eq!(ratio(Some(3.0), Some(2.0)), Some(1.5));
    assert_eq!(ratio(Some(3.0), Some(0.0)), None);
    assert_eq!(ratio(None, Some(2.0)), None);
}

/// Measured on the real pepper-grinder campaign: a run errored after five
/// seconds with zero turns and recorded `verify: pass`, because the
/// fixture's verify command is green on the untouched tree. It stays a
/// pass — the fixture defines the outcome — but the set names it.
#[test]
fn a_pass_from_a_run_that_errored_is_counted_and_named() {
    let mut r = run("do-nothing", "pass");
    r.result = Some("error".into());
    r.turns = 0;
    let mut ok = run("real", "pass");
    ok.result = Some("stop".into());
    let s = summarize(&[r, ok]);
    assert_eq!(s.passed, 2);
    assert_eq!(s.passed_with_runtime_error, 1);
    assert_eq!(s.flagged, vec![("do-nothing".to_string(), vec!["RUNTIME-ERROR"])]);
}

#[test]
fn an_escalated_run_is_flagged() {
    let mut r = run("a", "fail");
    r.result = Some("escalation_intra_turn_stall_exhausted".into());
    assert_eq!(flags(&r), vec!["ESCALATED"]);
}
