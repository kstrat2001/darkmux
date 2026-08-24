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
