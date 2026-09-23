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
            tokens_reconcile: Some(true),
            rest_within_wall: true,
            have_telemetry_samples: true,
            telemetry_covers_run: true,
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
    bad.checks.tokens_reconcile = Some(false);
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
    r.checks = RunChecks::default(); // every boolean check false
    r.checks.tokens_reconcile = Some(false);
    r.checks.turns_match_trajectory = Some(false);
    r.checks.missing_required_events = vec!["model.completed".into()];
    r.suspect_turns = vec![crate::lab::stats::SuspectTurn { seq: 1, reasoning_chars_per_token: 90.0 }];
    r.thermal_ratchet_fired = true;
    r.throttled_samples = 1;
    r.result = Some("error".into());
    r.verify_ungated = true;
    assert_eq!(
        flags(&r),
        vec![
            "RUNTIME-ERROR", "TOKENS", "PARSE", "VERDICT", "UNBILLED", "STREAMS", "REST",
            "COUNTS", "NO-TELEM", "NO-FLOW", "CHARS", "RATCHET", "THROTTLE", "UNGATED"
        ]
    );
    assert!(flags(&run("clean", "pass")).is_empty());
}

/// (#2833) `verify_ungated` alone raises exactly one flag, on an otherwise
/// clean run — proving the flag isn't riding along with some other check.
#[test]
fn verify_ungated_alone_raises_its_own_flag() {
    let mut r = run("a", "pass");
    r.verify_ungated = true;
    assert_eq!(flags(&r), vec!["UNGATED"]);
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

/// Two runs whose windows overlap each claim the whole host's power
/// (`scan_flow_lines` matches telemetry by time window only), so their
/// energy figures cannot be summed without double-counting. Both are
/// flagged, and the set's energy cost is withheld rather than divided up.
#[test]
fn overlapping_runs_are_flagged_and_their_energy_is_withheld() {
    let mut a = costed("a", "pass", 100_000, 90_000, 3_000.0);
    a.started_at_unix_ms = Some(1_000_000);
    a.wall_ms = 100_000; // window [1_000_000, 1_100_000]
    let mut b = costed("b", "pass", 100_000, 90_000, 3_000.0);
    b.started_at_unix_ms = Some(1_050_000); // starts before `a` ends
    b.wall_ms = 100_000;
    let s = summarize(&[a, b]);
    assert_eq!(s.flagged.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(s.flagged.iter().all(|(_, f)| f.contains(&"OVERLAP")));
    assert_eq!(s.cost_per_success.gpu_busy_ms, None);
    assert_eq!(s.cost_per_success.pkg_joules, None);
    // Active time is not host telemetry and is unaffected by the overlap.
    assert_eq!(s.cost_per_success.active_ms, Some(100_000.0));
    // (Review, 2026-09-23) the EXACT reason per figure, not merely "some
    // withheld note mentions OVERLAP" — a generic "not every run recorded
    // it" message must not ALSO appear once the real reason is known.
    assert_eq!(
        s.cost_per_success.withheld,
        vec![
            "GPU busy time: 2 run(s) have overlapping host-telemetry windows (OVERLAP) and cannot be apportioned"
                .to_string(),
            "package energy: 2 run(s) have overlapping host-telemetry windows (OVERLAP) and cannot be apportioned"
                .to_string(),
        ]
    );
}

/// (Review, 2026-09-23) A STALE-METRICS run's borrowed window must not
/// implicate the CLEAN run whose metrics it copied — `overlapping()`
/// excludes metrics_stale runs before building windows at all, so the
/// genuine owner is never even compared against the copy.
#[test]
fn a_stale_metrics_runs_window_does_not_flag_the_clean_owner_it_overlaps() {
    let mut owner = costed("owner", "pass", 100_000, 90_000, 3_000.0);
    owner.started_at_unix_ms = Some(1_000_000);
    owner.wall_ms = 100_000;
    let mut stale_copy = costed("copy", "pass", 100_000, 90_000, 3_000.0);
    // Literally the same window as `owner` — a byte-identical metrics.json
    // copy would produce exactly this.
    stale_copy.started_at_unix_ms = Some(1_000_000);
    stale_copy.wall_ms = 100_000;
    stale_copy.checks.metrics_stale = true;
    let s = summarize(&[owner, stale_copy]);
    let owner_flags = s.flagged.iter().find(|(r, _)| r == "owner").map(|(_, f)| f.clone());
    assert_eq!(owner_flags, None, "the clean owner must not be flagged OVERLAP: {:?}", s.flagged);
    // The copy is still flagged, on its own merits (STALE-METRICS), and its
    // energy is still withheld — just not doubly implicated as OVERLAP too.
    let copy_flags = s.flagged.iter().find(|(r, _)| r == "copy").unwrap().1.clone();
    assert_eq!(copy_flags, vec!["STALE-METRICS"]);
}

/// Adjacent, non-overlapping runs (one starts exactly as the other ends)
/// must not be flagged — the boundary itself is not an overlap.
#[test]
fn back_to_back_runs_do_not_overlap() {
    let mut a = costed("a", "pass", 100_000, 90_000, 3_000.0);
    a.started_at_unix_ms = Some(1_000_000);
    a.wall_ms = 100_000; // ends at 1_100_000
    let mut b = costed("b", "pass", 100_000, 90_000, 3_000.0);
    b.started_at_unix_ms = Some(1_100_000); // starts exactly when `a` ends
    b.wall_ms = 100_000;
    let s = summarize(&[a, b]);
    assert!(s.flagged.is_empty(), "{:?}", s.flagged);
    assert_eq!(s.cost_per_success.pkg_joules, Some(3_000.0));
}

/// A `metrics_stale` run's active time AND host-telemetry energy are both
/// built on that run's own `wall_ms`/window — which is exactly the value
/// under suspicion — so neither feeds cost per success. Unlike a checks
/// failure that leaves the FIGURE itself untouched (TOKENS, UNBILLED, etc.),
/// which stays in every figure per the set's "never drop a run" doctrine.
#[test]
fn a_stale_metrics_run_is_excluded_from_cost_per_success() {
    let mut stale = costed("a", "pass", 999_000, 900_000, 30_000.0);
    stale.checks.metrics_stale = true;
    let clean = costed("b", "pass", 100_000, 90_000, 3_000.0);
    let s = summarize(&[stale, clean]);
    assert_eq!(s.cost_per_success.active_ms, None, "the whole total is withheld, not read low");
    assert_eq!(s.cost_per_success.gpu_busy_ms, None);
    assert_eq!(s.cost_per_success.pkg_joules, None);
    // (Review, 2026-09-23) exact reasons: one for active time, one for
    // energy — both correctly attributed to STALE-METRICS, and NEITHER the
    // generic "not every run recorded it" fallback (which would be false
    // here — every run DID record a value; this run's is just excluded).
    assert_eq!(
        s.cost_per_success.withheld,
        vec![
            "active time: 1 run(s) have a metrics.json that does not belong to them (STALE-METRICS)"
                .to_string(),
            "GPU busy time: 1 run(s) have a metrics.json that does not belong to them (STALE-METRICS)"
                .to_string(),
            "package energy: 1 run(s) have a metrics.json that does not belong to them (STALE-METRICS)"
                .to_string(),
        ]
    );
}

/// (Frontier review, 2026-09-23) A STALE-METRICS run's active/wall/rest
/// figures are wrong, not merely uncertain — the docs said they were
/// withheld, but only `cost_per_success` was actually excluding them; the
/// SET RANGES still folded the bad number into the median/min/max, and a
/// baseline "moved" ratio (built from these same ranges) would too. The fix
/// excludes the run from these ranges outright, same as cost per success.
#[test]
fn a_stale_metrics_runs_active_time_is_excluded_from_the_set_range_not_just_cost() {
    let mut stale = costed("a", "pass", 999_000_000, 900_000, 30_000.0);
    stale.checks.metrics_stale = true;
    stale.wall_ms = 999_000_000;
    stale.rest_ms = 0;
    let clean = costed("b", "pass", 100_000, 90_000, 3_000.0);
    let clean_wall = clean.wall_ms;
    let s = summarize(&[stale, clean]);
    let active = s.active_ms.expect("clean run still contributes");
    assert_eq!(active.n, 1, "the stale run's absurd active time must not enter the range");
    assert_eq!(active.median, 100_000.0);
    let wall = s.wall_ms.expect("clean run still contributes");
    assert_eq!(wall.n, 1);
    assert_eq!(wall.median, clean_wall as f64);
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

/// Review C9: a figure over one run printed as a bare number, identical to
/// five runs that agreed. The count shows whenever it is short of the set.
#[test]
fn a_range_over_fewer_runs_than_the_set_says_how_many() {
    let f = |v: f64| format!("{v:.0}");
    assert_eq!(fmt_range(Range::of([120.0]), 5, &f), "120 [n=1]");
    assert_eq!(fmt_range(Range::of([120.0; 5]), 5, &f), "120");
    assert_eq!(fmt_range(Range::of([100.0, 140.0]), 5, &f), "120 (100–140) [n=2]");
    assert_eq!(fmt_range(None, 5, &f), "-");
}

/// Review C10: flags that are an OR of two conditions were only tested with
/// every condition false at once, so either half could be deleted unseen.
/// Each condition alone must raise its flag.
#[test]
fn each_condition_raises_its_flag_alone() {
    let only = |set: fn(&mut RunStats)| {
        let mut r = run("a", "pass");
        set(&mut r);
        flags(&r)
    };
    assert_eq!(only(|r| r.checks.checkpoint_parse_consistent = false), vec!["PARSE"]);
    assert_eq!(only(|r| r.checks.missing_required_events = vec!["x".into()]), vec!["PARSE"]);
    assert_eq!(only(|r| r.checks.frames_match_streams = false), vec!["STREAMS"]);
    assert_eq!(only(|r| r.checks.streams_terminated = false), vec!["STREAMS"]);
    assert_eq!(only(|r| r.checks.turns_match_trajectory = Some(false)), vec!["COUNTS"]);
    assert_eq!(only(|r| r.checks.rest_matches_trajectory = Some(false)), vec!["COUNTS"]);
    assert_eq!(only(|r| r.checks.telemetry_covers_run = false), vec!["TELEM-GAP"]);
    assert_eq!(only(|r| r.checks.have_flow_records = false), vec!["NO-FLOW"]);
    assert_eq!(only(|r| r.checks.policy_consistent = Some(false)), vec!["POLICY"]);
    assert_eq!(only(|r| r.checks.tokens_reconcile = None), Vec::<&str>::new(), "not checkable is not a failure");
}
