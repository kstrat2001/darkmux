//! (#2855) What `lab run stats` prints. CI's mutation job found every line
//! of the renderer could be deleted with the suite green; each test below
//! pins a promise the text makes to the person reading it.

use super::*;
use crate::lab::stats::{RunChecks, RunStats};

fn clean_checks() -> RunChecks {
    RunChecks {
        tokens_reconcile: Some(true),
        rest_within_wall: true,
        have_telemetry_samples: true,
        have_flow_records: true,
        checkpoint_parse_consistent: true,
        verdict_matches_ratio: true,
        all_streams_billed: true,
        frames_match_streams: true,
        streams_terminated: true,
        telemetry_covers_run: true,
        ..Default::default()
    }
}

fn run(name: &str) -> RunStats {
    RunStats {
        run: name.into(),
        model: Some("m-1".into()),
        result: Some("stop".into()),
        verify: Some("pass".into()),
        wall_ms: 400_000,
        rest_ms: 120_000,
        active_ms: 280_000,
        turns: 8,
        completion_tokens: 20_000,
        gen_ms_billed: 100_000,
        gen_ms_all: 100_000,
        billed_gen_fraction: Some(1.0),
        tok_per_s: Some(200.0),
        gpu_w_busy: Some(30.0),
        cpu_w_busy: Some(8.0),
        pkg_w_busy: Some(38.0),
        gpu_duty_pct: Some(70.0),
        samples_busy: 70,
        samples_idle: 30,
        busy_ms: Some(280_000),
        pkg_j_busy: Some(10_640.0),
        pkg_j_per_1k_tokens: Some(190.0),
        checks: clean_checks(),
        ..Default::default()
    }
}

/// The first line that STARTS with `prefix` (so a table header that merely
/// contains the word does not match).
fn line_starting<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no line starting {prefix:?} in:\n{text}"))
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

// ---- one run ---------------------------------------------------------------

/// Rest prints beside wall and active on ONE line, in seconds, so a rested
/// run's wall clock cannot be read as a slow model.
#[test]
fn rest_is_printed_beside_wall_and_active() {
    let t = run_text(&run("r"));
    assert_eq!(line_with(&t, "wall").trim(), "time         wall 400s   rest 120s   active 280s");
}

/// Both gates print, always, even with nothing to report. One "detection"
/// line is what let a reader take one gate's silence for the whole answer.
#[test]
fn both_gates_always_print() {
    let t = run_text(&run("r"));
    assert!(t.contains("stream gate  0 observations   0 degenerate   0 aborts"), "{t}");
    assert!(t.contains("checkpoint   0 observations   0 degenerate   0 cut"), "{t}");
}

/// Tok/s names the share of generation it rests on.
#[test]
fn throughput_names_its_billed_share() {
    let mut s = run("r");
    s.billed_gen_fraction = Some(0.14);
    s.gen_ms_billed = 43_000;
    s.gen_ms_all = 294_000;
    s.tok_per_s = Some(89.4);
    let t = run_text(&s);
    assert!(t.contains("89.4 tok/s over 14% of generation (43s of 294s)"), "{t}");
    s.tok_per_s = None;
    assert!(run_text(&s).contains("(no billed generation recorded)"));
}

/// A tail ratio prints at the precision that keeps a sub-threshold value
/// from reading as the threshold itself.
#[test]
fn a_tail_ratio_prints_at_six_decimals() {
    let mut s = run("r");
    s.gates.checkpoint.min_tail_ratio = Some(0.2499837);
    s.gates.checkpoint.policy = Some("observe".into());
    let t = run_text(&s);
    assert!(line_with(&t, "checkpoint").contains("min ratio 0.249984   policy=observe"), "{t}");
}

/// Rests and the ratchet print only when there were rests; the ratchet note
/// only when it fired.
#[test]
fn rests_print_their_distinct_delays_and_the_ratchet() {
    let mut s = run("r");
    assert!(!run_text(&s).contains("rests, delays"));
    s.rest_events = 3;
    s.rest_delays_ms = vec![15_000, 30_000];
    s.thermal_ratchet_fired = true;
    let t = run_text(&s);
    assert!(t.contains("3 rests, delays [15000, 30000]ms  (ratchet fired)"), "{t}");
}

/// Power, energy and thermals print from telemetry, with energy in kJ.
///
/// (#2855 review) The kJ figure is mean BUSY-sample watts × busy time, not
/// the run's total draw — idle and rest samples are excluded. "over the
/// run" reads as the whole run and overstates it; the label must say
/// "while busy" instead.
#[test]
fn power_and_energy_print_when_measured() {
    let mut s = run("r");
    s.thermal_states_busy.insert("fair".into(), 4);
    let t = run_text(&s);
    assert!(t.contains("gpu 30 W   cpu 8 W   package 38 W   busy 70% of the run (100 samples)"), "{t}");
    assert!(t.contains("energy       190 J per 1k tokens   10.6 kJ while busy"), "{t}");
    assert!(!t.contains("over the run"), "the run's total draw is a different, larger figure");
    assert!(t.contains("thermal      fair 4   cpu speed limit min 100%"), "{t}");
    s.gpu_w_busy = None;
    assert!(!run_text(&s).contains("power "), "no telemetry, no power line");
}

/// The caveats print beneath the figures, and only when a check failed.
#[test]
fn caveats_print_beneath_the_figures_when_a_check_failed() {
    let clean = run_text(&run("r"));
    assert!(!clean.contains("not reconciled"), "{clean}");
    let mut s = run("r");
    s.checks.all_streams_billed = false;
    let t = run_text(&s);
    let caveats = t.find("not reconciled").expect("caveat block");
    assert!(caveats > t.find("throughput").unwrap(), "caveats come after the figures");
    assert!(t[caveats..].contains("tok/s and energy per token cover only the billed streams"));
}

// ---- a set -----------------------------------------------------------------

fn set(runs: Vec<RunStats>) -> StatsSet {
    StatsSet { runs, errors: vec![], duplicates: vec![] }
}

/// One row per run, with its flags, or `ok` when it has none.
#[test]
fn the_table_has_one_row_per_run_with_its_flags() {
    let mut bad = run("bad-run");
    bad.checks.all_streams_billed = false;
    let t = sets_text(&set(vec![run("good-run"), bad]), None);
    assert!(line_with(&t, "good-run").trim_end().ends_with("ok"), "{t}");
    assert!(line_with(&t, "bad-run").trim_end().ends_with("UNBILLED"), "{t}");
    // Every figure column is filled from the run, not left blank.
    let row: Vec<&str> = line_with(&t, "good-run").split_whitespace().collect();
    assert_eq!(
        row,
        vec!["good-run", "pass", "280s", "120s", "8", "200.0", "100%", "20000", "0", "38.0", "70%", "190", "ok"]
    );
}

/// Set figures print as ranges, and the cost per success is shown.
#[test]
fn the_set_summary_prints_ranges_and_cost_per_success() {
    let mut b = run("b");
    b.active_ms = 480_000;
    let t = sets_text(&set(vec![run("a"), b]), None);
    assert_eq!(line_starting(&t, "  active").trim_end(), "  active           380s (280s–480s)");
    assert!(t.contains("cost per successful run"), "{t}");
    assert!(line_with(&t, "  energy").contains("10.6 kJ"), "{t}");
    assert!(line_with(&t, "set ").contains("2 of 2 passed"), "{t}");
}

/// A comparison prints both sides and the ratio between them.
#[test]
fn a_comparison_prints_what_moved() {
    let mut fast = run("fast");
    fast.tok_per_s = Some(400.0);
    let t = sets_text(&set(vec![fast]), Some(&set(vec![run("base")])));
    assert!(t.contains("baseline") && t.contains("candidate"), "{t}");
    assert!(line_starting(&t, "tok/s").trim_end().ends_with("2.00x"), "{t}");
}

/// What could not be counted is said, beneath the figures.
#[test]
fn unread_duplicate_and_unverified_runs_are_named() {
    let mut s = set(vec![run("a")]);
    s.errors.push(("gone".into(), "no run directory".into()));
    s.duplicates.push("a".into());
    let mut unv = run("u");
    unv.verify = None;
    s.runs.push(unv);
    let t = sets_text(&s, None);
    assert!(t.contains("gone") && t.contains("not counted: no run directory"), "{t}");
    assert!(t.contains("a was listed more than once and is counted once"), "{t}");
    assert!(t.contains("1 listed run(s) are not in the figures above"), "{t}");
    assert!(line_with(&t, "set ").contains("1 of 2 passed, 1 unverified"), "{t}");
}

/// A pass from a run whose runtime errored is called out.
#[test]
fn a_pass_from_an_errored_run_is_called_out() {
    let mut e = run("e");
    e.result = Some("error".into());
    let t = sets_text(&set(vec![e, run("ok")]), None);
    assert!(t.contains("1 of the 2 passes came from runs whose runtime result was `error`"), "{t}");
}

// ---- JSON and exit code ----------------------------------------------------

#[test]
fn the_set_json_carries_runs_summary_and_what_was_not_counted() {
    let mut s = set(vec![run("a")]);
    s.errors.push(("gone".into(), "x".into()));
    let j = sets_json(&s, Some(&set(vec![run("b")])));
    assert_eq!(j["runs"][0]["run"], "a");
    assert_eq!(j["summary"]["n"], 1);
    assert_eq!(j["errors"][0][0], "gone");
    assert_eq!(j["baseline"]["runs"][0]["run"], "b");
}

/// A run that could not be read fails the command; a duplicate does not.
#[test]
fn only_an_unread_run_makes_the_exit_code_non_zero() {
    let ok = set(vec![run("a")]);
    assert_eq!(exit_code(&ok, None), 0);
    let mut dup = set(vec![run("a")]);
    dup.duplicates.push("a".into());
    assert_eq!(exit_code(&dup, None), 0);
    let mut bad = set(vec![run("a")]);
    bad.errors.push(("gone".into(), "x".into()));
    assert_eq!(exit_code(&bad, None), 1);
    assert_eq!(exit_code(&ok, Some(&bad)), 1, "an unread baseline run counts too");
}

/// Unread runs on BOTH sides of a comparison are counted together.
#[test]
fn unread_runs_are_counted_across_both_sets() {
    let mut cand = set(vec![run("a")]);
    cand.errors.push(("gone-a".into(), "x".into()));
    let mut base = set(vec![run("b")]);
    base.errors.push(("gone-b".into(), "x".into()));
    let t = sets_text(&cand, Some(&base));
    assert!(t.contains("2 listed run(s) are not in the figures above"), "{t}");
}

// ---- Same run named in both arms (#2855 review) -----------------------------

/// `stats X --baseline X` prints a 1.00x comparison and exits 0 with no
/// indication the two arms are the same data. A notice, same shape as the
/// existing duplicates notice, must say so.
#[test]
fn a_run_named_in_both_arms_gets_a_notice() {
    let cand = set(vec![run("x"), run("y")]);
    let base = set(vec![run("x")]);
    let t = sets_text(&cand, Some(&base));
    assert!(
        t.contains("x is in both the candidate and the baseline"),
        "{t}"
    );
    assert!(!t.contains("y is in both"), "y is only in the candidate");
    assert_eq!(exit_code(&cand, Some(&base)), 0, "a notice, not an error");
    let j = sets_json(&cand, Some(&base));
    assert_eq!(j["cross_arm_overlap"], serde_json::json!(["x"]));
}

/// No overlap, no notice.
#[test]
fn distinct_arms_get_no_cross_arm_overlap_notice() {
    let t = sets_text(&set(vec![run("x")]), Some(&set(vec![run("y")])));
    assert!(!t.contains("is in both the candidate and the baseline"), "{t}");
}

// ---- Terminal control characters (#2855 review, security) ------------------

/// `model`, `result`, `verify` and the checkpoint `policy` ride the
/// trajectory, which the sandbox's model-writable `/darkmux-out` can shape.
/// An ESC/OSC payload in any of them must not reach the terminal.
#[test]
fn control_characters_in_model_facing_strings_are_stripped_before_printing() {
    let mut s = run("r");
    s.model = Some("evil\u{1b}]0;pwned\u{07}model".into());
    s.result = Some("stop\u{1b}[31m".into());
    s.verify = Some("pass\u{1b}[2J".into());
    s.gates.checkpoint.policy = Some("observe\u{1b}[0m".into());
    let t = run_text(&s);
    assert!(!t.contains('\u{1b}'), "an ESC byte reached the terminal:\n{t:?}");
    assert!(!t.contains('\u{07}'), "a BEL byte reached the terminal:\n{t:?}");
    assert!(t.contains("model:       evil]0;pwnedmodel"));
    assert!(line_starting(&t, "result:").contains("stop[31m"));
    assert!(line_with(&t, "policy=").contains("policy=observe[0m"));
}
