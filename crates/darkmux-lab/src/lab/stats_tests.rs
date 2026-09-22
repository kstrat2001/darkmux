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
    assert!(s.checks.tokens_reconcile, "the per-turn sum must equal the run total");
}

/// The check that caught the undercount: it fails the moment the per-turn
/// sum stops matching `metrics.json`.
#[test]
fn tokens_reconcile_is_false_when_the_run_total_disagrees() {
    let traj = format!("{}{}", stream(1, 0, Some(1_000)), completed(1, Some(100), 0));
    let s = stats(&traj, metrics(2_000, 0, 1, 999));
    assert!(!s.checks.tokens_reconcile);
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
#[test]
fn power_is_averaged_over_busy_samples_only_with_a_duty_cycle() {
    let raw = format!("{}{}{}{}", telem(10, 96, 40.0), telem(20, 97, 44.0), telem(30, 0, 0.0), telem(40, 1, 0.04));
    let mut samples = Vec::new();
    parse_telemetry(&raw, 0, 1_000, &mut samples);
    let flows = FlowFacts { samples, ..Default::default() };
    let s = derive_stats("t".into(), metrics(100_000, 0, 1, 0), Trajectory::default(), flows, None, None);

    assert_eq!(s.samples_busy, 2);
    assert_eq!(s.samples_idle, 2);
    assert_eq!(s.gpu_w_busy, Some(42.0), "a flat mean would read 21.0");
    assert_eq!(s.gpu_duty_pct, Some(50.0));
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
    let mut samples = Vec::new();
    parse_telemetry(&raw, 100, 200, &mut samples);
    assert_eq!(samples.len(), 1);
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

/// Flow files are named by UTC date. A reader that opens "today's" returns
/// nothing for a run that crossed midnight, or for any run read the next
/// morning — so every file is read.
#[test]
fn every_flow_file_is_read_not_just_todays() {
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
        format!("{}{}", stream(1, 1_000, Some(4_000)), completed(1, Some(300), 0)),
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
    assert_eq!(s.flow_records, 1, "yesterday's session record");
    assert!(s.bounds.contains_key("max_tokens_per_call"), "bounds from yesterday's file");
    assert!(s.checks.tokens_reconcile);
    assert!(s.unreconciled().is_empty(), "a clean run quotes cleanly: {:?}", s.unreconciled());
}

/// A run without runtime metrics has no derivable numbers, and says so
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
