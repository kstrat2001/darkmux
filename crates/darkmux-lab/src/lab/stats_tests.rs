//! (#2855) Tests for the derived-metrics layer.
//!
//! Each of these encodes a WRONG NUMBER that was actually published before
//! this module existed. Where a test's name says "not", the naive reading is
//! the thing it fails on: assigning usage frames instead of summing them,
//! reading one gate, counting an unbilled stream's seconds, averaging power
//! across idle. Mutating the code back to the naive form turns the named
//! test red — that is what each one is for.

use super::*;

fn line(v: serde_json::Value) -> String {
    format!("{v}\n")
}

/// A `model.completed` with an explicit (possibly null) completion count.
fn completed(seq: u64, ct: Option<u64>, rt: u64) -> String {
    line(serde_json::json!({
        "type": "model.completed", "seq": seq,
        "usage": {"completion_tokens": ct, "reasoning_tokens": rt}
    }))
}

fn stream(seq: u64, t0: u64, t1: Option<u64>) -> String {
    let mut s = line(serde_json::json!({
        "type": "model.streaming.start", "seq": seq, "ts": t0
    }));
    if let Some(t1) = t1 {
        s.push_str(&line(serde_json::json!({
            "type": "model.streaming.end", "seq": seq, "ts": t1
        })));
    }
    s
}

fn metrics(wall_ms: u64, rest_ms: u64, turns: u64, total_ct: u64) -> RuntimeMetrics {
    RuntimeMetrics {
        model: Some("test-model".into()),
        result: Some("stop".into()),
        started_at_unix_ms: Some(1_000_000),
        wall_ms: Some(wall_ms),
        rest_ms: Some(rest_ms),
        turns: Some(turns),
        compactions: Some(0),
        total_completion_tokens: Some(total_ct),
    }
}

fn stats(traj: &str, m: RuntimeMetrics) -> RunStats {
    derive_stats("t".into(), m, parse_trajectory(traj), FlowFacts::default(), None, None)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// One seq emits several usage frames — every checkpoint continuation
/// reports its own. Assigning instead of accumulating kept only the last and
/// undercounted a real run 15x (4,553 against 68,553), turning 173 tok/s
/// into 11.5.
#[test]
fn usage_frames_accumulate_within_one_turn_and_are_not_assigned() {
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(10_000)),
        completed(1, Some(237), 10),
        completed(1, Some(1_488), 20),
        completed(1, Some(1_881), 30),
    );
    let s = stats(&traj, metrics(20_000, 0, 1, 3_606));
    assert_eq!(s.completion_tokens, 3_606, "sum, not the last frame (1,881)");
    assert_eq!(s.reasoning_tokens, 60);
    assert_eq!(s.checks.tokens_reconcile, Some(true), "the per-turn sum must equal the run total");
}

/// The check that caught the undercount: it fails the moment the per-turn
/// sum stops matching `metrics.json`.
#[test]
fn tokens_reconcile_is_false_when_the_run_total_disagrees() {
    let traj = format!("{}{}", stream(1, 0, Some(1_000)), completed(1, Some(100), 0));
    let s = stats(&traj, metrics(2_000, 0, 1, 999));
    assert_eq!(s.checks.tokens_reconcile, Some(false));
    assert!(s.unreconciled().iter().any(|r| r.contains("sum to the run total")));
}

// ---------------------------------------------------------------------------
// Billing
// ---------------------------------------------------------------------------

/// A NULL `completion_tokens` is an UNBILLED stream, not a zero: the gate
/// ended the call, so the endpoint's final usage frame never arrived. Its
/// SECONDS are present while its TOKENS are absent, and counting both
/// against each other reported 13.0 tok/s for an engine measured at 116.
#[test]
fn a_null_usage_frame_is_an_unbilled_stream_not_a_zero() {
    // 80s unbilled, then a 20s billed stream carrying 2,000 tokens.
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(80_000)),
        completed(1, None, 0),
        stream(2, 80_000, Some(100_000)),
        completed(2, Some(2_000), 0),
    );
    let s = stats(&traj, metrics(120_000, 0, 2, 2_000));

    assert_eq!(s.streams, 2);
    assert_eq!(s.streams_unbilled, 1);
    assert_eq!(s.gen_ms_all, 100_000);
    assert_eq!(s.gen_ms_billed, 20_000, "the unbilled stream's 80s are excluded");
    assert_eq!(s.unbilled_gen_ms, 80_000);
    assert_eq!(s.tok_per_s, Some(100.0), "2000 tokens over the 20s that produced them");
    assert_eq!(s.billed_gen_fraction, Some(0.2), "and the reader is told it is a 20% sample");
    assert!(!s.checks.all_streams_billed);
    assert!(s.unreconciled().iter().any(|r| r.contains("billed streams")));
}

/// Energy per token has to run over the SAME subset the tokens came from.
/// Mixing all-generation seconds with billed-only tokens inflated one figure
/// 6.9x.
#[test]
fn energy_per_token_uses_billed_seconds_not_all_generation() {
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(80_000)),
        completed(1, None, 0),
        stream(2, 80_000, Some(100_000)),
        completed(2, Some(2_000), 0),
    );
    let flows = FlowFacts {
        samples: vec![Sample { gpu_pct: 95, w_gpu: 30.0, w_cpu: 5.0, w_total: 40.0, ..Default::default() }],
        ..Default::default()
    };
    let s = derive_stats("t".into(), metrics(120_000, 0, 2, 2_000), parse_trajectory(&traj), flows, None, None);
    // 40 W x 20 billed seconds / 2 thousand tokens = 400 J per 1k tokens.
    // Over all 100 generation seconds it would read 2,000 — five times high,
    // exactly the unbilled fraction.
    assert_eq!(s.pkg_j_per_1k_tokens, Some(400.0));
}

/// A seq can carry MORE THAN ONE stream: an aborted turn is retried under
/// the same seq. Keying spans by seq collapsed the pair and mis-timed both
/// (measured: an 87.1s abort and its 8.8s retry, both seq 2).
#[test]
fn two_streams_under_one_seq_are_timed_separately() {
    let traj = format!(
        "{}{}{}{}",
        stream(2, 0, Some(87_100)),
        completed(2, None, 0),
        stream(2, 90_000, Some(98_800)),
        completed(2, Some(500), 0),
    );
    let s = stats(&traj, metrics(120_000, 0, 1, 500));
    assert_eq!(s.streams, 2, "one seq, two streams");
    assert_eq!(s.gen_ms_all, 95_900, "87.1s + 8.8s, not one collapsed span");
    assert_eq!(s.gen_ms_billed, 8_800, "the retry is what was billed");
}

/// A stream that never ended is a run that died mid-call. Its generation
/// time is unknown, not zero, and the check says so rather than letting the
/// silence read as a fast turn.
#[test]
fn an_unterminated_stream_is_reported_not_silently_zero() {
    let traj = format!("{}{}", stream(1, 0, Some(1_000)), stream(2, 2_000, None));
    let s = stats(&traj, metrics(10_000, 0, 2, 0));
    assert_eq!(s.streams, 2);
    assert_eq!(s.streams_unterminated, 1);
    assert!(!s.checks.streams_terminated);
    assert!(!s.checks.frames_match_streams, "no usage frames arrived for either");
}

// ---------------------------------------------------------------------------
// The two gates
// ---------------------------------------------------------------------------

/// There are TWO gates. A reader that keys on `dispatch.checkpoint` alone
/// reports zero degeneracy on runs the STREAM gate cut — which is how a
/// campaign's 202 streaming observations and 9 degenerate turns became
/// invisible.
#[test]
fn both_gates_are_counted_and_never_conflated() {
    let traj = format!(
        "{}{}{}{}",
        line(serde_json::json!({"type":"dispatch.gate.observation","seq":2,"tail_ratio":1.0,"degenerate":false})),
        line(serde_json::json!({"type":"dispatch.gate.observation","seq":2,"tail_ratio":0.19,"degenerate":true})),
        line(serde_json::json!({"type":"dispatch.gate.abort","seq":2})),
        line(serde_json::json!({"type":"dispatch.checkpoint","seq":4,"tail_ratio":0.42,
                                "verdict":"continue","would_conclude":false,"policy":"enforce"})),
    );
    let s = stats(&traj, metrics(10_000, 0, 4, 0));

    assert_eq!(s.gates.stream.observations, 2);
    assert_eq!(s.gates.stream.degenerate_turns, vec![2]);
    assert_eq!(s.gates.stream.aborts, 1);
    assert_eq!(s.gates.stream.min_tail_ratio, Some(0.19));

    assert_eq!(s.gates.checkpoint.observations, 1, "the checkpoint gate is its own count");
    assert!(s.gates.checkpoint.degenerate_turns.is_empty());
    assert_eq!(s.gates.checkpoint.min_tail_ratio, Some(0.42));
    assert_eq!(s.gates.checkpoint.policy.as_deref(), Some("enforce"));
}

/// Under `observe` the runtime RECORDS a degenerate finding and does not act
/// on it: `would_conclude` is true while the verdict stays `continue`.
/// Reading the verdict as the judgment makes every observing run look clean,
/// which is the exact claim the policy exists to test.
#[test]
fn observe_separates_the_finding_from_the_action() {
    let traj = line(serde_json::json!({
        "type":"dispatch.checkpoint","seq":3,"tail_ratio":0.21,
        "verdict":"continue","would_conclude":true,"policy":"observe"
    }));
    let s = stats(&traj, metrics(10_000, 0, 3, 0));
    assert_eq!(s.gates.checkpoint.degenerate_turns, vec![3], "the finding is recorded");
    assert!(s.gates.checkpoint.concluded_turns.is_empty(), "and nothing was cut");
    assert!(s.checks.verdict_matches_ratio, "the ratio agrees with the finding");
    assert_eq!(s.gates.checkpoint.policy.as_deref(), Some("observe"));
}

/// The ratio is carried at full precision, because rounding it away changes
/// the verdict: 0.2499837 rounds to `0.25` at 4dp and then reads as sitting
/// ON the threshold, while the comparison the runtime made is a strict `<`.
#[test]
fn a_tail_ratio_is_not_rounded_into_the_threshold() {
    let traj = line(serde_json::json!({
        "type":"dispatch.checkpoint","seq":1,"tail_ratio":0.2499837,
        "verdict":"conclude","would_conclude":true
    }));
    let s = stats(&traj, metrics(10_000, 0, 1, 0));
    let r = s.gates.checkpoint.min_tail_ratio.unwrap();
    assert_eq!(r, 0.2499837, "carried as measured, not rounded");
    assert!(r < DEGENERATE_TAIL_RATIO);
    // The trap, stated: at four decimals this number IS the threshold.
    assert_eq!(format!("{r:.4}"), "0.2500");
    assert_eq!(format!("{:.*}", TAIL_RATIO_DISPLAY_DP, r), "0.249984");
    assert_eq!(s.gates.checkpoint.degenerate_turns_by_ratio, vec![1], "strictly below 0.25");
    assert!(s.checks.verdict_matches_ratio);
}

/// The threshold here is a COPY of the runtime's — the runtime crate is not
/// a workspace member and cannot be imported. So it is never trusted alone:
/// when the recorded judgment and the re-derived one name different turns,
/// the check says the copy has drifted rather than quietly picking one.
#[test]
fn a_verdict_the_threshold_does_not_reproduce_is_surfaced() {
    let traj = line(serde_json::json!({
        "type":"dispatch.checkpoint","seq":1,"tail_ratio":0.9,
        "verdict":"conclude","would_conclude":true
    }));
    let s = stats(&traj, metrics(10_000, 0, 1, 0));
    assert!(!s.checks.verdict_matches_ratio);
    assert!(s.unreconciled().iter().any(|r| r.contains("disagree")));
}

/// A checkpoint carrying no ratio is evidence of nothing; treating a missing
/// ratio as a perfect 1.0 would report the turn as healthy.
#[test]
fn a_checkpoint_without_a_ratio_does_not_count_as_healthy() {
    let traj = line(serde_json::json!({
        "type":"dispatch.checkpoint","seq":1,"verdict":"continue","would_conclude":false
    }));
    let s = stats(&traj, metrics(10_000, 0, 1, 0));
    assert_eq!(s.gates.checkpoint.observations, 1);
    assert_eq!(s.gates.checkpoint.min_tail_ratio, None, "no ratio, no claim");
}

/// The guard that catches a typo in THIS module: checkpoint records are
/// demonstrably in the trajectory and the parse counted none. Repointing the
/// parse at a name the producer does not emit flips exactly this, while
/// `checkpoint_events_seen` stays true — the contradiction is the signal.
#[test]
fn checkpoint_parse_consistency_catches_a_reader_keyed_on_the_wrong_name() {
    let mut t = Trajectory::default();
    t.seen_types.insert("dispatch.checkpoint".into());
    let s = derive_stats("t".into(), metrics(1, 0, 0, 0), t, FlowFacts::default(), None, None);
    assert!(s.checks.checkpoint_events_seen);
    assert!(!s.checks.checkpoint_parse_consistent);
    assert!(s.unreconciled().iter().any(|r| r.contains("none were parsed")));
}

/// A run that produced turns but carries none of the events this reading
/// keys on is a vocabulary mismatch, not a quiet zero.
#[test]
fn missing_required_events_are_named() {
    let traj = line(serde_json::json!({"type":"model.partial","seq":1,"cumulative_chars":10}));
    let s = stats(&traj, metrics(1_000, 0, 1, 0));
    assert_eq!(
        s.checks.missing_required_events,
        vec!["model.streaming.start", "model.streaming.end", "model.completed"]
    );
}

// ---------------------------------------------------------------------------
// Channels, time, rest
// ---------------------------------------------------------------------------

/// `model.reasoning` is the THINKING channel and `model.partial` is content.
/// They differ by more than 10x, so neither may be quoted as "output chars".
#[test]
fn reasoning_and_content_chars_are_counted_as_separate_channels() {
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(10_000)),
        line(serde_json::json!({"type":"model.reasoning","seq":1,"reasoning_chars":43_263})),
        line(serde_json::json!({"type":"model.partial","seq":1,"cumulative_chars":3_000})),
        line(serde_json::json!({"type":"model.partial","seq":1,"cumulative_chars":6_743})),
    );
    let s = stats(&traj, metrics(20_000, 0, 1, 0));
    assert_eq!(s.reasoning_chars, 43_263);
    assert_eq!(s.content_chars, 6_743, "cumulative within the turn, so the max");
    assert_eq!(s.reasoning_chars_per_s, Some(4326.3));
}

/// Wall carries a thermal penalty charged PER TURN, so an engine taking more
/// turns pays more rest at identical thermals. `active_ms` is the figure a
/// cross-engine comparison uses (#2848).
#[test]
fn active_time_excludes_rest() {
    let s = stats("", metrics(400_000, 120_000, 8, 0));
    assert_eq!(s.active_ms, 280_000);
    assert!(s.checks.rest_within_wall);
}

/// The thermal ratchet doubles the delay after a serious episode. A run whose
/// rests are not all the same length was slowed mid-run, which would
/// otherwise read as the run-index trend a blocked design looks for.
#[test]
fn distinct_rest_delays_expose_the_thermal_ratchet() {
    let traj = format!(
        "{}{}{}",
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
        line(serde_json::json!({"type":"runtime.rest","ms":30_000,"reason":"thermal-duty-cycle","state":"serious"})),
    );
    let s = stats(&traj, metrics(200_000, 60_000, 4, 0));
    assert_eq!(s.rest_events, 3);
    assert_eq!(s.rest_delays_ms, vec![15_000, 30_000]);
    assert!(s.thermal_ratchet_fired);
    assert_eq!(s.rest_states, vec!["fair".to_string(), "serious".to_string()]);
    assert_eq!(s.rest_ms_per_turn, Some(15_000.0));
}

/// The same rest delay recurring is the ordinary case — one thermal budget,
/// paid repeatedly at the same rate — and must NOT read as the ratchet
/// having fired. (Mutation survivor: `rest_delays.len() > 1` mutated to
/// `> 0` stayed green under the WHOLE suite, because nothing asserted the
/// negative case — a repeated, not-yet-escalated delay.)
#[test]
fn a_repeated_identical_rest_delay_does_not_fire_the_ratchet() {
    let traj = format!(
        "{}{}",
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
    );
    let s = stats(&traj, metrics(200_000, 30_000, 4, 0));
    assert_eq!(s.rest_events, 2);
    assert_eq!(s.rest_delays_ms, vec![15_000], "one DISTINCT delay, paid twice");
    assert!(!s.thermal_ratchet_fired);
}

/// (Frontier review, 2026-09-23) The governor's ratchet is ONE-WAY — it only
/// ever multiplies the duty-cycle delay, never divides it back down
/// (`thermal_governor.rs`). Two distinct thermal delays where the SECOND is
/// SMALLER cannot be the ratchet firing; the old "more than one distinct
/// value" predicate could not tell this from a genuine escalation.
#[test]
fn a_decreasing_thermal_delay_does_not_fire_the_ratchet() {
    let traj = format!(
        "{}{}",
        line(serde_json::json!({"type":"runtime.rest","ms":30_000,"reason":"thermal-duty-cycle","state":"serious"})),
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
    );
    let s = stats(&traj, metrics(200_000, 45_000, 4, 0));
    assert_eq!(s.rest_delays_ms, vec![15_000, 30_000], "distinct values are still reported for display");
    assert!(!s.thermal_ratchet_fired, "a decrease is not the one-way ratchet");
}

/// A distinct delay from an unrelated, non-thermal rest reason (a pace-file
/// pause, say) must not be mistaken for the governor's ratchet — only
/// `thermal-duty-cycle` rests carry ratchet evidence.
#[test]
fn a_non_thermal_rest_with_a_different_delay_does_not_fire_the_ratchet() {
    let traj = format!(
        "{}{}",
        line(serde_json::json!({"type":"runtime.rest","ms":15_000,"reason":"thermal-duty-cycle","state":"fair"})),
        line(serde_json::json!({"type":"runtime.rest","ms":90_000,"reason":"paused","state":"operator-hold"})),
    );
    let s = stats(&traj, metrics(200_000, 105_000, 4, 0));
    assert_eq!(s.rest_delays_ms, vec![15_000, 90_000]);
    assert!(!s.thermal_ratchet_fired, "only one thermal-duty-cycle delay exists; nothing escalated");
}

/// A turn whose reasoning chars and reasoning tokens imply an impossible
/// ratio is listed, not averaged in. Comparing against TOTAL completion
/// tokens instead flagged three healthy turns whose output was mostly
/// tool-call arguments.
#[test]
fn an_implausible_chars_per_token_turn_is_listed() {
    let traj = format!(
        "{}{}{}",
        stream(1, 0, Some(1_000)),
        completed(1, Some(500), 10),
        line(serde_json::json!({"type":"model.reasoning","seq":1,"reasoning_chars":9_000})),
    );
    let s = stats(&traj, metrics(2_000, 0, 1, 500));
    assert_eq!(s.suspect_turns.len(), 1);
    assert_eq!(s.suspect_turns[0].seq, 1);
    assert_eq!(s.suspect_turns[0].reasoning_chars_per_token, 900.0);
}

// ---------------------------------------------------------------------------
// Host telemetry
// ---------------------------------------------------------------------------

fn telem(ts: u64, gpu_pct: u64, w: f64) -> String {
    line(serde_json::json!({
        "action": "machine.telemetry",
        "payload": {
            "sampled_at_ms": ts, "gpu_pct": gpu_pct, "mem_pct": 62,
            "thermal": {"state": "nominal", "cpu_speed_limit_pct": 100},
            "power_mw": {"gpu": (w * 1000.0) as u64, "cpu": 5_000, "total": (w * 1000.0) as u64 + 5_000}
        }
    }))
}

/// Power averaged across the whole run mixes inference with idle and
/// understates load by whatever fraction of the run was tool calls and rest.
/// Busy-only, with the duty cycle beside it, is the honest pair.
///
/// (Revised after review: the first version of this test put four samples
/// in the first 30 ms of a 100 s run and asserted 50 s busy — it enshrined
/// the extrapolation it should have refused. These samples cover the run.)
#[test]
fn power_is_averaged_over_busy_samples_only_with_a_duty_cycle() {
    let from = 1_000_000; // `metrics()`'s start
    let at = |off: u64, gpu: u64, w: f64| {
        line(serde_json::json!({
            "action": "machine.telemetry",
            "payload": {
                "sampled_at_ms": from + off, "interval_ms": 25_000, "gpu_pct": gpu, "mem_pct": 62,
                "thermal": {"state": "nominal", "cpu_speed_limit_pct": 100},
                "power_mw": {"gpu": (w * 1000.0) as u64, "cpu": 5_000, "total": (w * 1000.0) as u64 + 5_000}
            }
        }))
    };
    let raw = format!("{}{}{}{}", at(25_000, 96, 40.0), at(50_000, 97, 44.0), at(75_000, 0, 0.0), at(100_000, 1, 0.04));
    let mut flows = FlowFacts::default();
    scan_flow_lines(raw.as_bytes(), from, from + 100_000, None, &mut flows);
    let s = derive_stats("t".into(), metrics(100_000, 0, 1, 0), Trajectory::default(), flows, None, None);

    assert_eq!(s.samples_busy, 2);
    assert_eq!(s.samples_idle, 2);
    assert_eq!(s.gpu_w_busy, Some(42.0), "a flat mean would read 21.0");
    assert_eq!(s.gpu_duty_pct, Some(50.0));
    assert!(s.checks.telemetry_covers_run);
    assert_eq!(s.busy_ms, Some(50_000));
    assert_eq!(s.pkg_w_busy, Some(47.0));
    assert_eq!(s.pkg_j_busy, Some(2_350.0), "47 W across 50 busy seconds");
    assert_eq!(s.thermal_states_busy.get("nominal"), Some(&2));
    assert_eq!(s.cpu_speed_limit_min, Some(100));
    assert_eq!(s.throttled_samples, 0);
    assert_eq!(s.mem_pct_busy_max, Some(62));
    assert!(s.checks.have_telemetry_samples);
}

/// The sampler runs continuously; only samples inside the run's own window
/// describe the run.
#[test]
fn telemetry_outside_the_run_window_is_ignored() {
    let raw = format!("{}{}{}", telem(50, 96, 40.0), telem(150, 96, 40.0), telem(250, 96, 40.0));
    let mut flows = FlowFacts::default();
    scan_flow_lines(raw.as_bytes(), 100, 200, None, &mut flows);
    assert_eq!(flows.samples.len(), 1);
}

/// No telemetry means the power arm has no data — said plainly, rather than
/// reported as zero watts.
#[test]
fn absent_telemetry_reads_as_absent_not_as_zero_watts() {
    let s = stats("", metrics(1_000, 0, 1, 0));
    assert_eq!(s.gpu_w_busy, None);
    assert_eq!(s.pkg_j_per_1k_tokens, None);
    assert!(!s.checks.have_telemetry_samples);
    assert!(s.unreconciled().iter().any(|r| r.contains("no host telemetry")));
}

// ---------------------------------------------------------------------------
// End to end over a run directory
// ---------------------------------------------------------------------------

/// Flow files are named by UTC date. A reader that picks files by NAME
/// returns nothing for a run that crossed midnight, or for any run read the
/// next morning — so files are chosen by what they could contain, and both
/// days' files are read here.
#[test]
fn a_run_that_crossed_midnight_reads_both_days_files() {
    let run = tempfile::TempDir::new().unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({
            "model": "m", "result": "stop", "started_at_unix_ms": 1_000,
            "wall_ms": 10_000, "rest_ms": 2_000, "turns": 1,
            "compactions": 0, "total_completion_tokens": 300
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("trajectory.jsonl"),
        // The 2 s of rest in metrics.json is recorded in the trajectory too;
        // the cross-check requires both to say it.
        format!("{}{}{}", stream(1, 1_000, Some(4_000)), completed(1, Some(300), 0), rest(2_000)),
    )
    .unwrap();
    std::fs::write(
        run.path().join("lifecycle.json"),
        serde_json::json!({"session_id": "sid-1"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("manifest.json"),
        serde_json::json!({
            // The shape the manifest actually writes — `{passed, details}`,
            // not a bare string.
            "verify": {"passed": true, "details": "verify command exited 0"},
            "ok": true, "session_id": "sid-1"
        })
        .to_string(),
    )
    .unwrap();

    // Yesterday's file carries the dispatch bounds; today's carries a busy
    // sample. A one-file reader misses whichever it did not name.
    std::fs::write(
        flows.path().join("2026-09-21.jsonl"),
        line(serde_json::json!({
            "action": "dispatch start", "session_id": "sid-1",
            "payload": {"bounds": {"max_tokens_per_call": {"value": 32_000, "source": "built-in"}}}
        })),
    )
    .unwrap();
    std::fs::write(flows.path().join("2026-09-22.jsonl"), telem(5_000, 96, 30.0)).unwrap();

    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.verify.as_deref(), Some("pass"));
    assert_eq!(s.ok, Some(true));
    assert_eq!(s.active_ms, 8_000);
    assert_eq!(s.tok_per_s, Some(100.0), "300 tokens over the 3s stream");
    assert_eq!(s.samples_busy, 1, "today's telemetry");
    assert_eq!(s.flow_records_in_window, 1, "yesterday's session record");
    assert!(s.bounds.contains_key("max_tokens_per_call"), "bounds from yesterday's file");
    assert_eq!(s.checks.tokens_reconcile, Some(true));
    assert!(s.unreconciled().is_empty(), "a clean run quotes cleanly: {:?}", s.unreconciled());
}

// ---------------------------------------------------------------------------
// Stale metrics.json (#2855 review)
// ---------------------------------------------------------------------------

/// `metrics.json` claiming less wall time than the trajectory spent
/// generating is a metrics file for a DIFFERENT, shorter run — measured:
/// wall 439s printed beside "1234s of 1234s" of generation.
#[test]
fn stale_metrics_is_flagged_when_generation_exceeds_the_claimed_wall_time() {
    let traj = format!("{}{}", stream(1, 0, Some(500_000)), completed(1, Some(100), 5));
    // wall_ms claims 100s but the stream alone ran 500s.
    let s = stats(&traj, metrics(100_000, 0, 1, 100));
    assert!(s.checks.metrics_stale);
    assert!(s.unreconciled().iter().any(|c| c.contains("does not belong to this run")));
}

/// The ordinary case — generation comfortably inside the claimed wall time —
/// must not be flagged.
#[test]
fn a_run_whose_generation_fits_inside_its_wall_time_is_not_flagged_stale() {
    let traj = format!("{}{}", stream(1, 0, Some(5_000)), completed(1, Some(100), 5));
    let s = stats(&traj, metrics(100_000, 0, 1, 100));
    assert!(!s.checks.metrics_stale);
}

/// The other half of the detection: `metrics.json`'s own clock disagrees
/// with the run's OWN identity (the epoch embedded in its run id) by more
/// than the slack. This is the shape actually found on disk — 11 run dirs
/// with a `metrics.json` that started before the run's own id timestamp, 4
/// of them byte-identical copies from days earlier.
#[test]
fn stale_metrics_is_flagged_when_its_clock_disagrees_with_the_runs_own_id() {
    let flows = tempfile::TempDir::new().unwrap();
    let runs = tempfile::TempDir::new().unwrap();
    // The run's OWN identity: epoch 1_780_000 seconds.
    let run_dir = runs.path().join("long-agentic-balanced-1780000000-1");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("metrics.json"),
        // metrics.json's clock: ~5 days EARLIER than the run's own id.
        serde_json::json!({
            "started_at_unix_ms": 1_779_570_000_000u64, "wall_ms": 10_000, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(&run_dir, flows.path()).unwrap();
    assert!(s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// A `metrics.json` clock a few seconds off its run's own id (ordinary
/// dispatch-startup latency) stays within slack and is not flagged.
#[test]
fn a_metrics_clock_within_slack_of_the_runs_own_id_is_not_flagged() {
    let flows = tempfile::TempDir::new().unwrap();
    let runs = tempfile::TempDir::new().unwrap();
    let run_dir = runs.path().join("long-agentic-balanced-1780000000-1");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("metrics.json"),
        serde_json::json!({
            // 3 seconds after the id's own stamp — ordinary startup lag.
            "started_at_unix_ms": 1_780_000_003_000u64, "wall_ms": 10_000, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(&run_dir, flows.path()).unwrap();
    assert!(!s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// (Frontier review, 2026-09-23) A 1ms gap between generation and claimed
/// wall time is clock jitter between two different clocks (trajectory
/// stream timestamps vs the runtime's own wall-clock stamp), not a
/// different run's metrics file. Pinned to the exact real numbers measured
/// on disk for `long-agentic-balanced-1779702243-1` (wall 23605ms, gen
/// 23606ms) — it owns its own metrics and must not be flagged.
#[test]
fn a_one_millisecond_generation_jitter_over_wall_is_not_flagged_stale() {
    let traj = format!("{}{}", stream(1, 0, Some(23_606)), completed(1, Some(100), 5));
    let s = stats(&traj, metrics(23_605, 0, 1, 100));
    assert!(!s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// Same shape, the other real example: wall 1219ms, gen 1220ms
/// (`long-agentic-balanced-1779802198-1`).
#[test]
fn a_one_millisecond_generation_jitter_on_a_short_run_is_not_flagged_stale() {
    let traj = format!("{}{}", stream(1, 0, Some(1_220)), completed(1, Some(100), 5));
    let s = stats(&traj, metrics(1_219, 0, 1, 100));
    assert!(!s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// (Frontier review, 2026-09-23) A symmetric slack let a `metrics.json`
/// starting ~90s BEFORE its own run's id pass as identity, because 90s sits
/// comfortably inside a 10-minute window either direction. That is exactly
/// `medium-coding-deep-1779688920-1` on disk — a byte-identical copy of
/// `…-1779688829-1`'s metrics, un-flagged before this fix. A clock claiming
/// a start before the run was even minted is impossible, so the tolerance
/// on that side must be tight, not symmetric with the generous AFTER side.
#[test]
fn a_metrics_clock_90_seconds_before_its_own_id_is_flagged_stale() {
    let flows = tempfile::TempDir::new().unwrap();
    let runs = tempfile::TempDir::new().unwrap();
    let run_dir = runs.path().join("medium-coding-deep-1779688920-1");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("metrics.json"),
        serde_json::json!({
            // 89.843s BEFORE the id's own stamp (1_779_688_920_000) —
            // the exact real gap measured on disk.
            "started_at_unix_ms": 1_779_688_830_157u64, "wall_ms": 68_953, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(&run_dir, flows.path()).unwrap();
    assert!(s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// The genuine owner of that same metrics content (id
/// `…-1779688829-1`, whose own epoch is only ~1.157s before the metrics
/// clock) must NOT be flagged — the fix is one-sided, not just tighter.
#[test]
fn the_genuine_owner_of_an_early_metrics_clock_is_not_flagged() {
    let flows = tempfile::TempDir::new().unwrap();
    let runs = tempfile::TempDir::new().unwrap();
    let run_dir = runs.path().join("medium-coding-deep-1779688829-1");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("metrics.json"),
        serde_json::json!({
            "started_at_unix_ms": 1_779_688_830_157u64, "wall_ms": 68_953, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(&run_dir, flows.path()).unwrap();
    assert!(!s.checks.metrics_stale, "checks: {:?}", s.checks);
}

/// (#2855 review) `lifecycle.json`'s `started_at_ms`, when present, is the
/// run's identity — NOT the id-embedded epoch, even when they disagree.
/// Rigged so the two sources give OPPOSITE verdicts: the id epoch alone
/// would call this run clean, but lifecycle.json (which must win) calls it
/// stale. Pins the precedence rather than just exercising the fallback.
#[test]
fn lifecycle_started_at_wins_over_the_id_epoch_when_they_disagree() {
    let flows = tempfile::TempDir::new().unwrap();
    let runs = tempfile::TempDir::new().unwrap();
    // Id epoch: 1_780_000_000s. metrics.json claims 500s later — well
    // within slack of the ID ALONE, so if the id epoch were used this run
    // reads clean.
    let run_dir = runs.path().join("long-agentic-balanced-1780000000-1");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("metrics.json"),
        serde_json::json!({
            "started_at_unix_ms": 1_780_000_500_000u64, "wall_ms": 10_000, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    // lifecycle.json's own identity is ~1000s EARLIER than metrics.json's
    // claim — if lifecycle wins, that puts metrics.json's claim well past
    // lifecycle's own `STALE_METRICS_SLACK_MS` budget.
    std::fs::write(
        run_dir.join("lifecycle.json"),
        serde_json::json!({"started_at_ms": 1_779_000_000_000u64}).to_string(),
    )
    .unwrap();
    let s = compute_from_dir(&run_dir, flows.path()).unwrap();
    assert!(s.checks.metrics_stale, "lifecycle.json must win over the id epoch: {:?}", s.checks);
}

/// A run without metrics has no derivable numbers, and says so
/// rather than returning a page of zeros.
#[test]
fn a_run_without_metrics_is_an_error_not_a_page_of_zeros() {
    let run = tempfile::TempDir::new().unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let err = compute_from_dir(run.path(), flows.path()).unwrap_err();
    assert!(err.to_string().contains("metrics.json"), "got: {err}");
}

/// A missing flow directory is not an error — it means no power arm.
#[test]
fn a_missing_flow_directory_is_not_an_error() {
    let run = tempfile::TempDir::new().unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({"wall_ms": 1_000, "turns": 0}).to_string(),
    )
    .unwrap();
    let s = compute_from_dir(run.path(), Path::new("/nonexistent-flows")).unwrap();
    assert!(!s.checks.have_telemetry_samples);
    assert_eq!(s.wall_ms, 1_000);
}

// ---------------------------------------------------------------------------
// The scan is bounded by the run, not by the archive
// ---------------------------------------------------------------------------

/// A minute, in the same unit as the run window.
const MIN: u64 = 60_000;

/// Write a flow file and stamp its modification time, so a test controls
/// what the file-selection bound sees.
fn flow_file(dir: &Path, name: &str, body: &str, mtime_ms: u64) {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms))
        .unwrap();
}

/// A run directory whose window is `[start, start + wall]`.
fn run_at(start: u64, wall: u64) -> tempfile::TempDir {
    let run = tempfile::TempDir::new().unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({
            "started_at_unix_ms": start, "wall_ms": wall, "rest_ms": 0,
            "turns": 1, "total_completion_tokens": 0
        })
        .to_string(),
    )
    .unwrap();
    run
}

/// Junk the scan would have to parse if it opened the file: many lines of
/// real-shaped records, none of them this run's.
fn filler(n: usize, ts: u64) -> String {
    (0..n).map(|i| telem(ts + i as u64, 0, 0.0)).collect()
}

/// A file last written before the run began cannot contain any of it, and is
/// never opened. Without this bound one `stats` call read 289 MB across 125
/// files to describe a run that lasted four minutes.
#[test]
fn a_flow_file_last_written_before_the_run_is_never_opened() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    let flows = tempfile::TempDir::new().unwrap();
    flow_file(flows.path(), "2026-09-20.jsonl", &filler(500, 10 * MIN), 20 * MIN);
    flow_file(flows.path(), "2026-09-21.jsonl", &filler(500, 500 * MIN), 600 * MIN);
    flow_file(flows.path(), "2026-09-22.jsonl", &telem(start + MIN, 96, 40.0), start + 20 * MIN);

    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_scan.files_total, 3);
    assert_eq!(s.flow_scan.files_skipped, 2, "both older files skipped unopened");
    assert_eq!(s.flow_scan.files_read, 1);
    assert_eq!(s.flow_scan.lines_scanned, 1);
    assert_eq!(s.samples_busy, 1, "and the run's own sample is still found");
}

/// A file is abandoned once its own clock passes the end of the window.
/// Files are append-only, so everything after that line is later still.
#[test]
fn the_scan_stops_once_the_file_clock_passes_the_window() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    let flows = tempfile::TempDir::new().unwrap();
    let body = format!(
        "{}{}{}",
        telem(start + MIN, 96, 40.0),
        telem(start + 60 * MIN, 0, 0.0), // an hour past: the clock has passed
        filler(5_000, start + 61 * MIN),
    );
    flow_file(flows.path(), "2026-09-22.jsonl", &body, start + 24 * 60 * MIN);

    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_scan.files_stopped_early, 1);
    assert_eq!(s.flow_scan.lines_scanned, 2, "the 5,000 lines after the stop were not read");
    assert_eq!(s.samples_busy, 1);
}

/// The whole point, stated as a property: growing the archive does not grow
/// the work. Ten old files or a thousand, one `stats` call reads the same
/// lines.
#[test]
fn the_cost_of_a_stats_call_does_not_grow_with_the_archive() {
    let start = 100_000 * MIN;
    let lines_for = |old_files: usize| {
        let run = run_at(start, 10 * MIN);
        let flows = tempfile::TempDir::new().unwrap();
        for i in 0..old_files {
            let t = (i as u64 + 1) * 60 * MIN;
            flow_file(flows.path(), &format!("old-{i:04}.jsonl"), &filler(200, t), t + MIN);
        }
        flow_file(
            flows.path(),
            "current.jsonl",
            &format!("{}{}", telem(start + MIN, 96, 40.0), filler(200, start + 90 * MIN)),
            start + 120 * MIN,
        );
        compute_from_dir(run.path(), flows.path()).unwrap().flow_scan.lines_scanned
    };
    assert_eq!(lines_for(10), lines_for(200));
    assert_eq!(lines_for(200), 2);
}

/// Session records land a little outside the runtime's own window: the host
/// writes `dispatch start` before the runtime stamps `started_at_unix_ms`.
/// The slack keeps them; the telemetry bound does not widen with it.
#[test]
fn the_window_slack_keeps_session_records_but_not_outside_telemetry() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    std::fs::write(run.path().join("lifecycle.json"), r#"{"session_id":"sid-9"}"#).unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let body = format!(
        "{}{}{}",
        telem(start - 2 * MIN, 96, 40.0), // before the run: not a run sample
        line(serde_json::json!({
            "action": "dispatch start", "session_id": "sid-9",
            "payload": {"bounds": {"max_turns": {"value": null, "source": "built-in"}}}
        })),
        telem(start + MIN, 96, 40.0),
    );
    // The session record precedes the run start; the slack keeps it.
    flow_file(flows.path(), "d.jsonl", &body, start + 2 * MIN);
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_records_in_window, 1);
    assert!(s.bounds.contains_key("max_turns"));
    assert_eq!(s.samples_busy, 1, "the pre-run sample is not averaged into the run");
}

// ---------------------------------------------------------------------------
// Merge-gate review, 2026-09-23. Each test below reproduces a PROVEN finding:
// a wrong figure that passed every check, or a guard no test pinned.
// ---------------------------------------------------------------------------

/// Review MF1: `aborts` counted distinct SEQS. A turn aborted twice (the
/// retry was aborted too) is two aborts. Measured: a real run with four
/// abort records reported three.
#[test]
fn every_abort_counts_not_every_aborted_turn() {
    let abort = |seq: u64| line(serde_json::json!({"type": "dispatch.gate.abort", "seq": seq}));
    let traj = format!("{}{}{}", abort(2), abort(6), abort(6));
    let s = stats(&traj, metrics(10_000, 0, 6, 0));
    assert_eq!(s.gates.stream.aborts, 3);
    assert_eq!(s.turns_with_stream_abort, vec![2, 6]);
}

fn rest(ms: u64) -> String {
    line(serde_json::json!({"type": "runtime.rest", "ms": ms, "reason": "thermal-duty-cycle", "state": "fair"}))
}

/// Review MF2: the runtime's error path writes `metrics.json` with turns and
/// rest hardcoded to zero, so an errored run printed "rest 0s, 0 turns"
/// beside three rests and 234k tokens, active time overstated by the rest it
/// hid, and no caveat. The trajectory holds the real counts.
#[test]
fn an_errored_run_takes_turns_and_rest_from_the_trajectory() {
    let traj = format!(
        "{}{}{}{}{}{}",
        stream(1, 0, Some(10_000)),
        completed(1, Some(100), 0),
        rest(15_000),
        stream(2, 30_000, Some(40_000)),
        completed(2, Some(200), 0),
        rest(15_000),
    );
    let mut m = metrics(100_000, 0, 0, 0);
    m.result = Some("error".into());
    let s = stats(&traj, m);
    assert_eq!(s.turns, 2);
    assert_eq!(s.rest_ms, 30_000);
    assert_eq!(s.active_ms, 70_000);
    assert!(s.checks.metrics_totals_zeroed);
    assert_eq!(s.checks.tokens_reconcile, None, "there is no total to reconcile against");
    assert!(s.unreconciled().iter().any(|r| r.contains("zeroed")));
}

/// Outside the error path the two sources must agree; when they do not, the
/// run says so instead of quietly picking one.
#[test]
fn a_run_whose_metrics_disagree_with_its_trajectory_is_flagged() {
    let traj = format!("{}{}{}", stream(1, 0, Some(1_000)), completed(1, Some(10), 0), rest(15_000));
    let s = stats(&traj, metrics(100_000, 0, 1, 10));
    assert_eq!(s.checks.rest_matches_trajectory, Some(false));
    assert_eq!(s.checks.turns_match_trajectory, Some(true));
    assert!(s.unreconciled().iter().any(|r| r.contains("rest")));
}

/// Review MF3: the "longest span on an aborted seq" guess ran even when null
/// frames had already identified the unbilled span exactly, so a SHORT abort
/// followed by a LONG billed retry marked the retry unbilled. Measured in the
/// probe: 700 tok/s against a true 100.
#[test]
fn a_short_abort_then_a_long_retry_keeps_the_retry_billed() {
    let traj = format!(
        "{}{}{}{}{}{}{}",
        stream(2, 0, Some(5_000)),
        completed(2, None, 0),
        stream(2, 5_000, Some(65_000)),
        completed(2, Some(6_000), 0),
        line(serde_json::json!({"type": "dispatch.gate.abort", "seq": 2})),
        stream(3, 70_000, Some(80_000)),
        completed(3, Some(1_000), 0),
    );
    let s = stats(&traj, metrics(100_000, 0, 3, 7_000));
    assert_eq!(s.streams_unbilled, 1);
    assert_eq!(s.gen_ms_billed, 70_000);
    assert_eq!(s.tok_per_s, Some(100.0));
}

/// When frames and streams do NOT pair, null-frame indexes are not trusted:
/// a frame missing from the middle would shift every later index.
#[test]
fn a_null_frame_is_not_paired_by_index_when_counts_differ() {
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(10_000)),
        completed(1, None, 0),
        stream(2, 10_000, Some(20_000)),
        stream(3, 20_000, Some(30_000)),
    );
    let s = stats(&traj, metrics(40_000, 0, 3, 0));
    assert!(!s.checks.frames_match_streams);
    assert_eq!(s.streams_unbilled, 0, "index pairing is off when the counts differ");
}

fn telem_at(ts: u64, gpu_pct: u64, interval_ms: u64) -> String {
    line(serde_json::json!({
        "action": "machine.telemetry",
        "payload": {
            "sampled_at_ms": ts, "gpu_pct": gpu_pct, "interval_ms": interval_ms,
            "power_mw": {"gpu": 30_000, "cpu": 5_000, "total": 35_000}
        }
    }))
}

/// Review MF4: two samples in the last ten seconds of a ten-minute run
/// (the daemon started late) extrapolated to 600 s busy and 21 kJ, with no
/// caveat. Busy time and energy are withheld when telemetry does not cover
/// the run.
#[test]
fn sparse_telemetry_withholds_busy_time_and_energy() {
    let from = 1_000_000; // `metrics()`'s start
    let raw = format!("{}{}", telem_at(from + 595_000, 96, 5_000), telem_at(from + 600_000, 96, 5_000));
    let mut flows = FlowFacts::default();
    scan_flow_lines(raw.as_bytes(), from, from + 600_000, None, &mut flows);
    let s = derive_stats("t".into(), metrics(600_000, 0, 1, 0), Trajectory::default(), flows, None, None);
    assert!(!s.checks.telemetry_covers_run);
    assert!(s.telemetry_max_gap_ms.unwrap() >= 590_000);
    assert_eq!(s.busy_ms, None);
    assert_eq!(s.pkg_j_busy, None);
    assert!(s.unreconciled().iter().any(|r| r.contains("telemetry")));
}

/// The sampler slows to ~51 s when idle and runs at ~5 s while busy, so
/// counting SAMPLES over-weights busy time. Each sample is weighted by the
/// interval it closes (`interval_ms`, measured equal to the actual gap).
#[test]
fn duty_is_weighted_by_time_not_by_sample_count() {
    let from = 1_000_000; // `metrics()`'s start
    let mut raw = String::new();
    for i in 1..=6 {
        raw.push_str(&telem_at(from + i * 5_000, 96, 5_000)); // 30 s busy
    }
    raw.push_str(&telem_at(from + 60_000, 0, 30_000)); // 30 s idle, one sample
    let mut flows = FlowFacts::default();
    scan_flow_lines(raw.as_bytes(), from, from + 60_000, None, &mut flows);
    let s = derive_stats("t".into(), metrics(60_000, 0, 1, 0), Trajectory::default(), flows, None, None);
    assert_eq!(s.gpu_duty_pct, Some(50.0), "by count it would read 85.7%");
    assert!(s.checks.telemetry_covers_run);
}

/// Review C5: `cumulative_chars` restarts with every stream, so the per-seq
/// maximum dropped every stream but the largest on a retried turn.
#[test]
fn content_chars_sum_across_streams_on_one_seq() {
    let partial = |n: u64| line(serde_json::json!({"type": "model.partial", "seq": 1, "cumulative_chars": n}));
    let traj = format!(
        "{}{}{}{}",
        stream(1, 0, Some(1_000)),
        partial(3_000),
        stream(1, 1_000, Some(2_000)),
        partial(2_000),
    );
    let s = stats(&traj, metrics(5_000, 0, 1, 0));
    assert_eq!(s.content_chars, 5_000);
}

/// Review C6: one invalid byte made `read_to_string` fail and the whole day
/// file vanished, counted as neither read nor skipped.
#[test]
fn an_undecodable_flow_file_is_still_read() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    let flows = tempfile::TempDir::new().unwrap();
    let mut body = b"{\"action\":\"x\",\"payload\":{\"note\":\"\xff\xfe\"}}\n".to_vec();
    body.extend_from_slice(telem(start + MIN, 96, 40.0).as_bytes());
    let path = flows.path().join("d.jsonl");
    std::fs::write(&path, &body).unwrap();
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_scan.files_read, 1);
    assert_eq!(s.samples_busy, 1);
    assert_eq!(
        s.flow_scan.files_total,
        s.flow_scan.files_read + s.flow_scan.files_skipped + s.flow_scan.files_unreadable
    );
}

/// Review C7: the stop bound's slack was unpinned. A session record written
/// just after the run ends, after a telemetry line already past `run_to`,
/// is still this run's.
#[test]
fn a_session_record_just_after_the_run_is_kept() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    std::fs::write(run.path().join("lifecycle.json"), r#"{"session_id":"sid-3"}"#).unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let body = format!(
        "{}{}",
        telem(start + 11 * MIN, 0, 0.0),
        line(serde_json::json!({"action": "dispatch complete", "session_id": "sid-3", "payload": {}})),
    );
    flow_file(flows.path(), "d.jsonl", &body, start + 12 * MIN);
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_records_in_window, 1);
}

/// Review C7: the open bound's slack was unpinned. The host writes `dispatch
/// start` before the runtime stamps its start; a file last written in that
/// gap still holds the run's bounds.
#[test]
fn a_file_last_written_just_before_the_run_is_still_opened() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    std::fs::write(run.path().join("lifecycle.json"), r#"{"session_id":"sid-4"}"#).unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let body = line(serde_json::json!({
        "action": "dispatch start", "session_id": "sid-4",
        "payload": {"bounds": {"max_turns": {"value": null, "source": "built-in"}}}
    }));
    flow_file(flows.path(), "d.jsonl", &body, start - 2 * MIN);
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert!(s.bounds.contains_key("max_turns"));
}

/// Review C8: an implausible chars-per-token turn was computed and never
/// shown; a run with no flow records raised nothing.
#[test]
fn suspect_turns_and_missing_flow_records_are_caveats() {
    let traj = format!(
        "{}{}{}",
        stream(1, 0, Some(1_000)),
        completed(1, Some(500), 10),
        line(serde_json::json!({"type": "model.reasoning", "seq": 1, "reasoning_chars": 9_000})),
    );
    let s = stats(&traj, metrics(2_000, 0, 1, 500));
    let u = s.unreconciled();
    assert!(u.iter().any(|r| r.contains("chars per token")), "{u:?}");
    assert!(u.iter().any(|r| r.contains("flow records")), "{u:?}");
}

/// Review M1: records written before `would_conclude` existed carry only the
/// verdict. A conclusion there still counts as the finding.
#[test]
fn an_older_checkpoint_without_would_conclude_still_counts_its_conclusion() {
    let traj = line(serde_json::json!({"type": "dispatch.checkpoint", "seq": 1, "tail_ratio": 0.1, "verdict": "conclude"}));
    let s = stats(&traj, metrics(1_000, 0, 1, 0));
    assert_eq!(s.gates.checkpoint.degenerate_turns, vec![1]);
}

/// Review M9: rest longer than wall is impossible; the check must fail.
#[test]
fn rest_longer_than_wall_fails_its_check() {
    let s = stats("", metrics(1_000, 5_000, 1, 0));
    assert!(!s.checks.rest_within_wall);
}

/// Review M10: a second end record for one stream must not re-time it.
#[test]
fn a_duplicate_end_does_not_re_time_a_closed_stream() {
    let traj = format!(
        "{}{}",
        stream(1, 0, Some(1_000)),
        line(serde_json::json!({"type": "model.streaming.end", "seq": 1, "ts": 9_000})),
    );
    let s = stats(&traj, metrics(10_000, 0, 1, 0));
    assert_eq!(s.gen_ms_all, 1_000);
}

/// Review M17: the runtime's comparison is strict. A ratio of exactly the
/// threshold is NOT degenerate.
#[test]
fn a_ratio_exactly_at_the_threshold_is_not_degenerate() {
    let traj = line(serde_json::json!({
        "type": "dispatch.checkpoint", "seq": 1, "tail_ratio": DEGENERATE_TAIL_RATIO,
        "verdict": "continue", "would_conclude": false
    }));
    let s = stats(&traj, metrics(1_000, 0, 1, 0));
    assert!(s.gates.checkpoint.degenerate_turns_by_ratio.is_empty());
    assert!(s.checks.verdict_matches_ratio);
}

/// Review C12: a run id that does not resolve read as "a run without runtime
/// metrics", which sent the reader looking in the wrong place.
#[test]
fn a_run_that_does_not_exist_says_so() {
    let err = compute_from_dir(Path::new("/nonexistent/run-xyz"), Path::new("/nonexistent")).unwrap_err();
    assert!(err.to_string().contains("no run directory"), "got: {err}");
}

/// Review M14: when frames and streams do NOT pair, the fallback marks the
/// LONGEST span on an aborted seq as the unbilled one (the gate fires at the
/// end of the run it killed). Nothing pinned longest over shortest.
#[test]
fn without_a_pairing_the_longest_span_on_an_aborted_seq_is_the_unbilled_one() {
    let traj = format!(
        "{}{}{}{}{}{}",
        stream(2, 0, Some(50_000)), // the aborted stream: no usage frame at all
        line(serde_json::json!({"type": "dispatch.gate.abort", "seq": 2})),
        stream(2, 50_000, Some(55_000)),
        completed(2, Some(500), 0),
        stream(3, 60_000, Some(70_000)),
        completed(3, Some(1_000), 0),
    );
    let s = stats(&traj, metrics(80_000, 0, 3, 1_500));
    assert!(!s.checks.frames_match_streams, "three streams, two frames");
    assert_eq!(s.streams_unbilled, 1);
    assert_eq!(s.gen_ms_billed, 15_000, "the 5 s retry and the 10 s turn");
}

fn with_policy_bound(value: &str) -> FlowFacts {
    let mut f = FlowFacts::default();
    f.bounds.insert(
        "detection_degeneracy_policy".into(),
        serde_json::json!({"value": value, "source": "env"}),
    );
    f
}

/// A run with no checkpoint records (a clean run, or any run under `off`)
/// showed no policy at all, though `dispatch start.bounds` recorded it. That
/// made a healthy `enforce` run and an `off` run look identical, the exact
/// ambiguity the detector policy exists to remove. Measured on a real
/// `observe` run with no checkpoints.
#[test]
fn the_policy_is_read_from_the_bounds_when_no_checkpoint_recorded_it() {
    let s = derive_stats("t".into(), metrics(1_000, 0, 0, 0), Trajectory::default(), with_policy_bound("off"), None, None);
    assert_eq!(s.gates.checkpoint.policy.as_deref(), Some("off"));
    assert_eq!(s.checks.policy_consistent, None, "one source, nothing to compare");
}

/// The host resolves the policy it records in the bounds, but the runtime
/// reads its OWN environment inside the container. When the two disagree,
/// the policy that ran is not the one the operator's settings say.
#[test]
fn a_policy_the_runtime_did_not_run_is_flagged() {
    let traj = line(serde_json::json!({
        "type": "dispatch.checkpoint", "seq": 1, "tail_ratio": 0.9,
        "verdict": "continue", "would_conclude": false, "policy": "observe"
    }));
    let s = derive_stats("t".into(), metrics(1_000, 0, 1, 0), parse_trajectory(&traj), with_policy_bound("enforce"), None, None);
    assert_eq!(s.gates.checkpoint.policy.as_deref(), Some("observe"), "what ran wins");
    assert_eq!(s.checks.policy_consistent, Some(false));
    assert!(s.unreconciled().iter().any(|r| r.contains("policy")));
}

// ---------------------------------------------------------------------------
// Re-review of the coverage rule, 2026-09-23.
// ---------------------------------------------------------------------------

/// Coverage over `from..from+wall` for raw telemetry lines.
fn coverage_of(raw: &str, wall: u64) -> RunStats {
    let from = 1_000_000; // `metrics()`'s start
    let mut flows = FlowFacts::default();
    scan_flow_lines(raw.as_bytes(), from, from + wall, None, &mut flows);
    derive_stats("t".into(), metrics(wall, 0, 1, 0), Trajectory::default(), flows, None, None)
}

fn old_telem(off: u64, gpu: u64) -> String {
    // Telemetry as recorded before 2026-09-05: no `interval_ms`.
    line(serde_json::json!({
        "action": "machine.telemetry",
        "payload": {"sampled_at_ms": 1_000_000 + off, "gpu_pct": gpu,
                    "power_mw": {"gpu": 30_000, "cpu": 5_000, "total": 35_000}}
    }))
}

fn new_telem(off: u64, gpu: u64, interval: u64) -> String {
    telem_at(1_000_000 + off, gpu, interval)
}

/// Re-review MF1: a sample with no `interval_ms` reached all the way back to
/// the previous sample or the window start, so no gap was ever visible. Every
/// run recorded before 2026-09-05 is like this. Two samples in the last ten
/// seconds of a ten-minute run passed as 600 s busy and 27 kJ.
#[test]
fn old_telemetry_without_an_interval_still_shows_its_gaps() {
    let s = coverage_of(&format!("{}{}", old_telem(590_000, 96), old_telem(595_000, 96)), 600_000);
    assert!(!s.checks.telemetry_covers_run);
    assert_eq!(s.busy_ms, None);
}

/// Re-review MF1, second shape: an unstamped first sample claimed the whole
/// unobserved start of the run, so duty was made up.
#[test]
fn an_unstamped_first_sample_does_not_claim_the_unobserved_start() {
    let mut raw = old_telem(300_000, 0);
    for i in 1..=60 {
        raw.push_str(&new_telem(300_000 + i * 5_000, 96, 5_000));
    }
    let s = coverage_of(&raw, 600_000);
    assert!(!s.checks.telemetry_covers_run);
}

/// Re-review N4: the tail after the last sample is the only thing that sees
/// a sampler that stopped partway through a run.
#[test]
fn a_sampler_that_stops_partway_fails_coverage() {
    let raw: String = (1..=60).map(|i| new_telem(i * 5_000, 96, 5_000)).collect();
    let s = coverage_of(&raw, 600_000); // samples end at 300 s
    assert!(!s.checks.telemetry_covers_run);
    assert!(s.telemetry_max_gap_ms.unwrap() >= 300_000);
}

/// Re-review N6: the allowance is TWICE the typical interval. A 15 s hole in
/// a 5 s cadence is a hole.
#[test]
fn a_gap_of_three_intervals_fails_coverage() {
    let raw = format!(
        "{}{}{}{}",
        new_telem(5_000, 96, 5_000),
        new_telem(10_000, 96, 5_000),
        new_telem(30_000, 96, 5_000), // stands for 25-30 s: 10-25 s uncovered
        new_telem(35_000, 96, 5_000),
    );
    let s = coverage_of(&raw, 35_000);
    assert_eq!(s.telemetry_max_gap_ms, Some(15_000));
    assert!(!s.checks.telemetry_covers_run);
}

/// Re-review N9: when the sampler switches from busy to idle cadence, the
/// first idle sample is stamped with the idle interval but only the time
/// since the previous sample is new. Counting its whole stamped interval
/// double-counts, and halves this duty.
#[test]
fn an_interval_overlapping_the_previous_sample_is_not_counted_twice() {
    let mut raw: String = (1..=10).map(|i| new_telem(i * 5_000, 96, 5_000)).collect();
    raw.push_str(&new_telem(55_000, 0, 51_000));
    let s = coverage_of(&raw, 55_000);
    assert_eq!(s.gpu_duty_pct, Some(90.9), "50 s busy of 55 s; double-counting reads 49.5");
}

/// Re-review C3: an entry that opens but cannot be read (a directory named
/// like a day file, or a read error midway) was counted as read, silently.
#[test]
fn a_flow_file_that_cannot_be_read_to_the_end_is_counted() {
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    let flows = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(flows.path().join("2026-01-02.jsonl")).unwrap();
    flow_file(flows.path(), "d.jsonl", &telem(start + MIN, 96, 40.0), start + 20 * MIN);
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert_eq!(s.flow_scan.files_read_errors, 1);
    assert!(s.unreconciled().iter().any(|r| r.contains("could not be read to the end")));
}

/// Re-review N14: the `files_unreadable` counter had no test. A file that
/// cannot be opened is counted, so the scan's totals always add up.
#[cfg(unix)]
#[test]
fn a_flow_file_that_cannot_be_opened_is_counted() {
    use std::os::unix::fs::PermissionsExt;
    let start = 1_000 * MIN;
    let run = run_at(start, 10 * MIN);
    let flows = tempfile::TempDir::new().unwrap();
    flow_file(flows.path(), "locked.jsonl", &telem(start + MIN, 96, 40.0), start + 20 * MIN);
    let locked = flows.path().join("locked.jsonl");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(s.flow_scan.files_unreadable, 1);
    assert_eq!(s.flow_scan.files_total, s.flow_scan.files_read + s.flow_scan.files_skipped + s.flow_scan.files_unreadable);
}

/// (#2833) A run whose manifest predates the work gate (`schema_version` <
/// 6) but whose fixture declares `baseline.test_count` must be flagged
/// `verify_ungated` — its `verify: pass` is the old, vacuous "exited 0"
/// signal, not the gated one, and must never be silently compared as if it
/// were.
#[test]
fn a_pre_gate_run_on_a_baselined_fixture_is_flagged_ungated() {
    let run = tempfile::TempDir::new().unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let fixture = tempfile::TempDir::new().unwrap();
    std::fs::write(
        fixture.path().join(".fixture.json"),
        serde_json::json!({"name": "demo", "baseline": {"test_count": 14}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({"wall_ms": 1_000, "turns": 1}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("manifest.json"),
        serde_json::json!({
            "schema_version": 5,
            "verify": {"passed": true, "details": "verify command exited 0"},
            "fixture": {"source_path": fixture.path().display().to_string()},
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert!(s.verify_ungated, "a pre-gate run on a baselined fixture must be flagged");
}

/// The same manifest, but already gated (`schema_version: 6`) — must NOT be
/// flagged, even though the fixture still declares a baseline.
#[test]
fn a_gated_run_on_the_same_fixture_is_not_flagged_ungated() {
    let run = tempfile::TempDir::new().unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let fixture = tempfile::TempDir::new().unwrap();
    std::fs::write(
        fixture.path().join(".fixture.json"),
        serde_json::json!({"name": "demo", "baseline": {"test_count": 14}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({"wall_ms": 1_000, "turns": 1}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("manifest.json"),
        serde_json::json!({
            "schema_version": 6,
            "verify": {"passed": true, "details": "8 test(s) added"},
            "fixture": {"source_path": fixture.path().display().to_string()},
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert!(!s.verify_ungated);
}

/// A pre-gate run on a fixture with NO baseline declared must not be
/// flagged — the gate never would have applied to it anyway.
#[test]
fn a_pre_gate_run_on_an_unbaselined_fixture_is_not_flagged_ungated() {
    let run = tempfile::TempDir::new().unwrap();
    let flows = tempfile::TempDir::new().unwrap();
    let fixture = tempfile::TempDir::new().unwrap();
    std::fs::write(
        fixture.path().join(".fixture.json"),
        serde_json::json!({"name": "demo"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("metrics.json"),
        serde_json::json!({"wall_ms": 1_000, "turns": 1}).to_string(),
    )
    .unwrap();
    std::fs::write(
        run.path().join("manifest.json"),
        serde_json::json!({
            "schema_version": 5,
            "verify": {"passed": true, "details": "verify command exited 0"},
            "fixture": {"source_path": fixture.path().display().to_string()},
        })
        .to_string(),
    )
    .unwrap();
    let s = compute_from_dir(run.path(), flows.path()).unwrap();
    assert!(!s.verify_ungated);
}
