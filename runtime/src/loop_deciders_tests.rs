//! (#3136) Table tests for the loop's deciders, one table per rule.
//!
//! Each table is the whole specification of its rule: a change to the rule
//! should change its rows here and nothing else, while the characterization
//! traces (`loop_characterization_tests.rs`) confirm the loop still wires the
//! rule's effect in. Rows name the case they pin, so a failure says which one.

use super::*;

/// A state with only the fields a table sets; everything else zero.
fn state() -> LoopState {
    LoopState::default()
}

fn limits() -> Limits {
    Limits::resolve(Knobs::default(), 600, 0, false)
}

fn bound(per_call_cap: u32, wire_max_tokens: u32, reasoning: bool, generation: bool) -> CallBound {
    CallBound { per_call_cap, wire_max_tokens, reasoning, generation }
}

fn assistant(content: Option<&str>, calls: usize) -> Message {
    let mut m = Message::assistant("");
    m.content = content.map(str::to_string);
    if calls > 0 {
        let call: ToolCall = serde_json::from_value(serde_json::json!({
            "id": "c", "type": "function",
            "function": { "name": "read", "arguments": "{}" },
        }))
        .unwrap();
        m.tool_calls = Some(vec![call; calls]);
    }
    m
}

// ─── state folds ─────────────────────────────────────────────────────────

#[test]
fn seeded_restores_the_counters_and_the_hand_back_latch_only() {
    assert_eq!(LoopState::seeded(None), state(), "a fresh dispatch starts at zero");

    let seed = |hand_back: Option<bool>| checkpoint::RunCheckpoint {
        schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
        role_id: "r".into(),
        messages: vec![],
        turns: 4,
        total_completion_tokens: 900,
        compactions: 2,
        pending_hand_back: hand_back.map(|is_reasoning| checkpoint::PendingHandBack {
            thought: String::new(),
            answer: String::new(),
            think_closed: false,
            is_reasoning,
            carries_own_opener: false,
        }),
        pending_tool_calls: None,
        pending_tool_calls_seq_base: 0,
        pending_head_started: false,
        written_at_unix_ms: 0,
    };
    // (hand_back, resuming, reasoned)
    for (hand_back, resuming, reasoned) in [(None, false, false), (Some(false), true, false), (Some(true), true, true)] {
        let s = LoopState::seeded(Some(&seed(hand_back)));
        assert_eq!((s.turns, s.total_completion_tokens, s.compactions), (4, 900, 2));
        assert_eq!(s.resuming_after_checkpoint, resuming, "hand_back {hand_back:?}");
        assert_eq!(s.dispatch_has_reasoned, reasoned, "hand_back {hand_back:?}");
        assert_eq!(s.stall_recoveries_used + s.consecutive_malformed_turns, 0, "detectors never restored");
    }
}

#[test]
fn begin_call_opens_a_turn_unless_it_continues_one() {
    let mut fresh = LoopState { turns: 3, generation_continuations_this_turn: 2, turn_completion_tokens: 70, ..state() };
    assert!(fresh.begin_call(), "a fresh call opens a turn");
    assert_eq!((fresh.turns, fresh.generation_continuations_this_turn, fresh.turn_completion_tokens), (4, 0, 0));

    let mut cont = LoopState {
        turns: 3,
        generation_continuations_this_turn: 2,
        turn_completion_tokens: 70,
        resuming_after_checkpoint: true,
        ..state()
    };
    assert!(!cont.begin_call(), "a continuation is the same turn");
    assert_eq!(
        (cont.turns, cont.generation_continuations_this_turn, cont.turn_completion_tokens, cont.resuming_after_checkpoint),
        (3, 2, 70, false),
        "the latch is consumed; the turn's budgets carry on"
    );
}

#[test]
fn fold_completion_takes_the_reported_count_then_the_estimate() {
    // (reported, estimate, spent)
    for (reported, estimate, spent) in [(Some(10), Some(99), 10), (None, Some(7), 7), (None, None, 0), (Some(0), Some(5), 0)] {
        let mut s = LoopState { total_completion_tokens: 100, turn_completion_tokens: 1, ..state() };
        s.fold_completion(reported, estimate);
        assert_eq!((s.total_completion_tokens, s.turn_completion_tokens), (100 + spent, 1 + spent), "{reported:?}/{estimate:?}");
    }
    let mut s = LoopState { total_completion_tokens: u32::MAX, ..state() };
    s.fold_completion(Some(5), None);
    assert_eq!(s.total_completion_tokens, u32::MAX, "saturates");
}

#[test]
fn fold_prompt_count_tracks_staleness_and_anchors_the_estimate() {
    let mut s = state();
    // (reported, frozen after)
    for (reported, frozen) in [(500, 0), (500, 1), (500, 2), (650, 0)] {
        s.fold_prompt_count(reported, 2_000);
        assert_eq!(s.frozen_prompt_turns, frozen, "after reporting {reported}");
        assert_eq!((s.latest_prompt_tokens, s.prev_prompt_tokens), (reported, Some(reported)));
        assert_eq!(s.prompt_anchor, Some(PromptAnchor { chars: 2_000, tokens: reported }));
    }
}

#[test]
fn limits_floor_the_generation_continuations_at_four() {
    // (answer, generation interval, continuations)
    for (answer, generation, expected) in [(10_000, 4_000, 4), (40_000, 4_000, 10), (10_000, 0, 10_000), (100, 4_000, 4)] {
        let k = Knobs {
            max_tokens_per_call: Some(answer),
            generation_checkpoint_interval: Some(generation),
            ..Knobs::default()
        };
        assert_eq!(Limits::resolve(k, 600, 0, false).max_generation_continuations, expected, "{answer}/{generation}");
    }
    let defaults = limits();
    assert_eq!(
        (defaults.answer_max_tokens, defaults.reasoning_interval, defaults.generation_interval, defaults.stall_recovery_budget),
        (MAX_TOKENS_PER_CALL, REASONING_CHECKPOINT_INTERVAL, GENERATION_CHECKPOINT_INTERVAL, MAX_STALL_RECOVERIES),
    );
}

// ─── the inactivity clock ────────────────────────────────────────────────

#[test]
fn reanchors_after_sleep_table() {
    // (turns, warning already fired, elapsed, budget, re-anchor)
    let rows = [
        (1, false, 1_201, 600, true),
        (1, false, 1_200, 600, false),
        (0, false, 9_999, 600, false),
        (1, true, 9_999, 600, false),
        (1, false, 9_999, 0, false),
    ];
    for (turns, fired, elapsed, budget, expected) in rows {
        let s = LoopState { turns, soft_warning_fired: fired, ..state() };
        assert_eq!(reanchors_after_sleep(&s, elapsed, budget), expected, "{turns}/{fired}/{elapsed}/{budget}");
    }
}

#[test]
fn warns_of_inactivity_table() {
    // (warning already fired, elapsed, budget, warn)
    let rows = [
        (false, 450, 600, true),
        (false, 449, 600, false),
        (true, 599, 600, false),
        (false, 1, 1, true),
        (false, 0, 1, false),
        (false, u64::MAX - 1, 0, false),
    ];
    for (fired, elapsed, budget, expected) in rows {
        let s = LoopState { soft_warning_fired: fired, ..state() };
        assert_eq!(warns_of_inactivity(&s, elapsed, budget), expected, "{fired}/{elapsed}/{budget}");
    }
}

// ─── the turn boundary ───────────────────────────────────────────────────

#[test]
fn budget_stop_table() {
    // (turns, tokens, max_turns, max_tokens, stop)
    let rows = [
        (2, 0, Some(2), None, Some(BudgetStop::MaxTurns(2))),
        (1, 0, Some(2), None, None),
        (0, 30, None, Some(30), Some(BudgetStop::CumulativeTokens(30))),
        (0, 29, None, Some(30), None),
        (5, 50, Some(5), Some(30), Some(BudgetStop::MaxTurns(5))),
        (9, 9_999, None, None, None),
    ];
    for (turns, tokens, max_turns, max_cumulative_tokens, expected) in rows {
        let s = LoopState { turns, total_completion_tokens: tokens, ..state() };
        let l = Limits { max_turns, max_cumulative_tokens, ..limits() };
        assert_eq!(budget_stop(&s, &l), expected, "{turns}/{tokens}/{max_turns:?}/{max_cumulative_tokens:?}");
    }
}

#[test]
fn rests_before_call_table() {
    // (turns, resuming, delay, rest)
    for (turns, resuming, delay, expected) in [(1, false, 500, true), (0, false, 500, false), (1, true, 500, false), (1, false, 0, false)] {
        let s = LoopState { turns, resuming_after_checkpoint: resuming, ..state() };
        let l = Limits { turn_delay_ms: delay, ..limits() };
        assert_eq!(rests_before_call(&s, &l), expected, "{turns}/{resuming}/{delay}");
    }
}

#[test]
fn boundary_checkpoint_and_pace_table() {
    // (turns, resuming, checkpoint, honors pace)
    let rows = [
        (0, false, BoundaryCheckpoint::Skip, true),
        (0, true, BoundaryCheckpoint::Skip, false),
        (3, false, BoundaryCheckpoint::Write, true),
        (3, true, BoundaryCheckpoint::WriteWithHandBack, false),
    ];
    for (turns, resuming, checkpoint, pace) in rows {
        let s = LoopState { turns, resuming_after_checkpoint: resuming, ..state() };
        assert_eq!(boundary_checkpoint(&s), checkpoint, "{turns}/{resuming}");
        assert_eq!(honors_pace(&s), pace, "{turns}/{resuming}");
    }
}

#[test]
fn call_bound_table() {
    // (in answer region, has reasoned, generation interval, streaming, bound)
    let rows = [
        (false, true, 4_000, false, bound(1_000, 1_000, true, false)),
        (false, false, 4_000, false, bound(4_000, 4_000, false, true)),
        (true, true, 4_000, false, bound(4_000, 4_000, false, true)),
        (false, false, 10_000, false, bound(10_000, 10_000, false, false)),
        (false, false, 20_000, false, bound(10_000, 10_000, false, false)),
        (false, true, 4_000, true, bound(1_000, 10_000, true, false)),
        (false, false, 4_000, true, bound(4_000, 10_000, false, true)),
    ];
    for (answer_region, reasoned, generation_interval, streaming, expected) in rows {
        let l = Limits {
            answer_max_tokens: 10_000,
            reasoning_interval: 1_000,
            generation_interval,
            streaming,
            ..limits()
        };
        assert_eq!(
            call_bound(answer_region, reasoned, &l),
            expected,
            "{answer_region}/{reasoned}/{generation_interval}/{streaming}"
        );
    }
}

#[test]
fn request_seq_names_the_turn_the_request_belongs_to() {
    assert_eq!(request_seq(&LoopState { turns: 4, ..state() }), 5, "a fresh turn");
    assert_eq!(request_seq(&LoopState { turns: 4, resuming_after_checkpoint: true, ..state() }), 4, "a continuation");
}

// ─── reading a response ──────────────────────────────────────────────────

#[test]
fn call_reasoned_table() {
    // (per-turn reasoning, separate field, content, reasoned)
    let rows: [(&str, Option<&str>, Option<&str>, bool); 9] = [
        ("thought", None, None, true),
        ("  \n", None, None, false),
        ("", Some("separate"), None, true),
        ("", Some("  "), None, false),
        ("", None, Some("<think>still going"), true),
        ("", None, Some("  <think>a</think>"), false),
        ("", None, Some("answer then <think>"), false),
        ("", None, Some("<think>a</think><think>b"), true),
        ("", None, None, false),
    ];
    for (reasoning, separate, content, expected) in rows {
        assert_eq!(call_reasoned(reasoning, separate, content), expected, "{reasoning:?}/{separate:?}/{content:?}");
    }
}

#[test]
fn message_shape_predicates_table() {
    // (content, calls, no text, blank assistant, dispatchable)
    let rows = [
        (None, 0, true, true, false),
        (Some("  "), 0, true, true, false),
        (Some("hi"), 0, false, false, true),
        (None, 1, true, false, true),
        (Some("hi"), 1, false, false, true),
    ];
    for (content, calls, no_text, blank, dispatchable) in rows {
        let m = assistant(content, calls);
        assert_eq!(has_no_text(&m), no_text, "{content:?}/{calls}");
        assert_eq!(is_blank_assistant(&m), blank, "{content:?}/{calls}");
        assert_eq!(has_dispatchable_output(&m), dispatchable, "{content:?}/{calls}");
        assert_eq!(has_tool_calls(&m), calls > 0);
    }
    let mut user = assistant(None, 0);
    user.role = "user".into();
    assert!(!is_blank_assistant(&user), "only an assistant message is a blank assistant");
}

#[test]
fn reports_no_reasoning_region_table() {
    // (already logged, has reasoned, this call's reasoning, had output, report)
    let rows = [
        (false, false, "", true, true),
        (true, false, "", true, false),
        (false, true, "", true, false),
        (false, false, "thought", true, false),
        (false, false, "", false, false),
    ];
    for (logged, reasoned, reasoning, output, expected) in rows {
        let s = LoopState { no_reasoning_region_logged: logged, dispatch_has_reasoned: reasoned, ..state() };
        assert_eq!(reports_no_reasoning_region(&s, reasoning, output), expected, "{logged}/{reasoned}/{reasoning:?}/{output}");
    }
}

#[test]
fn salvage_nudges_only_when_the_answer_budget_ran_out() {
    // (reasoning bound, generation bound, nudge)
    for (reasoning, generation, expected) in [(false, false, true), (true, false, false), (false, true, false)] {
        assert_eq!(salvage_nudges(&bound(1, 1, reasoning, generation)), expected, "{reasoning}/{generation}");
    }
}

#[test]
fn turn_end_table() {
    // (effective finish, salvaged, end)
    let rows = [
        ("stop", false, TurnEnd::Fold),
        ("tool_calls", false, TurnEnd::Fold),
        ("length", false, TurnEnd::Continue),
        ("tool_calls", true, TurnEnd::Supersede),
        ("content_filter", false, TurnEnd::Fold),
    ];
    for (finish, salvaged, expected) in rows {
        assert_eq!(turn_end(finish, salvaged), expected, "{finish}/{salvaged}");
    }
}

// ─── tool calls ──────────────────────────────────────────────────────────

#[test]
fn stall_budget_exhausted_table() {
    // (used, budget, exhausted)
    for (used, budget, expected) in [(2, 2, true), (1, 2, false), (0, 0, true), (3, 2, true)] {
        let s = LoopState { stall_recoveries_used: used, ..state() };
        let l = Limits { stall_recovery_budget: budget, ..limits() };
        assert_eq!(stall_budget_exhausted(&s, &l), expected, "{used}/{budget}");
    }
}

#[test]
fn tally_dispatch_table() {
    // (malformed before, stall before, dispatched any, opened a turn) -> (malformed, stall, escalate)
    let rows = [
        ((0, 1, false, true), (1, 1, false)),
        ((MAX_CONSECUTIVE_MALFORMED_TURNS - 1, 1, false, true), (MAX_CONSECUTIVE_MALFORMED_TURNS, 1, true)),
        ((2, 2, true, true), (0, 1, false)),
        ((2, 2, true, false), (0, 2, false)),
        ((0, 0, true, true), (0, 0, false)),
    ];
    for ((malformed, stall, dispatched, opened), (m, st, escalate)) in rows {
        let s = LoopState { consecutive_malformed_turns: malformed, stall_recoveries_used: stall, ..state() };
        assert_eq!(
            tally_dispatch(&s, dispatched, opened),
            DispatchTally { consecutive_malformed_turns: m, stall_recoveries_used: st, escalate },
            "{malformed}/{stall}/{dispatched}/{opened}"
        );
    }
}

#[test]
fn cadence_after_call_table() {
    let edit = |p: &str| format!(r#"{{"path":"{p}","old_string":"a","new_string":"b"}}"#);
    let at = |path: Option<&str>, n: u32| LoopState {
        last_edited_path: path.map(str::to_string),
        consecutive_same_file_edits: n,
        ..state()
    };
    let cadence = |path: Option<&str>, n: u32, fired: Option<&str>| Cadence {
        last_edited_path: path.map(str::to_string),
        consecutive_same_file_edits: n,
        fired: fired.map(str::to_string),
    };
    // (state, tool, args, next)
    let rows = [
        (at(None, 0), "edit", edit("a.rs"), cadence(Some("a.rs"), 1, None)),
        (at(Some("a.rs"), 1), "write", edit("./a.rs"), cadence(Some("a.rs"), 2, None)),
        (at(Some("a.rs"), 2), "edit", edit("a.rs"), cadence(None, 0, Some("a.rs"))),
        (at(Some("a.rs"), 2), "edit", edit("b.rs"), cadence(Some("b.rs"), 1, None)),
        (at(Some("a.rs"), 2), "edit", "not json".to_string(), cadence(Some("a.rs"), 2, None)),
        (at(Some("a.rs"), 2), "bash", r#"{"command":"cargo test"}"#.to_string(), cadence(None, 0, None)),
        (at(Some("a.rs"), 2), "read", edit("a.rs"), cadence(Some("a.rs"), 2, None)),
    ];
    for (s, tool, args, expected) in rows {
        assert_eq!(cadence_after_call(&s, tool, &args), expected, "{tool} {args} from {:?}", s.last_edited_path);
    }
}

// ─── compaction ──────────────────────────────────────────────────────────

#[test]
fn compaction_bounds_table() {
    for (frozen, crossed) in [(STALE_PROMPT_TOKENS_TURNS, true), (STALE_PROMPT_TOKENS_TURNS - 1, false), (STALE_PROMPT_TOKENS_TURNS + 1, false)] {
        assert_eq!(stale_count_crossed(&LoopState { frozen_prompt_turns: frozen, ..state() }), crossed, "frozen {frozen}");
    }
    for (n, escalate) in [(UNPRODUCTIVE_COMPACTION_TURNS, true), (UNPRODUCTIVE_COMPACTION_TURNS - 1, false)] {
        assert_eq!(unproductive_compactions_escalate(n), escalate, "unproductive {n}");
    }
    for (n, escalate) in [(COMPACTION_REREAD_TURNS, true), (COMPACTION_REREAD_TURNS - 1, false)] {
        assert_eq!(reread_loop_escalates(n), escalate, "reread {n}");
    }
    // (compactions, bail, reached)
    for (compactions, bail, reached) in [(1, Some(1), true), (0, Some(1), false), (5, None, false), (2, Some(1), true)] {
        assert_eq!(compaction_bound_reached(compactions, bail), reached, "{compactions}/{bail:?}");
    }
}

// ─── the length arm ──────────────────────────────────────────────────────

#[test]
fn length_effect_table() {
    // (stall used, useless stall, cap cliff, produced nothing, effect)
    let rows = [
        (0, false, false, false, LengthEffect::Overflow),
        (2, false, false, false, LengthEffect::Overflow),
        (2, true, false, true, LengthEffect::Escalate),
        (2, false, true, false, LengthEffect::Escalate),
        (0, true, true, true, LengthEffect::RecoverStall),
        (0, true, false, true, LengthEffect::RecoverStall),
        (0, false, true, false, LengthEffect::Checkpoint),
        (1, false, true, false, LengthEffect::Checkpoint),
    ];
    for (used, useless, cliff, nothing, expected) in rows {
        let s = LoopState { stall_recoveries_used: used, ..state() };
        let l = Limits { stall_recovery_budget: 2, ..limits() };
        assert_eq!(length_effect(&s, &l, useless, cliff, nothing), expected, "{used}/{useless}/{cliff}/{nothing}");
    }
}

#[test]
fn draw_generation_budget_table() {
    // (drawn so far, generation-bound call, max) -> (drawn, exhausted)
    let rows = [((0, true, 4), (1, false)), ((4, true, 4), (5, true)), ((3, true, 4), (4, false)), ((9, false, 4), (9, false))];
    for ((drawn, generation, max), expected) in rows {
        let s = LoopState { generation_continuations_this_turn: drawn, ..state() };
        let l = Limits { max_generation_continuations: max, ..limits() };
        assert_eq!(draw_generation_budget(&s, &bound(1, 1, false, generation), &l), expected, "{drawn}/{generation}/{max}");
    }
}

#[test]
fn governing_interval_table() {
    let abort = CutSource::RuntimeAbort(crate::stream_gate::AbortReason::Degenerate);
    let server = CutSource::ServerLength { measured_at_cap: Some(true) };
    // (cut, per-call cap, wire, interval)
    for (cut, cap, wire, expected) in [(abort, 1_000, 10_000, 1_000), (server, 1_000, 1_000, 1_000), (server, 1_000, 10_000, 10_000), (CutSource::None, 4_000, 4_000, 4_000)] {
        assert_eq!(governing_interval(cut, &bound(cap, wire, false, false)), expected, "{cut:?}/{cap}/{wire}");
    }
}

#[test]
fn checkpoint_remedy_table() {
    use ContinuationLimit::{ContextWindow, GenerationBudget};
    // (degenerate, writing thought, limit, thought closed, remedy)
    let rows = [
        (true, false, None, true, Remedy::HandOff),
        (true, false, Some(GenerationBudget), true, Remedy::HandOff),
        (true, true, Some(GenerationBudget), false, Remedy::StopAt(GenerationBudget)),
        (false, true, Some(ContextWindow), false, Remedy::StopAt(ContextWindow)),
        (true, true, None, false, Remedy::CloseThought),
        (false, false, None, true, Remedy::ContinueClosed),
        (false, true, None, false, Remedy::ContinueOpen),
    ];
    for (degenerate, writing, limit, closed, expected) in rows {
        assert_eq!(checkpoint_remedy(degenerate, writing, limit, closed), expected, "{degenerate}/{writing}/{limit:?}/{closed}");
    }
}
