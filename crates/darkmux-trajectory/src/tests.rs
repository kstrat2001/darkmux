use super::*;

/// Lines copied from real run directories (text fields shortened). They are
/// the append-only archive this crate must keep reading.
const ARCHIVE: &[&str] = &[
    r#"{"model":"darkmux:qwen3.6-35b-a3b-turboquant-mlx","prompt_chars":3202,"system_chars":13159,"tools":["search","read","edit","write","bash","create_mod"],"ts":1790328432569,"type":"dispatch.start"}"#,
    r#"{"prompt_chars":3202,"seq":1,"system_chars":13159,"ts":1790328432570,"type":"model.streaming.start"}"#,
    r#"{"cumulative_chars":0,"delta_chars":0,"generated_chars":3,"partial_index":1,"seq":1,"tool_calls_present":false,"ts":1790328440561,"type":"model.partial"}"#,
    r#"{"chars_per_token":null,"observations":0,"partial_count":139,"seq":1,"tool_calls_count":5,"total_content_chars":6,"ts":1790328444429,"type":"model.streaming.end"}"#,
    r#"{"finish_reason":"tool_calls","seq":1,"tool_calls":[{"arguments_chars":63,"id":"821193764","name":"read"}],"ts":1790328444430,"type":"model.completed","usage":{"cached_tokens":null,"completion_tokens":384,"prompt_tokens":6362,"reasoning_tokens":122,"total_tokens":6746}}"#,
    r#"{"reasoning_chars":578,"reasoning_format":"separate-field","reasoning_text":"The user wants me to perform a QA review","seq":1,"ts":1790328444429,"type":"model.reasoning"}"#,
    r#"{"args":"{\"limit\":0,\"offset\":1,\"path\":\"/workspace","args_chars":63,"emit_seq":null,"emitted":null,"exit_code":null,"failure_reason":null,"ok":true,"outcome":"ok","result":"1: import crypto","result_chars":7036,"seq":1,"tool_name":"read","tool_seq":0,"ts":1790328444431,"type":"tool.completed"}"#,
    r#"{"ms":15000,"reason":"thermal-duty-cycle","seq":13,"state":"fair","ts":1790328616846,"type":"runtime.rest"}"#,
    r#"{"max":262144,"seq":1,"ts":1790328444429,"type":"dispatch.context","used":6362}"#,
    r#"{"canonical_args":"{\"path\":\"/workspace/t.js\"}","code_hash":"d6be","count":3,"seq":7,"tool_name":"edit","ts":1790328522922,"type":"dispatch.cycle.suspected","window_size":10}"#,
    r#"{"message_count":2,"seq":7,"signal_kinds":["cycle_suspected","test_cadence_drift"],"ts":1790328522925,"type":"dispatch.feedback.injected"}"#,
    r#"{"acted":false,"degenerate":false,"interval_tokens":1000,"observation":1,"policy":"enforce","seq":13,"slice_chars":4003,"tail_ratio":1.0,"ts":1790328601125,"type":"dispatch.gate.observation"}"#,
    r#"{"cumulative_chars":113,"generated_chars":117,"partial_index":28,"phase":"writing_tool_call","seq":5,"tool_name":"edit","ts":1790328476087,"type":"model.tool_call.writing"}"#,
    r#"{"result":"stop","ts":1790329043710,"type":"dispatch.complete","wall_ms":611137}"#,
    r#"{"checkpoint":1,"seq":4,"slice_tokens":999,"tail_ratio":0.9935483932495117,"ts":1787667771620,"type":"dispatch.checkpoint","verdict":"continue"}"#,
    r#"{"after_messages":7,"before_messages":12,"generation":1,"summary_chars":1128,"ts":1779707195785,"type":"compaction"}"#,
    r#"{"generated_chars":16005,"interval_tokens":1000,"observation":4,"seq":7,"slice_chars":16005,"ts":1789885827726,"type":"dispatch.gate.abort"}"#,
];

#[test]
fn every_archive_line_parses_to_its_own_variant() {
    use TrajectoryEvent as E;
    let events: Vec<TrajectoryEvent> = ARCHIVE.iter().map(|l| parse_line(l).unwrap_or_else(|| panic!("{l}"))).collect();
    assert!(matches!(&events[0], E::DispatchStart(s) if s.tools.len() == 6 && s.ts == 1790328432569));
    assert!(matches!(&events[4], E::ModelCompleted(m) if m.seq == 1
        && m.usage.as_ref().is_some_and(|u| u.total_tokens == Some(6746) && u.reasoning_tokens == Some(122) && u.cached_tokens.is_none())));
    assert!(matches!(&events[6], E::ToolCompleted(t) if t.ok && t.outcome == Some(ToolOutcomeKind::Ok) && t.emitted.is_none()));
    assert!(matches!(&events[7], E::Rest(r) if r.reason == RestReason::Paced("thermal-duty-cycle".into()) && r.state.as_deref() == Some("fair")));
    assert!(matches!(&events[12], E::ToolCallWriting(w) if w.phase == StreamPhase::WritingToolCall && w.tool_name == "edit"));
    assert!(matches!(&events[13], E::DispatchComplete(c) if c.wall_ms == 611137 && c.turn_delay_effective_ms.is_none()));
    assert!(matches!(&events[14], E::Checkpoint(c) if c.verdict == Verdict::Continue && !c.judged_degenerate()));
    assert!(matches!(&events[16], E::GateAbort(g) if g.seq == 7));
    assert!(events.iter().all(|e| !matches!(e, E::Unknown)), "every archive type is known: {events:?}");
}

#[test]
fn an_unknown_type_reads_as_unknown_and_a_broken_line_is_skipped() {
    assert_eq!(parse_line(r#"{"type":"from.the.future","x":1}"#), Some(TrajectoryEvent::Unknown));
    assert_eq!(parse_line(r#"{"type":"model.completed","seq":1,"usa"#), None, "a partial last line");
    assert_eq!(parse_line("   "), None);
    // A known type with a field of the wrong JSON type: skipped, not guessed.
    assert_eq!(parse_line(r#"{"type":"runtime.rest","ms":"a lot"}"#), None);
}

/// The wire spelling of every event type. Archives are append-only, so a
/// rename would make every recorded run unreadable: this list may only grow.
#[test]
fn the_wire_names_are_fixed() {
    let names = [
        "dispatch.start", "dispatch.complete", "model.streaming.start", "model.partial",
        "model.tool_call.writing", "model.streaming.end", "model.completed", "model.reasoning",
        "tool.completed", "tool_call.promoted", "tool_call.promotion_suppressed", "runtime.rest",
        "compaction.start", "compaction.call", "compaction", "compaction.skipped",
        "compaction.unproductive", "dispatch.context", "dispatch.context.stale_tokens",
        "dispatch.pre_send_bound", "dispatch.checkpoint", "dispatch.gate.observation",
        "dispatch.gate.abort", "dispatch.cycle.suspected", "dispatch.reasoning_loop.suspected",
        "dispatch.reasoning_bound.not_applied", "dispatch.tool.repeated_failure",
        "dispatch.intra_turn_stall.recovered", "dispatch.empty_tool_calls.recovered",
        "dispatch.per_turn_cap.salvaged", "dispatch.tool_call.discarded",
        "dispatch.tool.malformed_names", "dispatch.escalation.triggered",
        "dispatch.feedback.injected", "prompt.submitted",
    ];
    for name in names {
        let e = parse_line(&format!(r#"{{"type":"{name}"}}"#)).unwrap_or_else(|| panic!("{name} does not parse"));
        assert_ne!(e, TrajectoryEvent::Unknown, "{name} is not a known event");
        let written = serde_json::to_value(&e).unwrap();
        assert_eq!(written["type"], name, "{name} is written under another name");
    }
}

#[test]
fn a_written_event_reads_back_equal() {
    let e = TrajectoryEvent::ModelCompleted(ModelCompleted {
        seq: 3,
        ts: 9,
        finish_reason: "tool_calls".into(),
        usage: Some(Usage { prompt_tokens: Some(10), completion_tokens: Some(2), total_tokens: Some(12), reasoning_tokens: None, cached_tokens: Some(4) }),
        tool_calls: Some(vec![ToolCallEntry { id: "a".into(), name: "read".into(), arguments_chars: 5, path: Some("f".into()), runs: Some(false) }]),
        reported_model: None,
        calls_planned: true,
    });
    let line = serde_json::to_string(&e).unwrap();
    assert_eq!(parse_line(&line), Some(e));
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(v.get("reported_model").is_none(), "absent, never null: {line}");
    assert!(v["usage"]["reasoning_tokens"].is_null(), "an unreported count is null, never 0: {line}");
    let rest = TrajectoryEvent::Rest(Rest { seq: 1, ts: 2, ms: 3, reason: RestReason::TurnDelay, state: None });
    let v = serde_json::to_value(&rest).unwrap();
    assert_eq!(v["reason"], "turn_delay");
    assert_eq!(parse_line(&v.to_string()), Some(rest));
}

fn fold(lines: &[&str]) -> TrajectoryFold {
    TrajectoryFold::from_lines(&lines.join("\n"))
}

#[test]
fn a_continuation_is_the_same_turn_and_its_usage_still_counts() {
    let f = fold(&[
        r#"{"type":"model.completed","seq":1,"finish_reason":"length","usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}"#,
        r#"{"type":"model.completed","seq":1,"finish_reason":"stop","usage":{"prompt_tokens":120,"completion_tokens":5,"total_tokens":130}}"#,
        r#"{"type":"model.completed","seq":2,"finish_reason":"stop","usage":null}"#,
    ]);
    assert_eq!(f.turns(), 2, "two logical turns, three calls");
    assert_eq!(f.model_calls, 3);
    assert_eq!((f.tokens.prompt, f.tokens.completion, f.tokens.total), (220, 15, 240));
    assert_eq!(f.frames, vec![Some(10), Some(5), None], "an unbilled call is None, not 0");
    assert_eq!(f.turn_detail[&1].completion_tokens, 15, "a turn's frames accumulate");
    let null_count = fold(&[r#"{"type":"model.completed","seq":1,"usage":{"completion_tokens":null,"total_tokens":7}}"#]);
    assert_eq!(null_count.model_calls, 1, "a null count inside the block still parses");
    assert_eq!(null_count.frames, vec![None], "and reads as unreported, never 0");
    assert_eq!(null_count.tokens.total, 7);
}

#[test]
fn rests_split_routine_from_paced_and_sum_both() {
    let f = fold(&[
        r#"{"type":"runtime.rest","seq":1,"ms":500,"reason":"turn_delay"}"#,
        r#"{"type":"runtime.rest","seq":2,"ms":1000}"#,
        r#"{"type":"runtime.rest","seq":3,"ms":2000,"reason":"paused","state":null}"#,
    ]);
    assert_eq!((f.rest_count(), f.rest_ms(), f.paced_rest_ms()), (3, 3500, 2000));
}

#[test]
fn wall_is_the_runtimes_own_clock_else_the_span_its_events_cover() {
    let done = fold(&[
        r#"{"type":"dispatch.start","ts":1000}"#,
        r#"{"type":"model.completed","seq":1,"ts":5000}"#,
        r#"{"type":"dispatch.complete","ts":6000,"result":"stop","wall_ms":5100}"#,
    ]);
    assert_eq!(done.wall_ms(), Some(5100));
    assert_eq!(done.started_at_ms(), Some(1000));
    let killed = fold(&[
        r#"{"type":"dispatch.start","ts":1000}"#,
        r#"{"type":"model.streaming.start","seq":2,"ts":7000}"#,
    ]);
    assert_eq!(killed.wall_ms(), Some(6000), "a killed run's known span");
    assert_eq!(fold(&[]).wall_ms(), None);
}

#[test]
fn tool_calls_that_never_ran_are_bucketed_by_why() {
    let f = fold(&[
        r#"{"type":"tool.completed","seq":1,"tool_name":"read","ok":true}"#,
        r#"{"type":"tool.completed","seq":1,"tool_name":"bash","ok":false}"#,
        r#"{"type":"tool.completed","seq":2,"tool_name":"read"}"#,
        r#"{"type":"dispatch.tool.malformed_names","seq":2,"count":4,"reason":"not_a_tool"}"#,
        r#"{"type":"dispatch.tool.malformed_names","seq":2,"count":2,"reason":"real_tool_not_granted"}"#,
        r#"{"type":"dispatch.tool.malformed_names","seq":3,"count":1}"#,
    ]);
    assert_eq!((f.tool_calls(), f.tool_calls_failed()), (3, 1), "a call without `ok` predates #469 and succeeded");
    assert_eq!((f.tool_calls_invalid_name, f.tool_calls_ungranted), (5, 2));
}

#[test]
fn streams_pair_with_the_latest_open_one_on_their_turn() {
    let f = fold(&[
        r#"{"type":"model.streaming.start","seq":2,"ts":0}"#,
        r#"{"type":"model.partial","seq":2,"ts":1,"cumulative_chars":40}"#,
        r#"{"type":"model.streaming.end","seq":2,"ts":87100}"#,
        r#"{"type":"model.streaming.start","seq":2,"ts":90000}"#,
        r#"{"type":"model.partial","seq":2,"ts":90001,"cumulative_chars":7}"#,
        r#"{"type":"model.streaming.end","seq":2,"ts":98800}"#,
        r#"{"type":"model.streaming.start","seq":3,"ts":99000}"#,
    ]);
    let ms: Vec<u64> = f.streams.iter().map(Stream::ms).collect();
    assert_eq!(ms, vec![87100, 8800, 0], "an abort and its retry keep their own times");
    assert_eq!(f.generation_ms(2), Some(95900));
    assert_eq!(f.generation_ms(3), None, "an open stream's time is unknown");
    assert_eq!(f.streams.iter().map(|s| s.content_chars).sum::<u64>(), 47, "chars sum across streams");
    let repeated_end = fold(&[
        r#"{"type":"model.streaming.start","seq":1,"ts":0}"#,
        r#"{"type":"model.streaming.end","seq":1,"ts":10}"#,
        r#"{"type":"model.streaming.end","seq":1,"ts":50}"#,
    ]);
    assert_eq!(repeated_end.generation_ms(1), Some(10), "an end only closes a stream that is still open");
}

#[test]
fn checkpoints_and_the_stream_gate_keep_finding_apart_from_action() {
    let f = fold(&[
        r#"{"type":"dispatch.checkpoint","seq":4,"tail_ratio":0.2,"verdict":"continue","would_conclude":true,"policy":"warn"}"#,
        r#"{"type":"dispatch.checkpoint","seq":5,"verdict":"conclude"}"#,
        r#"{"type":"dispatch.gate.observation","seq":6,"tail_ratio":0.4,"degenerate":true}"#,
        r#"{"type":"dispatch.gate.observation","seq":6,"tail_ratio":0.9}"#,
        r#"{"type":"dispatch.gate.abort","seq":6}"#,
        r#"{"type":"dispatch.gate.abort","seq":6}"#,
    ]);
    let judged: Vec<bool> = f.checkpoints.iter().map(|c| c.judged_degenerate).collect();
    assert_eq!(judged, vec![true, true], "a pre-`would_conclude` conclusion still counts as a finding");
    assert_eq!(f.checkpoint_policy.as_deref(), Some("warn"));
    assert_eq!(f.checkpoints_concluded(), 1);
    assert_eq!(f.checkpoint_tail_ratios(), (Some(0.2), Some(0.2)), "a checkpoint with no ratio is no evidence, not a 1.0");
    assert_eq!((f.gate.observations, f.gate.abort_events, f.gate.aborted_turns.len()), (2, 2, 1));
    assert_eq!(f.gate.min_tail_ratio, Some(0.4));
}

#[test]
fn detector_firings_are_counted_and_promotions_summed() {
    let f = fold(&[
        r#"{"type":"dispatch.cycle.suspected","seq":1}"#,
        r#"{"type":"dispatch.reasoning_loop.suspected","seq":1}"#,
        r#"{"type":"dispatch.intra_turn_stall.recovered","seq":1}"#,
        r#"{"type":"dispatch.empty_tool_calls.recovered","seq":1}"#,
        r#"{"type":"dispatch.feedback.injected","seq":1}"#,
        r#"{"type":"tool_call.promoted","seq":2,"promoted_call_count":2}"#,
        r#"{"type":"tool_call.promoted","seq":3}"#,
        r#"{"type":"compaction","generation":1}"#,
    ]);
    let d = f.detectors;
    assert_eq!((d.cycle, d.reasoning_loop, d.intra_turn_stall, d.empty_tool_calls, d.feedback_injected), (1, 1, 1, 1, 1));
    assert_eq!(d.promoted_calls, 3, "an event older than the count promoted one call");
    assert_eq!(f.compactions(), 1);
}

/// An openclaw-era run (retired in #1405): its turns are `prompt.submitted`
/// events, its compactions are distinct summaries inside the thread, and its
/// own `model.completed` lines (ISO-string clocks) are not this format's.
#[test]
fn an_openclaw_run_counts_its_prompts_and_distinct_summaries() {
    let summary = |s: &str, before: u64| {
        format!(r#"{{"type":"prompt.submitted","ts":"2026-05-18T13:43:11.589Z","data":{{"messages":[{{"role":"user","summary":null}},{{"role":"compactionSummary","summary":"{s}","tokensBefore":{before}}}]}}}}"#)
    };
    let raw = [
        summary("first summary", 900),
        r#"{"traceSchema":"openclaw-trajectory","type":"model.completed","ts":"2026-05-18T13:43:11.589Z","seq":5}"#.to_string(),
        summary("first summary", 900),
        summary("second summary", 1200),
    ]
    .join("\n");
    let f = TrajectoryFold::from_lines(&raw);
    assert_eq!(f.turns(), 3);
    assert_eq!(f.compactions(), 2);
    assert_eq!(f.legacy.compactions.iter().map(|c| c.tokens_before).collect::<Vec<_>>(), vec![900, 1200]);
    assert_eq!(f.legacy.compactions.iter().map(|c| c.turn).collect::<Vec<_>>(), vec![1, 3], "the turn that first carried each");
    assert_eq!(f.legacy.compactions[0].summary, "first summary");
    assert_eq!(f.model_calls, 0, "openclaw's own model.completed is not a call of this format");
}

/// (#1959) A run that decayed and recovered must not read healthier than a
/// clean one: the minimum is the running minimum, never the last value.
#[test]
fn checkpoint_ratios_report_the_worst_and_the_mean() {
    let f = fold(&[
        r#"{"type":"dispatch.checkpoint","seq":1,"tail_ratio":0.9}"#,
        r#"{"type":"dispatch.checkpoint","seq":2,"tail_ratio":0.2}"#,
        r#"{"type":"dispatch.checkpoint","seq":3,"tail_ratio":0.99}"#,
    ]);
    let (min, mean) = f.checkpoint_tail_ratios();
    assert_eq!(min, Some(0.2));
    assert!((mean.unwrap() - (0.9 + 0.2 + 0.99) / 3.0).abs() < 1e-12);
    assert_eq!(fold(&[]).checkpoint_tail_ratios(), (None, None));
}

/// (#2263) A resumed run's whole-task turn count. The runtime numbers each
/// call's `seq` with the task's own turn counter, seeded from the
/// checkpoint, so the count is the later of the checkpoint's and the last
/// seq this run recorded. A hand-back resume CONTINUES turn N (its first
/// call is `seq: N`), a clean one starts turn N+1: adding the run's own
/// turn count to the seed over-counts the first by one.
#[test]
fn a_resumed_runs_whole_task_turns_come_from_its_seqs() {
    let seed = CheckpointCounts { turns: 3, compactions: 1 };
    let hand_back = fold(&[r#"{"type":"model.completed","seq":3,"finish_reason":"stop"}"#]);
    assert_eq!(seed.cumulative_turns(&hand_back), 3, "turn 3 continued, not a fourth turn");
    let clean = fold(&[
        r#"{"type":"model.completed","seq":4,"finish_reason":"tool_calls"}"#,
        r#"{"type":"compaction","generation":2}"#,
        r#"{"type":"model.completed","seq":5,"finish_reason":"stop"}"#,
    ]);
    assert_eq!(seed.cumulative_turns(&clean), 5);
    assert_eq!(seed.cumulative_compactions(&clean), 2);
    assert_eq!(seed.cumulative_turns(&fold(&[])), 3, "a run that made no call ends where it started");
    let fresh = CheckpointCounts::default();
    assert_eq!(fresh.cumulative_turns(&clean), 5);
}
