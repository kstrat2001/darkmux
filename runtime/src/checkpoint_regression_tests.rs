//! (#1221) Checkpoint regression tests — the defects a live dispatch and two
//! adversarial review passes found, each pinned so it cannot come back.
//!
//! Provenance matters here. Every test below started as a PROVEN defect against
//! commit 53b4deaa, on a branch whose own 442-test suite was fully green — so
//! each one marks a place the unit suite could not see. Three came from review
//! probes briefed to FALSIFY the feature's central claims rather than to walk a
//! checklist; one came from watching a real 66-call dispatch on qwen3.6-35b.
//!
//! They drive the whole `run` loop against a mock server, which is the level
//! the defects live at: every one of them is an interaction between the region
//! machine, the message vector, and a finish_reason arm.
//!
//! Note for anyone adding to this file: `MockServer::mock` takes an `FnOnce`
//! that runs ONCE at registration, so a call counter inside it never advances
//! and the mock answers identically forever. Use mutually exclusive mocks keyed
//! on the request body via `.matches(...)`, as every test here does.
#![allow(clippy::too_many_arguments)]

use super::tests::chat_response_json;
use super::*;
use crate::lmstudio::LmStudioClient;
use crate::trajectory::Trajectory;
use httpmock::prelude::*;

fn body_of(r: &HttpMockRequest) -> String {
    String::from_utf8_lossy(r.body.as_deref().unwrap_or(&[])).to_string()
}

/// A response with NO `usage` object — some hosted endpoints and proxies omit
/// it. darkmux's own local path sets `stream_options.include_usage`, so
/// LMStudio always reports it; this is the shape everything else can send.
fn response_without_usage(content: &str, finish_reason: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "ignored-by-test",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish_reason,
        }],
    })
}

fn deliverable(o: &LoopOutcome) -> String {
    o.final_answer
        .clone()
        .filter(|a| !a.trim().is_empty())
        .or_else(|| {
            o.messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .and_then(|m| m.content.clone())
        })
        .unwrap_or_else(|| "<empty>".into())
}

fn go(server: &MockServer, prefix: &str, max_turns: Option<u32>) -> Result<LoopOutcome> {
    let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
    let tmp = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    let initial = vec![Message::system("test"), Message::user("go")];
    let tools: [Tool; 0] = [];
    let cfg = compaction::CompactionConfig::never_compact();
    run(
        &client,
        &client,
        "test-model",
        initial,
        &tools,
        &mut traj,
        false,
        &cfg,
        max_turns,
        None,
        Some(200),
        Some(200),
        std::collections::BTreeMap::new(),
        None,
    )
}

/// WORK LOST. A legitimate long answer whose last 8000 words are
/// structurally repetitive is ruled degenerate on a turn that never opened a
/// thought. `fall_through_to_recovery` runs `turn.abandon()` and
/// `recover_intra_turn_stall` pops the message — the text is gone from BOTH
/// the accumulation and the transcript, and the model is NOT resumed.
#[test]
#[serial_test::serial]
fn a_repetitive_but_legitimate_answer_is_never_deleted() {
    let server = MockServer::start();
    let part_one = format!(
        "SECTION-ONE the audit of module alpha found the following items.\n{}",
        "- [ ] verify the handler returns the right status code\n".repeat(200)
    );
    let _m1 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| {
                !body_of(r).contains("repeated itself without converging")
            });
        then.status(200)
            .json_body(chat_response_json(Some(&part_one), None, "length", 100, 200));
    });
    let _m2 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| {
                body_of(r).contains("repeated itself without converging")
            });
        then.status(200)
            .json_body(chat_response_json(Some("Done."), None, "stop", 100, 3));
    });

    let o = go(&server, "degen2", Some(6)).expect("Ok");
    let d = deliverable(&o);
    eprintln!("terminal={:?} deliverable={:?}", o.terminal_reason, d);
    assert!(
        d.contains("SECTION-ONE"),
        "regression: the whole first answer chunk was deleted by the \
         degeneracy verdict; the operator receives {d:?}"
    );
}

/// WORK LOST. `finish_reason=tool_calls` with an EMPTY tool_calls
/// array arriving mid-accumulation: `fold` writes the whole accumulation into
/// the assistant message, the message is pushed, and the #1123 recovery pops
/// it. The accumulation is cleared, so `pending_answer()` cannot recover it.
#[test]
#[serial_test::serial]
fn an_empty_tool_calls_turn_does_not_delete_the_accumulation() {
    let server = MockServer::start();
    let part_one = format!(
        "PART-ONE here are the first results.\n{}",
        (0..200).map(|j| format!("item{j} ")).collect::<String>()
    );
    let _m1 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| {
                let b = body_of(r);
                !b.contains("PART-ONE") && !b.contains("emitted reasoning tokens")
            });
        then.status(200)
            .json_body(chat_response_json(Some(&part_one), None, "length", 100, 200));
    });
    let _m2 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| body_of(r).contains("PART-ONE"));
        then.status(200).json_body(chat_response_json(
            None,
            Some(serde_json::json!([])),
            "tool_calls",
            100,
            5,
        ));
    });
    let _m3 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| {
                let b = body_of(r);
                !b.contains("PART-ONE") && b.contains("emitted reasoning tokens")
            });
        then.status(200)
            .json_body(chat_response_json(Some("Done."), None, "stop", 100, 3));
    });

    let o = go(&server, "emptytc2", Some(6)).expect("Ok");
    let d = deliverable(&o);
    eprintln!("terminal={:?} deliverable={:?}", o.terminal_reason, d);
    assert!(
        d.contains("PART-ONE"),
        "regression: the folded accumulation was pushed and then popped \
         by the empty-tool_calls recovery; the operator receives {d:?}"
    );
}

/// UNBOUNDED. Once the degeneracy verdict CLOSES the thought,
/// `degenerate` is hard-gated on `!turn.think_closed`, so nothing ever judges
/// the ANSWER region again. A model that keeps repeating after the forced
/// conclude checkpoints forever. A HANG here IS the finding.
#[test]
#[serial_test::serial]
fn a_repeating_answer_region_terminates_instead_of_running_forever() {
    let server = MockServer::start();
    // While the thought is OPEN (no "</think>" in the thread yet): a degenerate
    // INLINE think block, so the gate closes the thought.
    let open_reasoning = format!(
        "<think>\n{}",
        "I should re-read the file again. ".repeat(200)
    );
    let _m_open = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| !body_of(r).contains("</think>"));
        then.status(200)
            .json_body(chat_response_json(Some(&open_reasoning), None, "length", 100, 200));
    });
    // Once darkmux has closed the thought, the model repeats in the ANSWER
    // region. Nothing judges it.
    let repeating_answer = "The conclusion is that the handler is wrong. ".repeat(60);
    let _m_closed = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| body_of(r).contains("</think>"));
        then.status(200)
            .json_body(chat_response_json(Some(&repeating_answer), None, "length", 100, 200));
    });

    let o = go(&server, "unbounded", None);
    eprintln!("returned: {:?}", o.map(|x| x.terminal_reason));
}

/// SCRATCH LEAKED, modal path for the inline-think family.
///
/// An inline-think model (the qwen 3.x line this feature targets) is cut
/// mid-`<think>`; darkmux hands the block back OPEN. The model resumes, closes
/// the block ITSELF with `</think>`, and writes its answer. `absorb` is never
/// called for a terminal turn and `think_closed` is only ever set by the
/// degeneracy verdict, so `fold` takes the WHOLE terminal slice — trailing
/// reasoning, the stray `</think>`, and the answer — as the deliverable.
#[test]
#[serial_test::serial]
fn the_models_own_think_close_on_a_terminal_turn_is_not_the_answer() {
    let server = MockServer::start();
    let opening = format!("<think>\nSCRATCH-A {}", (0..300).map(|j| format!("distinct reasoning step {j} about module alpha. ")).collect::<String>());
    let _m1 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| !body_of(r).contains("SCRATCH-A"));
        then.status(200)
            .json_body(chat_response_json(Some(&opening), None, "length", 100, 200));
    });
    let concluding = "SCRATCH-B one last check before I answer.\n</think>\n\nThe handler is missing a status check.";
    let _m2 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| body_of(r).contains("SCRATCH-A"));
        then.status(200)
            .json_body(chat_response_json(Some(concluding), None, "stop", 100, 30));
    });

    let o = go(&server, "ownclose", Some(6)).expect("Ok");
    let d = deliverable(&o);
    eprintln!("terminal={:?}\nDELIVERABLE={d:?}", o.terminal_reason);
    assert!(
        !d.contains("SCRATCH-B") && !d.contains("</think>"),
        "regression: the model's own trailing reasoning and its \
         `</think>` delimiter are handed over as the answer"
    );
}

/// WORK LOST + SCRATCH LEAKED. The same self-close arriving at a
/// CHECKPOINT (not a terminal turn) is absorbed into the THOUGHT region,
/// because `think_closed` is never set from the delimiter. Every subsequent
/// slice — real committed ANSWER text — keeps landing in the thought, so the
/// answer region stays empty and `fold` delivers only the final slice.
#[test]
#[serial_test::serial]
fn an_answer_after_the_models_own_think_close_is_delivered() {
    let server = MockServer::start();
    let opening = format!("<think>\nSCRATCH-A {}", (0..300).map(|j| format!("distinct reasoning step {j} about module alpha. ")).collect::<String>());
    let _m1 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| !body_of(r).contains("SCRATCH-A"));
        then.status(200)
            .json_body(chat_response_json(Some(&opening), None, "length", 100, 200));
    });
    // The model closes its own block and starts the answer, but is cut again.
    let closing_then_answer = format!(
        "done deliberating.\n</think>\n\nANSWER-PART-ONE the handler is missing a status check. {}",
        "Detail sentence. ".repeat(40)
    );
    let _m2 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| {
                let b = body_of(r);
                b.contains("SCRATCH-A") && !b.contains("ANSWER-PART-ONE")
            });
        then.status(200)
            .json_body(chat_response_json(Some(&closing_then_answer), None, "length", 100, 200));
    });
    let _m3 = server.mock(move |when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(|r: &HttpMockRequest| body_of(r).contains("ANSWER-PART-ONE"));
        then.status(200)
            .json_body(chat_response_json(Some(" ANSWER-PART-TWO and that is all."), None, "stop", 100, 10));
    });

    let o = go(&server, "strand", Some(6)).expect("Ok");
    let d = deliverable(&o);
    eprintln!("terminal={:?}\nDELIVERABLE={d:?}", o.terminal_reason);
    assert!(
        d.contains("ANSWER-PART-ONE"),
        "regression: committed answer text emitted after the model's own \
         `</think>` was filed as THOUGHT and never delivered — operator gets {d:?}"
    );
}

/// WORK LOST. `cap_cliff` (and `at_cap`) are computed with
/// `this_turn_completion_tokens.is_some_and(...)`, so an ABSENT usage object
/// makes both false. A `length` finish with real content then takes the
/// context-overflow hard-error branch and kills the whole dispatch — the
/// checkpoint machinery is never reached at all.
#[test]
#[serial_test::serial]
fn an_absent_usage_object_does_not_make_every_boundary_fatal() {
    let server = MockServer::start();
    let answer: String = (0..200).map(|j| format!("word{j} ")).collect();
    let _m = server.mock(move |when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .json_body(response_without_usage(&answer, "length"));
    });

    let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
    let tmp = tempfile::Builder::new().prefix("nousage").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    let initial = vec![Message::system("test"), Message::user("go")];
    let tools: [Tool; 0] = [];
    let cfg = compaction::CompactionConfig::never_compact();

    let r = run(
        &client,
        &client,
        "test-model",
        initial,
        &tools,
        &mut traj,
        false,
        &cfg,
        Some(5),
        None,
        Some(200),
        Some(200),
        std::collections::BTreeMap::new(),
        None,
    );
    match r {
        Ok(o) => eprintln!("Ok: {:?} final={:?}", o.terminal_reason, o.final_answer),
        Err(e) => panic!("PROVEN: usage-absent boundary is a hard Err — {e}"),
    }
}

/// (#1959) A per-turn-cap SALVAGE is not a terminal finish.
///
/// Found by the first tool-using checkpointed dispatch — the shape #1221 was
/// never tested against, because every dispatch verified during that work was
/// a single-turn analyst answering a question with no tools.
///
/// Salvage rewrites `finish_reason` to `tool_calls` so the recovered calls get
/// dispatched, and deliberately sets `content = None` so truncated output does
/// not anchor the model or inflate every later prompt. The terminal fold was
/// gated on `finish_reason != "length"`, so it ran anyway and wrote the whole
/// accumulation straight back into that field.
///
/// The provider then received an assistant message with a large content blob
/// AND seven tool calls, followed by seven tool results, and returned HTTP 500.
#[test]
#[serial_test::serial]
fn a_salvaged_mid_turn_does_not_fold_and_does_not_restore_cleared_content() {
    const MARKER: &str = "ACCUMULATED-THOUGHT-THAT-MUST-NOT-RETURN";
    let server = MockServer::start();

    // Call 1: reasoning at the checkpoint interval — banks an accumulation.
    // Genuinely novel text, NOT a repeated phrase. A first draft repeated one
    // clause, which tripped the degeneracy gate on checkpoint 1 — the thought
    // closed, `deliverable()` correctly returned the answer region only, and
    // the test passed against the very bug it exists to catch. The fixture has
    // to keep the thought OPEN for the fold to have an accumulation to restore.
    let opening = format!(
        "<think>\n{MARKER} {}",
        (0..60)
            .map(|i| format!("examining module {i} for discarded results in path {i}. "))
            .collect::<String>()
    );
    let _m1 = server.mock(move |when, then| {
        when.method(POST).path("/v1/chat/completions").matches(|r: &HttpMockRequest| {
            !body_of(r).contains(MARKER)
        });
        then.status(200)
            .json_body(chat_response_json(Some(&opening), None, "length", 100, 200));
    });

    // Call 2: hits the interval again but carries a well-formed tool call —
    // the salvage shape. `length` + at-cap + tool calls.
    let _m2 = server.mock(move |when, then| {
        when.method(POST).path("/v1/chat/completions").matches(|r: &HttpMockRequest| {
            body_of(r).contains(MARKER)
        });
        then.status(200).json_body(chat_response_json(
            Some("truncated reasoning that ran past the cap"),
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": { "name": "echo", "arguments": "{\"text\":\"hi\"}" }
            }])),
            "length",
            100,
            200,
        ));
    });

    let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
    let tmp = tempfile::Builder::new().prefix("salvagefold").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    let initial = vec![Message::system("t"), Message::user("crawl")];
    let tools = [Tool::Echo];
    let cfg = compaction::CompactionConfig::never_compact();

    // max_turns bounds the run; the assertion is about message SHAPE.
    let o = run(
        &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
        Some(4), None, Some(200), Some(200), std::collections::BTreeMap::new(), None,
    )
    .expect("a salvaged mid-turn must not kill the dispatch");

    // Any assistant message carrying tool calls must NOT also carry the
    // accumulation. Salvage cleared that field on purpose.
    for m in o.messages.iter().filter(|m| m.role == "assistant") {
        let has_tools = m.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
        if has_tools {
            let content = m.content.as_deref().unwrap_or("");
            assert!(
                !content.contains(MARKER),
                "the fold restored content that salvage deliberately cleared — \
                 a tool-call message must not also carry the accumulated body: {content:?}"
            );
        }
    }
}

/// (#1959) A routine reasoning check-in must not tell the model to think less.
///
/// Salvage SHOULD still fire here — the model produced well-formed tool calls
/// and we cut it, so the calls get dispatched. What must not happen is the
/// accompanying nudge to "reduce per-call reasoning."
///
/// Once #1221 made `per_call_cap` region-dependent, that nudge began firing on
/// every 1000-token reasoning check-in. It is the one instruction #1221
/// measured as actively harmful: a model invited to wrap up wraps up, and
/// produced a tidy summary with ZERO findings where the same model
/// uninterrupted found real ones. On a loop, every thousand tokens.
#[test]
#[serial_test::serial]
fn a_reasoning_checkpoint_dispatches_tools_without_nudging_the_model_to_think_less() {
    let server = MockServer::start();
    // Cut at the 200-token reasoning interval, mid-thought, WITH a tool call.
    let _m = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(chat_response_json(
            Some("<think>\nstill working through the call graph"),
            Some(serde_json::json!([{
                "id": "c1",
                "type": "function",
                "function": { "name": "echo", "arguments": "{\"text\":\"x\"}" }
            }])),
            "length",
            100,
            199,
        ));
    });

    let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
    let tmp = tempfile::Builder::new().prefix("nonudge").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    let initial = vec![Message::system("t"), Message::user("crawl")];
    let tools = [Tool::Echo];
    let cfg = compaction::CompactionConfig::never_compact();

    let o = run(
        &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
        Some(3), None, Some(5000), Some(200), std::collections::BTreeMap::new(), None,
    )
    .expect("a reasoning checkpoint carrying tool calls returns Ok");

    // The tool calls DID get dispatched — salvage is doing its job.
    let tool_msgs = o.messages.iter().filter(|m| m.role == "tool").count();
    assert!(
        tool_msgs > 0,
        "well-formed tool calls cut at the check-in interval must still be dispatched"
    );

    // But nothing told the model to think less.
    // Matched on a verbatim phrase from the template, not on keywords. A
    // case-sensitive keyword filter (`"reduce"` against a template that says
    // `"Reduce"`) made a first draft of this assertion unable to fail.
    let nudged = o.messages.iter().any(|m| {
        m.role == "system"
            && m.content
                .as_deref()
                .map(|c| c.contains("Reduce reasoning length"))
                .unwrap_or(false)
    });
    assert!(
        !nudged,
        "a routine reasoning check-in must not inject a 'reduce your reasoning' \
         nudge — that is the one instruction measured to cost real findings"
    );
}

/// Counts outgoing requests that carried an invalid conversation.
///
/// A module static rather than a captured Arc because `httpmock`'s `matches`
/// takes a fn POINTER, which cannot close over anything. Safe: the only test
/// that touches it is `#[serial_test::serial]`.
static ADJACENT_ASSISTANT_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn body_has_adjacent_assistants(r: &HttpMockRequest) -> bool {
    let b = body_of(r);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&b) else { return false };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    let roles: Vec<&str> = msgs
        .iter()
        .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
        .collect();
    if roles.windows(2).any(|w| w[0] == "assistant" && w[1] == "assistant") {
        ADJACENT_ASSISTANT_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    // Never actually serve — this mock exists only to observe.
    false
}

/// (#1959) Never two consecutive assistant messages.
///
/// A checkpoint prefill followed by a salvaged tool-call message produced
/// exactly that, and the provider returned HTTP 500 on the next request. The
/// prefill has to be removed WITHOUT folding — folding ends the turn early and
/// restores content salvage deliberately cleared.
///
/// Pinned as a shape invariant rather than a behaviour, because the whole class
/// is "the conversation we send must be well-formed" and any future path that
/// pushes an assistant message beside a live prefill fails here too.
#[test]
#[serial_test::serial]
fn a_salvage_after_a_checkpoint_never_leaves_two_assistant_messages_adjacent() {
    const MARK: &str = "BANKED-REASONING";
    let server = MockServer::start();

    let opening = format!(
        "<think>\n{MARK} {}",
        (0..60).map(|i| format!("tracing call site {i} through module {i}. ")).collect::<String>()
    );
    let _m1 = server.mock(move |when, then| {
        when.method(POST).path("/v1/chat/completions").matches(|r: &HttpMockRequest| {
            !body_of(r).contains(MARK)
        });
        then.status(200)
            .json_body(chat_response_json(Some(&opening), None, "length", 100, 200));
    });
    let _m2 = server.mock(move |when, then| {
        when.method(POST).path("/v1/chat/completions").matches(|r: &HttpMockRequest| {
            body_of(r).contains(MARK)
        });
        then.status(200).json_body(chat_response_json(
            Some("cut mid-sentence but the call is well formed"),
            Some(serde_json::json!([{
                "id": "c1",
                "type": "function",
                "function": { "name": "echo", "arguments": "{\"text\":\"x\"}" }
            }])),
            "length",
            100,
            200,
        ));
    });

    ADJACENT_ASSISTANT_HITS.store(0, std::sync::atomic::Ordering::SeqCst);
    let _detector = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .matches(body_has_adjacent_assistants);
        then.status(500).body("never served — this mock only observes");
    });

    let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
    let tmp = tempfile::Builder::new().prefix("adjacent").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    let initial = vec![Message::system("t"), Message::user("crawl")];
    let tools = [Tool::Echo];
    let cfg = compaction::CompactionConfig::never_compact();

    let o = run(
        &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
        Some(4), None, Some(200), Some(200), std::collections::BTreeMap::new(), None,
    )
    .expect("Ok");

    // Asserted on what went OVER THE WIRE, not on the final message vector.
    //
    // A first draft checked `o.messages` at the end and could not fail: later
    // checkpoints clean the vector up, so the invalid state is INTERMEDIATE and
    // invisible by the time the run returns. The provider, however, sees every
    // intermediate request — which is why it 500s and the final-state check
    // does not.
    let malformed = ADJACENT_ASSISTANT_HITS.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        malformed, 0,
        "{malformed} outgoing request(s) carried two consecutive assistant \
         messages — an invalid conversation the provider answers with HTTP 500"
    );
    drop(o);
}
