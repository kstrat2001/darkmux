//! The tool-call loop.
//!
//! Sends the conversation to LMStudio. If the model returns a
//! `tool_calls` finish_reason, dispatches each tool, appends results,
//! checks whether the context budget needs compaction, and re-sends.
//! Loops until `stop`, or fails loudly on `length` / unexpected outcomes.
//!
//! Phase 6 added compaction via `crate::compaction`: token-count-aware
//! middle-replace strategy that summarizes via a companion model.
//!
//! Phase 7 (#205) added SSE streaming for the main turn chat() call:
//! delta chunks accumulate into the same `ChatResponse` shape the loop
//! used to receive from non-streaming, and per-chunk `model.partial`
//! events land in the trajectory so a second observer can `tail -F`
//! and see the dispatch making progress mid-turn. The companion
//! compactor model (`compaction::compact`) stays non-streaming — it's a
//! short fire-and-forget summarization call where mid-turn observability
//! doesn't matter.
//!
//! Still omitted (Phase 8+ if measurements show they're needed):
//!
//! - No retries on transient failures. A network blip aborts the loop.
//! - No per-profile threshold derivation (compaction threshold is env-
//!   tunable but global, not derived from active darkmux profile).

use std::collections::HashSet;

use anyhow::{anyhow, Result};

use crate::compaction;
use crate::cycle_detector::{CycleDetector, CycleSignal};
use crate::failure_rate::{FailureCascadeSignal, FailureRateDetector};
use crate::feedback::FeedbackInjector;
use crate::lmstudio::{ChatRequest, ChunkAccumulator, LmStudioClient, Message};
use crate::plain_text_tool_calls::promote_plain_text_tool_calls;
use crate::reasoning_loop::{ReasoningLoopDetector, ReasoningLoopSignal};
use crate::tools::{dispatch, Tool};
use crate::trajectory::Trajectory;

// (#457) Cap on tool-call turns inside a single dispatch — REMOVED
// as a hardcoded constant. Now passed as `Option<u32>` to `run()` via
// the `--max-turns` runtime CLI flag; host derives the value from the
// `DARKMUX_RUNTIME_MAX_TURNS` env var. Default `None` = unlimited.
//
// Pre-#457 this was a const `100`. Beat 47 run 5 hit it mid-coding
// with 100 turns and an active edit loop; #416 named the fix as
// "operator-tunable per profile, no default ceiling." The inactivity
// timeout (#458) now catches the genuine-stuck case; a productive
// dispatch making real progress turn-by-turn shouldn't be killed by
// an arbitrary turn count.

/// Per-call cap on completion tokens. LMStudio counts BOTH content
/// tokens AND reasoning_content tokens against this cap (verified
/// empirically — `usage.completion_tokens_details.reasoning_tokens`
/// is included in the total). So the cap bounds runaway-reasoning
/// emission too, not just runaway content.
///
/// **Why an absolute value, not a ratio of `n_ctx`** — this cap is a
/// **failure-boundary**, not a context-budget allocation. A 14-min
/// reasoning hang generates roughly the same token count regardless
/// of whether context is 32K or 1M. Ratio-of-context would give a
/// 1M-context operator 100K tokens per turn under a 10% ratio —
/// "more RAM = more rope = worse outcomes," an anti-incentive. The
/// cap should land below the unstuck-but-burning-tokens threshold
/// AND above the legitimate-useful-turn ceiling — both bounded by
/// the WORK shape, not the RAM tier.
///
/// **Why 10000** — 2× the observed max-useful-turn (5082 tokens
/// across 170 turns in 4 baseline runs, lab notebook Beat 47).
/// Comfortable ceiling for legitimately verbose turns; still well
/// below the runaway-emission territory (~50K tokens in a 14-min
/// reasoning hang per Beat 47 run 3). Roughly 22% above openclaw's
/// `SELF_HOSTED_DEFAULT_MAX_TOKENS = 8192` — same defensive shape,
/// slightly more headroom for thoughtful turns. (#415)
const MAX_TOKENS_PER_CALL: u32 = 10000;

// (#457) Per-dispatch cumulative-completion-tokens cap — REMOVED as
// a hardcoded constant. Now passed as `Option<u32>` to `run()` via
// the `--max-tokens` runtime CLI flag; host derives the value from
// the `DARKMUX_RUNTIME_MAX_TOKENS` env var. Default `None` =
// unlimited.
//
// Pre-#457 this was a const `250_000`. Same reframe as `MAX_TURNS`:
// absolute caps embed a guess about how long good work should take,
// which doesn't generalize across the workload distribution operators
// will encounter. The inactivity timeout (#458) catches the
// genuine-stuck case; the operator can layer their own ceiling here
// for cost-conscious cloud-billed or supervised-only dispatches.

/// (#414 PR A) Per-dispatch budget for intra-turn stall recoveries.
/// Each recovery costs one extra chat() call + a small nudge message;
/// the budget caps the cost while still tolerating a transient stall.
///
/// **Why 2** — Beat 47/48 showed runs that hit one runaway-reasoning
/// turn then recovered on the next normal call. A budget of 2 gives
/// the loop one "free" retry after the first stall, plus a second if
/// the next turn also stalls. Three consecutive stalls is the
/// pathology signal — escalate rather than burn more turns trying.
const MAX_STALL_RECOVERIES: u32 = 2;

/// (#414 PR A) Nudge text injected as a system message when the loop
/// recovers from a length-finish stall. Names BOTH valid next moves
/// (tool call OR final answer) so the model has explicit alternatives
/// to the reasoning-spiral pattern that triggered the stall. The
/// `[darkmux-runtime]` prefix flags the message as runtime-injected so
/// the operator + reviewer can recognize it in trajectory replay.
const STALL_NUDGE_MESSAGE: &str = "[darkmux-runtime] Your previous response \
emitted reasoning tokens up to the per-call cap without producing a tool \
call or a final answer. Please either invoke a tool to make progress, or \
provide a direct final answer.";

/// How the loop terminated. Distinguishes "model said stop" from
/// "loop hit the safety cap and gave up" — semantically different
/// outcomes for downstream consumers (a max_turns hit means the
/// reply is partial/wedged and a re-dispatch with a fresh session
/// might be the right move; a stop means use the reply).
///
/// Pre-fix the MAX_TURNS path was an `Err(...)` indistinguishable
/// from infrastructure failures (Docker died, LMStudio went away).
/// Operators reading the JSON envelope's `result` field saw `error`
/// for both cases; structured terminal reason lets the runtime emit
/// `result: "max_turns"` instead. (#325)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    /// Model returned finish_reason=stop.
    Stop,
    /// Loop hit MAX_TURNS without reaching a stop. Reply is whatever
    /// the last assistant message produced — likely partial.
    MaxTurns,
    /// (#377) Operator-set bound was hit and the dispatch escalated
    /// out of local-tier rather than continuing. The bound + the
    /// specific condition that fired live in [`EscalationReason`].
    /// Salvageable state (final messages, partial work, completed
    /// turns) is in the rest of [`LoopOutcome`] so the frontier-tier
    /// handoff skill can pick up where local-tier left off. KISS-
    /// doubled (Beat 44 closure): bound the cost, don't optimize it.
    EscalationTriggered(EscalationReason),
}

/// (#377) Which operator-set bound was crossed when an
/// [`TerminalReason::EscalationTriggered`] terminal fires. Designed
/// as an enum (not a single variant on TerminalReason) so future
/// escalation conditions — token-budget exhaustion, hang-timeout,
/// role-explicit bail — can join under the same terminal without
/// fragmenting the JSON envelope's `result` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// Compaction count reached the operator-configured
    /// `bail_after_compactions` threshold (typed field
    /// `profile.runtime.compaction.reserve.bail_after_compactions`,
    /// schema landed in #357, consumer in #377).
    CompactionLimitReached,
    /// (#423) Sum of `usage.completion_tokens` across all turns
    /// crossed [`MAX_CUMULATIVE_COMPLETION_TOKENS`]. Catches the
    /// "death by a thousand cuts" pattern that per-call max_tokens
    /// (#415) and MAX_TURNS individually don't: a dispatch can stay
    /// under both individual caps yet still burn through hundreds of
    /// thousands of cumulative tokens. Salvageable partial state
    /// flows through `LoopOutcome` as with the other escalation
    /// reasons.
    CumulativeTokensExceeded,
    /// (#414 PR A) Intra-turn stall recovery budget
    /// ([`MAX_STALL_RECOVERIES`]) exhausted. Fires when the model
    /// returned `finish_reason=length` with no content and no
    /// tool_calls more times than the budget allows — the recovery
    /// nudge isn't breaking the pattern, so the dispatch escalates
    /// rather than burn more turns on the same stall.
    IntraTurnStallExhausted,
}

/// Outcome of a completed loop run.
#[derive(Debug)]
pub struct LoopOutcome {
    /// Why the loop terminated. See [`TerminalReason`].
    pub terminal_reason: TerminalReason,
    /// Full conversation, in order, including system / user / assistant
    /// / tool messages. The final assistant message has the model's
    /// terminal response.
    pub messages: Vec<Message>,

    /// Number of model turns the loop took (each chat-completion call).
    pub turns: u32,

    /// Total prompt tokens summed across all calls. Used for cumulative
    /// cost reporting; per-call usage drives compaction triggering.
    pub total_prompt_tokens: u32,

    /// Total completion tokens summed across all calls.
    pub total_completion_tokens: u32,

    /// Number of compaction events that fired during the loop.
    /// Phase 6: middle-replace via the companion compactor model.
    pub compactions: u32,
}

/// Run the tool-call loop to completion.
///
/// `trajectory` records each significant event (model.completed,
/// tool.completed, compaction). When the recorder was opened against
/// an unwritable path, its methods are no-ops — the loop runs the same
/// either way.
///
/// `streaming` switches the per-turn chat call between SSE-streamed
/// (default; emits model.partial trajectory events as chunks arrive)
/// and single-shot non-streaming (opt-out for tests/benchmarks where
/// determinism or simpler trajectory size matters). The accumulated
/// final response is identical either way; the rest of the loop
/// (tool dispatch, compaction triggering, finish_reason handling)
/// doesn't change.
#[allow(clippy::too_many_arguments)]
pub fn run(
    client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
    streaming: bool,
    compaction_cfg: &compaction::CompactionConfig,
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u32>,
    feedback_templates: std::collections::BTreeMap<String, String>,
) -> Result<LoopOutcome> {
    let mut messages = initial_messages;
    let tool_defs: Vec<_> = tools.iter().map(|t| t.to_tool_def()).collect();
    // Set of tool names the model is allowed to call. Drives the
    // plain-text-tool-call promoter (#406): any tool name in the
    // promoted markup that isn't here is rejected so adversarial /
    // malformed output can't smuggle arbitrary tool names into the
    // dispatch pipeline.
    let allowed_tool_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();

    let mut turns: u32 = 0;
    let mut total_prompt_tokens: u32 = 0;
    let mut total_completion_tokens: u32 = 0;
    let mut compactions: u32 = 0;
    let mut latest_prompt_tokens: u32 = 0;
    // (#414 PR A) Per-dispatch budget for intra-turn stall recoveries.
    // Each occurrence of `finish_reason=length` with empty content +
    // no tool_calls (the classic Beat 47 / Run 1 runaway-reasoning
    // shape) consumes one slot. Exhausted budget escalates the
    // dispatch via `IntraTurnStallExhausted` so the operator/frontier
    // can intervene instead of burning more turns on the same stall.
    let mut stall_recoveries_used: u32 = 0;
    // (#418) Per-dispatch cycle detector — warns on repeated tool
    // calls within a sliding window. Observability-only in the MVP;
    // bail-on-cycle is a follow-up if warn alone proves insufficient.
    let mut cycle_detector = CycleDetector::new();
    // (#419) Per-dispatch tool-failure-rate detector — warns on
    // consecutive failures of one tool (e.g., agent retrying gcc
    // inside sandbox where it doesn't exist). Sibling to the cycle
    // detector; same MVP shape (warn-only).
    let mut failure_rate_detector = FailureRateDetector::new();
    // (#461) Per-dispatch reasoning-loop detector — warns when the
    // model's reasoning stream repeats across turns. Catches the
    // Beat 54 Run 5 case where every tool call looks unique but
    // the reasoning is visibly stuck. Sibling of cycle_detector;
    // same sliding-window shape applied to reasoning text instead
    // of tool args.
    let mut reasoning_loop_detector = ReasoningLoopDetector::new();
    // Feedback injection — Step 1 of the feedback-injection primitive
    // (see `feedback.rs`). When cycle/cascade signals fire, the
    // injector queues a synthetic system message; the message is
    // drained at the top of the next loop iteration and prepended to
    // the conversation so the model sees runtime telemetry as
    // model-facing context, not just operator-stderr noise.
    // Operator-disable via `DARKMUX_FEEDBACK_INJECTION=0`.
    // (#457 Step 2) Per-role template overrides come in from the
    // dispatcher via `--feedback-templates-json`; main.rs parses
    // into a BTreeMap and passes here. Empty map = all defaults.
    let mut feedback_injector = FeedbackInjector::with_templates(feedback_templates);

    // (#466) Inactivity-approach soft-warning detector. Tracks the
    // same proof-of-work signals the host-side hard watchdog does
    // (#468: tool.completed and compaction) so a productive
    // dispatch never sees the warning, while a stuck or stalling
    // one gets a graceful wrap-up chance before the 100% hard kill.
    //
    // **Wedged-LMStudio = host-only territory.** When the model is
    // mid-stream in an LMStudio call, the `loop {}` cannot iterate,
    // so the soft check below never runs. The host's hard kill at
    // 100% is the safety net for that case. Soft is best-effort
    // between-turn telemetry; hard is the unconditional kill.
    //
    // - soft threshold: `inactivity_soft_threshold_secs(budget)` — a
    //   linear 75% of the inactivity budget, floored so it's never zero
    //   and capped to leave headroom before the hard kill on small
    //   budgets (#474). Operator-visible via the runtime stderr; queued
    //   into the feedback injector for the model.
    // - `inactivity_budget_secs`: read once from
    //   `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (matches the host's
    //   default of 600s). The host-side watchdog also reads this;
    //   runtime-side tracking mirrors so the soft warning fires
    //   before the host's hard kill at 100% of the same budget.
    // - `last_proof_of_work`: instant of the most recent reset.
    //   Initialized at run() entry; updated on tool.completed and
    //   compaction completed.
    // - `inactivity_soft_warning_fired_in_window`: edge-trigger
    //   flag so the warning fires once per stuck window, not on
    //   every loop iteration.
    let inactivity_budget_secs: u64 = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let mut last_proof_of_work = std::time::Instant::now();
    let mut inactivity_soft_warning_fired_in_window = false;

    // (#465) Test-cadence-drift detector — REDESIGNED from #457's
    // edits-since-last-bash counter. The prior shape mis-fired on
    // productive multi-file edit campaigns and missed genuine
    // single-file thrash (Beat 54 N=5). New shape: track the most
    // recently edited path + a same-file repetition counter.
    //
    // - Edit/write to a NEW path → reset counter to 1, remember path
    //   (path is normalized lexically first, #471, so `./src/x` and
    //   `src/x` aren't seen as different files)
    // - Edit/write to the SAME path as last edit → increment counter
    // - Edit/write with unparseable/path-less args → HOLD state (#472):
    //   no increment, no reset — a transient malformed edit must not
    //   erase an in-progress thrash run
    // - Bash → reset both (verification cleared the slate)
    // - Counter hits THRESHOLD → fire signal, edge-trigger reset
    //
    // Multi-file campaign (one edit per file) never trips. Single-
    // file thrash trips at the 3rd consecutive edit. The path is
    // surfaced into the feedback nudge so the model knows which file
    // it's been thrashing on.
    const TEST_CADENCE_DRIFT_THRESHOLD: u32 = 3;
    let mut last_edited_path: Option<String> = None;
    let mut consecutive_same_file_edits: u32 = 0;

    loop {
        // (#466) Check soft-deadline approach before draining. If the
        // dispatch has gone past 75% of the inactivity budget without
        // a proof-of-work signal AND we haven't already warned in
        // this window, queue the warning so it drains alongside any
        // other pending signals on this iteration. Edge-triggered:
        // the flag clears on the next proof-of-work reset.
        {
            let elapsed_secs = last_proof_of_work.elapsed().as_secs();
            let soft_threshold_secs =
                inactivity_soft_threshold_secs(inactivity_budget_secs);
            if !inactivity_soft_warning_fired_in_window
                && elapsed_secs >= soft_threshold_secs
            {
                eprintln!(
                    "darkmux-runtime: ⚠ inactivity-approach — {}s of {}s budget elapsed \
                     without a proof-of-work signal. Queueing soft warning before the \
                     host-side hard kill.",
                    elapsed_secs, inactivity_budget_secs
                );
                feedback_injector
                    .queue_inactivity_approach(elapsed_secs, inactivity_budget_secs);
                inactivity_soft_warning_fired_in_window = true;
            }
        }

        // Drain any feedback messages queued by signal producers in
        // the prior iteration (cycle/cascade today, more signals in
        // Step 3 of the feedback-injection ladder). Pushes
        // `Message::system()` instances into the conversation BEFORE
        // the next ChatRequest is built, so the model sees the
        // telemetry on its next turn. No-op when the queue is empty
        // or when `DARKMUX_FEEDBACK_INJECTION` is disabled.
        //
        // **Drained-or-discarded**: signals that fire on a turn which
        // then routes to a terminal exit (MAX_TURNS, compaction bail,
        // stall-budget exhausted, stop) are queued but never drained
        // — the loop ends before the next iteration. Acceptable: the
        // signal still reached stderr + trajectory, and the model is
        // about to stop receiving any further nudges anyway.
        let pending_feedback = feedback_injector.drain();
        if !pending_feedback.is_empty() {
            let count = pending_feedback.len();
            // (#457 Step 3) Replace Step 1's combined "cycle_or_cascade"
            // bucket with per-signal-kind discrimination. The injector
            // tracks which kinds were drained on the most recent call;
            // we read them and stamp on the trajectory event so
            // analytics can distinguish cycle / cascade / compaction /
            // cadence-drift firings.
            let kinds = feedback_injector.last_drained_kinds().to_vec();
            messages.extend(pending_feedback);
            trajectory.append_feedback_injected(turns, count, &kinds);
        }
        // (#325, #457) max_turns is operator-opt-in. When set, hitting
        // the cap returns a structured `result: "max_turns"` terminal —
        // distinguishable from Docker / LMStudio failures (which would
        // surface as `result: "error"`). When unset (`None`), the loop
        // runs unbounded turn-count-wise; other bounds (inactivity
        // timeout, per-call token cap, cumulative-tokens cap) still
        // apply if set.
        if let Some(cap) = max_turns {
            if turns >= cap {
                eprintln!(
                    "darkmux-runtime: loop hit max_turns={cap} without reaching stop; \
                     returning partial outcome"
                );
                return Ok(LoopOutcome {
                    terminal_reason: TerminalReason::MaxTurns,
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                });
            }
        }
        // (#423, #457) Cumulative completion-tokens cap is operator-
        // opt-in. When set, hitting it triggers an
        // `EscalationTriggered(CumulativeTokensExceeded)` terminal so
        // the operator's intervention layer can investigate without
        // unbounded cost. When unset (`None`), no cap applies —
        // operators running on their own hardware can let long-arc
        // work continue.
        if let Some(cap) = max_cumulative_tokens {
            if total_completion_tokens >= cap {
                eprintln!(
                    "darkmux-runtime: cumulative completion_tokens={total_completion_tokens} \
                     reached cap max_tokens={cap}; escalating out of local tier with \
                     partial outcome (#423, #457)"
                );
                return Ok(LoopOutcome {
                    terminal_reason: TerminalReason::EscalationTriggered(
                        EscalationReason::CumulativeTokensExceeded,
                    ),
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                });
            }
        }

        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: Some(MAX_TOKENS_PER_CALL),
            response_format: None,
        };

        let next_seq = turns + 1;
        let mut response = if streaming {
            run_streaming_turn(client, &request, next_seq, trajectory)?
        } else {
            client.chat(&request)?
        };
        turns += 1;

        // (#406) Recover plain-text tool calls the model emitted in
        // `content` or `reasoning_content` instead of the structured
        // `tool_calls` field. Three formats recognized: bracket /
        // harmony (mirrors openclaw's `promoteLmstudioPlainTextToolCalls`)
        // and XML (the Qwen 3.x thinking-mode case openclaw doesn't
        // handle today). When promotion fires we flip `finish_reason`
        // to `"tool_calls"` regardless of its incoming value so the
        // downstream match below routes into the dispatch branch — the
        // model intended to call a tool, it just emitted the markup
        // in the wrong field. This catches both `"stop"` (the V4 N=5
        // bail shape) and the rarer `"length"` (which would otherwise
        // hit the context-overflow Err path and throw away a perfectly
        // good recovered call).
        let promotion = response
            .choices
            .first_mut()
            .and_then(|choice| {
                let info = promote_plain_text_tool_calls(&mut choice.message, &allowed_tool_names)?;
                choice.finish_reason = "tool_calls".to_string();
                Some(info)
            });
        if let Some(info) = promotion {
            trajectory.append_tool_call_promoted(
                turns,
                info.source.as_str(),
                info.format.as_str(),
                info.call_count,
            );
        }

        // (#406) Clear `reasoning_content` from the response message
        // now that the promoter has had its chance to scan it. The
        // Message struct's `reasoning_content` field carries a
        // documented invariant (`runtime/src/lmstudio.rs` Message
        // doc): "skip-serialize so outgoing request messages never
        // emit it (always None on the request side)". The streaming
        // path used to enforce this by stripping reasoning via
        // `accumulator.take_reasoning_content()` BEFORE building the
        // response; #406 re-attached it so the promoter could scan
        // it. The original invariant must hold from this point on —
        // the response message is about to be cloned into the
        // conversation history (`messages.push(assistant_message)`)
        // and shipped back to LMStudio on the next request. Carrying
        // reasoning_content into request-side history caused a
        // recursive-feedback regression (Beat 47 attempt 2: run 2
        // hit MAX_TURNS with 100 thinking-mode entries; run 3 went
        // 1235s before runtime exit). Clearing here restores the
        // pre-#406 behavior for the conversation history while
        // preserving the promoter's ability to scan reasoning above.
        // (Promotion-from-reasoning path also clears reasoning_content
        // inside `apply_promotion`, so this is idempotent on that
        // path.)
        if let Some(choice) = response.choices.first_mut() {
            choice.message.reasoning_content = None;
        }

        // (#414 PR A) Capture this turn's completion-token count BEFORE
        // it folds into the cumulative total, so the stall-recovery
        // branch below can record it in the trajectory event. Kept as
        // Option so an absent-usage response (rare) is distinguishable
        // from a legitimate zero in the trajectory event — the event's
        // purpose is to discriminate per-call-cap stalls (count ≈
        // MAX_TOKENS_PER_CALL) from context-overflow stalls, so the
        // distinction matters.
        let this_turn_completion_tokens: Option<u32> =
            response.usage.as_ref().map(|u| u.completion_tokens);
        if let Some(usage) = &response.usage {
            total_prompt_tokens = total_prompt_tokens.saturating_add(usage.prompt_tokens);
            total_completion_tokens =
                total_completion_tokens.saturating_add(usage.completion_tokens);
            latest_prompt_tokens = usage.prompt_tokens;
        }

        // Record model.completed for trajectory. We grab the first
        // choice's finish_reason + tool_calls below; mirror it here.
        let trajectory_finish_reason = response
            .choices
            .first()
            .map(|c| c.finish_reason.clone())
            .unwrap_or_default();
        let trajectory_tool_calls = response
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.as_ref())
            .cloned();
        trajectory.append_model_completed(
            turns,
            &trajectory_finish_reason,
            response.usage.as_ref(),
            trajectory_tool_calls.as_deref(),
        );

        // Take the first choice — LMStudio's OpenAI-compatible endpoint
        // returns exactly one for non-streaming requests, but we don't
        // assume that.
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LMStudio returned no choices"))?;

        let mut assistant_message = choice.message;
        let finish_reason = choice.finish_reason;

        // Extract reasoning content from `<think>...</think>` blocks in
        // the assistant message content (#204). Thinking-mode models
        // (qwen 3.x line, in particular) emit reasoning inline; we
        // surface it as a separate trajectory event so the flow
        // stream + viewer can render it as a collapse/expand block
        // (operator discretion to expand). The original content stays
        // unchanged in `assistant_message` — downstream consumers
        // (compaction, conversation history) see everything as-was.
        let mut per_turn_reasoning = String::new();
        if let Some(content) = assistant_message.content.as_deref() {
            for reasoning_text in extract_think_blocks(content) {
                trajectory.append_model_reasoning(
                    turns,
                    &reasoning_text,
                    "inline-think-tags",
                );
                per_turn_reasoning.push_str(&reasoning_text);
                per_turn_reasoning.push('\n');
            }
        }
        if let Some(separate) = assistant_message.reasoning_content.as_deref() {
            per_turn_reasoning.push_str(separate);
        }
        // (#461) Feed the combined reasoning to the loop detector. The
        // detector skips empty / too-short reasoning internally so
        // turns without reasoning content don't pollute the window.
        if let Some(ReasoningLoopSignal::Suspected { count, window_size }) =
            reasoning_loop_detector.record(&per_turn_reasoning)
        {
            eprintln!(
                "darkmux-runtime: ⚠ reasoning-loop suspected — same reasoning content \
                 appeared {} times in {} turns. Queueing feedback nudge.",
                count, window_size
            );
            trajectory.append_reasoning_loop_suspected(turns, count, window_size);
            feedback_injector.queue_reasoning_loop(count, window_size);
        }

        // (#479) Per-turn-cap salvage. The model hit MAX_TOKENS_PER_CALL
        // on this turn AND the tool call args were well-formed JSON.
        // Discard the truncated content (probably mid-emission) and
        // route the dispatch through the tool_calls path so the
        // well-formed call lands, instead of bailing as the partial-
        // content case does in the length-arm below. Queues a feedback
        // nudge so the model knows what happened. Beat 55 Run 1 was
        // the empirical case: 31K reasoning chars + 1 well-formed
        // tool call → bail pre-#479; salvage post-#479.
        //
        // **Detection runs BEFORE the assistant_message push.** The
        // truncated content (probably reasoning that ran past the cap)
        // is cleared to None when salvage fires, mirroring the
        // stall-arm's `messages.pop()` rationale: leaving the noise
        // in history would anchor the model on the failed pattern
        // AND inflate prompt_tokens on every subsequent turn.
        let salvaged_per_turn_cap = finish_reason == "length"
            && this_turn_completion_tokens == Some(MAX_TOKENS_PER_CALL)
            && assistant_message_has_well_formed_tool_calls(&assistant_message);
        if salvaged_per_turn_cap {
            let salvaged_count = count_well_formed_tool_calls(&assistant_message);
            let observed_tokens = this_turn_completion_tokens.unwrap_or(MAX_TOKENS_PER_CALL);
            eprintln!(
                "darkmux-runtime: ⚡ per-turn-cap salvage — completion_tokens=\
                 {} hit cap {}; dispatching {} well-formed tool call(s) and \
                 nudging the model to reduce per-call reasoning.",
                observed_tokens, MAX_TOKENS_PER_CALL, salvaged_count
            );
            trajectory.append_per_turn_cap_salvaged(
                turns,
                observed_tokens,
                MAX_TOKENS_PER_CALL,
                salvaged_count,
            );
            feedback_injector
                .queue_per_turn_cap_approach(observed_tokens, MAX_TOKENS_PER_CALL);
            // Clear truncated content — keep tool_calls. Mirrors the
            // stall-arm's pop reason: anchoring + prompt-token bloat.
            assistant_message.content = None;
        }
        let effective_finish_reason = if salvaged_per_turn_cap {
            "tool_calls"
        } else {
            finish_reason.as_str()
        };

        // Append the assistant's message to the conversation before we
        // process its tool calls — that's the order the next request
        // needs to see things in. When salvage fired, the content
        // field was cleared above so the truncated reasoning doesn't
        // leak into history.
        messages.push(assistant_message.clone());

        match effective_finish_reason {
            "stop" => {
                return Ok(LoopOutcome {
                    terminal_reason: TerminalReason::Stop,
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                });
            }
            "tool_calls" => {
                let calls = assistant_message
                    .tool_calls
                    .clone()
                    .unwrap_or_default();

                if calls.is_empty() {
                    return Err(anyhow!(
                        "finish_reason=tool_calls but assistant message had no tool_calls"
                    ));
                }

                // Dispatch each call; append a `tool` message per
                // result so the next request shows the model exactly
                // what each tool returned. Trajectory records each
                // tool.completed event so the operator can see what
                // ran post-dispatch.
                for (tool_seq, call) in calls.into_iter().enumerate() {
                    // (#418) Record the call into the cycle detector
                    // BEFORE dispatch so the suspicion event lands
                    // immediately next to the tool.completed event in
                    // trajectory order. Edge-triggered: same hash
                    // continuing to repeat does NOT re-fire.
                    if let Some(CycleSignal::Suspected {
                        tool_name,
                        canonical_args,
                        count,
                        window_size,
                    }) = cycle_detector.record(&call.function.name, &call.function.arguments)
                    {
                        eprintln!(
                            "darkmux-runtime: ⟳ cycle suspected — tool `{}` called {} times in \
                             the last {} turns with the same canonical args. Operator-visible \
                             only; no behavior change.",
                            tool_name, count, window_size
                        );
                        trajectory.append_cycle_suspected(
                            turns,
                            &tool_name,
                            &canonical_args,
                            count,
                            window_size,
                        );
                        // Step 1 of feedback injection — route the
                        // same signal that goes to stderr/trajectory
                        // INTO the model's next-turn prompt as a
                        // synthetic system message. Drains at top of
                        // next loop iteration.
                        feedback_injector.queue_cycle_suspected(
                            &tool_name,
                            count,
                            window_size,
                        );
                    }
                    let result = dispatch(&call.function.name, &call.function.arguments);
                    trajectory.append_tool_completed(
                        turns,
                        tool_seq as u32,
                        &call.function.name,
                        call.function.arguments.len(),
                        result.len(),
                    );
                    // (#466) Proof-of-work signal for the inactivity-
                    // approach detector. Mirrors the host-side #468
                    // reset trigger so the runtime-side soft warning
                    // and the host-side hard kill share the same
                    // deadline semantics. Fires on success AND failure
                    // (same gap as #469); the cycle + failure-rate
                    // detectors guard the fast-fail seam.
                    last_proof_of_work = std::time::Instant::now();
                    inactivity_soft_warning_fired_in_window = false;
                    // (#465) Track test-cadence drift via same-file
                    // repetition. See state-machine doc above the
                    // declaration of `last_edited_path` for full
                    // rationale. Edge-triggered: counter + path reset
                    // after firing so the next nudge requires another
                    // THRESHOLD consecutive same-file edits.
                    match call.function.name.as_str() {
                        "edit" | "write" => {
                            let path =
                                extract_edit_target_path(&call.function.arguments);
                            match (path.as_deref(), last_edited_path.as_deref()) {
                                (Some(p), Some(last)) if p == last => {
                                    consecutive_same_file_edits =
                                        consecutive_same_file_edits.saturating_add(1);
                                }
                                (Some(p), _) => {
                                    last_edited_path = Some(p.to_string());
                                    consecutive_same_file_edits = 1;
                                }
                                (None, _) => {
                                    // (#472) Malformed args (or a path-less
                                    // write shape we don't recognize) — treat
                                    // as a no-op for the detector: hold state,
                                    // neither increment nor reset. Resetting
                                    // here would let a degraded coder thrashing
                                    // on one file dodge the detector by
                                    // occasionally emitting malformed tool args.
                                    // A real bash verification (below) is what
                                    // clears the slate, not a parse failure.
                                }
                            }
                            if consecutive_same_file_edits >= TEST_CADENCE_DRIFT_THRESHOLD {
                                let path_for_signal =
                                    last_edited_path.clone().unwrap_or_default();
                                eprintln!(
                                    "darkmux-runtime: ⚠ test-cadence drift — {} \
                                     consecutive edits to `{}` without a bash \
                                     verification call. Queueing feedback nudge.",
                                    consecutive_same_file_edits, path_for_signal
                                );
                                feedback_injector.queue_test_cadence_drift(
                                    consecutive_same_file_edits,
                                    &path_for_signal,
                                );
                                // Edge-trigger reset: next firing
                                // requires another full THRESHOLD
                                // consecutive same-file edits.
                                consecutive_same_file_edits = 0;
                                last_edited_path = None;
                            }
                        }
                        "bash" => {
                            // Verification cleared the slate.
                            consecutive_same_file_edits = 0;
                            last_edited_path = None;
                        }
                        _ => {}
                    }
                    // (#419) Record into the failure-rate detector
                    // AFTER dispatch so the result is available to
                    // classify. Edge-triggered: counter resets on a
                    // single success, warn fires once per cascade.
                    if let Some(FailureCascadeSignal::Suspected {
                        tool_name,
                        consecutive_failures,
                    }) = failure_rate_detector.record(
                        &call.function.name,
                        &call.function.arguments,
                        &result,
                    )
                    {
                        eprintln!(
                            "darkmux-runtime: ✕ tool-failure cascade — `{}` failed {} times in a \
                             row. The tool or its environment may need operator attention. \
                             Operator-visible only; no behavior change.",
                            tool_name, consecutive_failures
                        );
                        trajectory.append_tool_repeated_failure(
                            turns,
                            &tool_name,
                            consecutive_failures,
                        );
                        // Step 1 of feedback injection — see cycle-
                        // suspected callsite above for the rationale.
                        // `consecutive_failures` is `u32` at the
                        // signal layer; cast to `usize` to match the
                        // injector's API (which uses `usize` for
                        // counter fields uniformly).
                        feedback_injector.queue_tool_failure_cascade(
                            &tool_name,
                            consecutive_failures as usize,
                        );
                    }
                    messages.push(Message::tool_result(
                        call.id,
                        call.function.name,
                        result,
                    ));
                }

                // Phase 6: check whether the most recent prompt's
                // token count crossed the compaction threshold, AND
                // whether the conversation is long enough to compact.
                // If so, compact BEFORE the next chat() call so the
                // next request sees a smaller message thread.
                if compaction::needs_compaction(
                    latest_prompt_tokens,
                    messages.len(),
                    compaction_cfg,
                ) {
                    let before_count = messages.len();
                    compactions = compactions.saturating_add(1);
                    // (#372 T2-C) Route by strategy. Narrative is
                    // today's default (prose summary as synthetic
                    // USER message). StructuredSlot is tier-2 (typed
                    // schema + JSON mode + SYSTEM message); on
                    // success the parsed output is persisted to
                    // `/workspace/.darkmux-runtime/compaction-<gen>.json`
                    // per #352 Step 5 "persistence falls out for free."
                    match compaction_cfg.strategy {
                        compaction::CompactionStrategy::Narrative => {
                            compaction::compact(
                                client,
                                &mut messages,
                                compactions,
                                compaction_cfg,
                            )?;
                        }
                        compaction::CompactionStrategy::StructuredSlot => {
                            // (#439) Build budget snapshot so the
                            // compacted SYSTEM message can surface
                            // remaining budget to the model. Lets
                            // the model pace within bounds + use the
                            // BLOCKED: escalation convention before
                            // cap exhaustion.
                            let budget = compaction::BudgetSnapshot {
                                turns_used: turns,
                                // (#457) Pass-through of the operator-
                                // set caps (None = unlimited; renderer
                                // skips the corresponding budget line).
                                max_turns,
                                cumulative_completion_tokens_used: total_completion_tokens,
                                max_cumulative_completion_tokens: max_cumulative_tokens,
                                max_tokens_per_call: MAX_TOKENS_PER_CALL,
                            };
                            let parsed = compaction::structured_compact(
                                client,
                                &mut messages,
                                compactions,
                                compaction_cfg,
                                Some(budget),
                            )?;
                            // Persist the JSON for downstream
                            // consumers (replay, methodology
                            // research, cross-sprint memory). Best-
                            // effort: a write failure logs but does
                            // NOT fail the dispatch — observability,
                            // not correctness.
                            persist_structured_compaction_output(
                                std::path::Path::new("/workspace/.darkmux-runtime"),
                                compactions,
                                &parsed,
                            );
                        }
                    }
                    let after_count = messages.len();
                    // Approximate summary_chars from the inserted
                    // synthetic message at PRESERVE_HEAD position
                    // (works for both Narrative=user + StructuredSlot=system).
                    let summary_chars = messages
                        .get(2)
                        .and_then(|m| m.content.as_ref())
                        .map(|c| c.len())
                        .unwrap_or(0);
                    trajectory.append_compaction(
                        compactions,
                        before_count,
                        after_count,
                        summary_chars,
                    );

                    // (#457 Step 3) Post-compaction feedback nudge.
                    // The model's working state was just compressed
                    // (compactions of 26+ messages → ~1500-char
                    // summary); orient it toward the smallest concrete
                    // next step rather than re-reading everything
                    // (Beat 45's retrace pattern). Fires once per
                    // compaction event; drains at the top of the next
                    // loop iteration alongside any cycle/cascade
                    // signals from this turn.
                    feedback_injector.queue_post_compaction(turns);
                    // (#466) Compaction is a proof-of-work signal for
                    // the inactivity-approach detector. Same trigger
                    // set as #468 on the host-side reset.
                    last_proof_of_work = std::time::Instant::now();
                    inactivity_soft_warning_fired_in_window = false;

                    // (#377) Escalation bound check. After persisting
                    // this compaction's trajectory entry, see whether
                    // we've crossed the operator-configured
                    // `bail_after_compactions`. If yes, bail with
                    // EscalationTriggered so the frontier-tier handoff
                    // skill picks up the salvageable state instead of
                    // burning more local-tier cycles. KISS-doubled
                    // (Beat 44 closure): bound the cost, escalate past
                    // the bound. The check is AFTER the trajectory
                    // append so the bound-crossing compaction is still
                    // observable + persisted; only the next chat()
                    // call is skipped.
                    if let Some(bail) = compaction_cfg.bail_after_compactions {
                        if compactions >= bail {
                            eprintln!(
                                "darkmux-runtime: escalation_triggered — \
                                 compactions ({compactions}) reached bail_after_compactions ({bail}); \
                                 emitting EscalationTriggered terminal for frontier handoff"
                            );
                            return Ok(LoopOutcome {
                                terminal_reason: TerminalReason::EscalationTriggered(
                                    EscalationReason::CompactionLimitReached,
                                ),
                                messages,
                                turns,
                                total_prompt_tokens,
                                total_completion_tokens,
                                compactions,
                            });
                        }
                    }
                }

                // Loop back and call chat() again.
                continue;
            }
            "length" => {
                // (#414 PR A) Detect the runaway-reasoning shape:
                // finish_reason=length AND content empty AND no
                // tool_calls. This is the Beat 47 / Run 1 pattern —
                // the model emitted up to the per-call cap entirely
                // in reasoning tokens, producing nothing actionable.
                // The other length-shape (real content truncated
                // mid-emission, OR truncated mid-tool-args) is not
                // recoverable in the same way and stays a hard error.
                // Read the just-landed turn shape directly from
                // `assistant_message` (still in scope) rather than from
                // `messages.last()`. Avoids a brittle `.expect()` on a
                // future refactor that pushes the message conditionally.
                let content_empty = assistant_message
                    .content
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);
                let no_tool_calls = assistant_message
                    .tool_calls
                    .as_ref()
                    .map(|tc| tc.is_empty())
                    .unwrap_or(true);
                let is_useless_stall = content_empty && no_tool_calls;

                if !is_useless_stall {
                    return Err(anyhow!(
                        "model returned finish_reason=length with partial \
                         content or partial tool_calls — the response was \
                         cut off mid-generation but salvage isn't safe. \
                         Two causes are possible: \
                         (1) per-call cap fired (MAX_TOKENS_PER_CALL = {MAX_TOKENS_PER_CALL}); \
                         the model emitted that many tokens (content + reasoning) \
                         in this single turn. \
                         (2) context overflow; prompt_tokens crossed the model's \
                         loaded context window. Check `usage.completion_tokens` in \
                         the trajectory's model.completed event: if it equals \
                         {MAX_TOKENS_PER_CALL}, cause (1); otherwise cause (2). \
                         If (2), compaction may need a smaller threshold or a larger n_ctx."
                    ));
                }

                // Budget check FIRST so an exhausted-budget escalation
                // doesn't have to also account for the unproductive
                // turn that just landed.
                if stall_recoveries_used >= MAX_STALL_RECOVERIES {
                    eprintln!(
                        "darkmux-runtime: escalation_triggered — intra-turn \
                         stall recovery budget ({MAX_STALL_RECOVERIES}) exhausted; \
                         {stall_recoveries_used} prior recoveries didn't break the \
                         pattern. Emitting EscalationTriggered for frontier handoff."
                    );
                    return Ok(LoopOutcome {
                        terminal_reason: TerminalReason::EscalationTriggered(
                            EscalationReason::IntraTurnStallExhausted,
                        ),
                        messages,
                        turns,
                        total_prompt_tokens,
                        total_completion_tokens,
                        compactions,
                    });
                }

                // Recover: drop the useless turn from history (it's
                // pure reasoning noise — leaving it would anchor the
                // model on the same failed pattern), record a stall-
                // recovered trajectory event for observability, then
                // append a system-role nudge naming the two valid
                // next moves (tool call or final answer). The next
                // iteration's chat() call will see the augmented
                // conversation.
                messages.pop();
                stall_recoveries_used = stall_recoveries_used.saturating_add(1);
                trajectory.append_intra_turn_stall_recovered(
                    turns,
                    this_turn_completion_tokens,
                    stall_recoveries_used,
                    MAX_STALL_RECOVERIES,
                );
                messages.push(Message::system(STALL_NUDGE_MESSAGE));
                let tokens_str = this_turn_completion_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                eprintln!(
                    "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} \
                     emitted {tokens_str} completion tokens with no \
                     content and no tool calls (runaway-reasoning shape). Dropped \
                     the useless turn, injected a nudge; budget {stall_recoveries_used}/{MAX_STALL_RECOVERIES} used. (#414)"
                );
                continue;
            }
            other => {
                return Err(anyhow!(
                    "unexpected finish_reason: {other} — runtime doesn't know \
                     how to handle this. Aborting."
                ));
            }
        }
    }
}

/// Run one SSE-streamed turn: consume the chunk iterator, emit a
/// `model.partial` trajectory event per chunk (stats only — no content
/// in the events to keep `trajectory.jsonl` bounded), and return the
/// accumulated `ChatResponse` shaped identically to a non-streaming
/// response. (#205)
///
/// Reasoning content delivered via the separate-field stream
/// (`Delta.reasoning_content`, the Qwen 3 / DeepSeek pattern) is
/// extracted from the accumulator and emitted as a `model.reasoning`
/// trajectory event with `format=separate-field`, mirroring the
/// inline-`<think>`-tag path that the caller handles post-turn.
fn run_streaming_turn(
    client: &LmStudioClient,
    request: &ChatRequest,
    seq: u32,
    trajectory: &mut Trajectory,
) -> Result<crate::lmstudio::ChatResponse> {
    let (system_chars, prompt_chars) = measure_request_context(&request.messages);
    trajectory.append_model_streaming_start(seq, system_chars, prompt_chars);
    let mut accumulator = ChunkAccumulator::new();
    let mut last_content_bytes: usize = 0;
    let stream = client.chat_streaming(request)?;
    for chunk_result in stream {
        let chunk = chunk_result?;
        let partial_index = accumulator.ingest(&chunk);
        let cumulative = accumulator.content_bytes();
        let delta_bytes = cumulative.saturating_sub(last_content_bytes);
        last_content_bytes = cumulative;
        trajectory.append_model_partial(
            seq,
            partial_index,
            delta_bytes,
            cumulative,
            accumulator.has_tool_calls(),
        );
    }
    let partial_count = accumulator.partial_count();
    let total_content = accumulator.content_bytes();
    let reasoning_content = accumulator.take_reasoning_content();
    let mut response = accumulator.into_response();
    let tool_calls_count = response
        .choices
        .first()
        .and_then(|c| c.message.tool_calls.as_ref())
        .map(|tc| tc.len())
        .unwrap_or(0);
    trajectory.append_model_streaming_end(seq, partial_count, total_content, tool_calls_count);
    if let Some(reasoning) = reasoning_content {
        trajectory.append_model_reasoning(seq, &reasoning, "separate-field");
        // (#406) Surface reasoning_content on the response message so
        // the caller's plain-text-tool-call promoter can scan it.
        // Without this the streaming path loses the reasoning field
        // before promotion runs — and Qwen 3.x thinking-mode bails
        // ride in reasoning, not content.
        if let Some(choice) = response.choices.first_mut() {
            choice.message.reasoning_content = Some(reasoning);
        }
    }
    Ok(response)
}

/// Measure per-turn context size: returns `(system_chars, prompt_chars)`.
/// `system_chars` is the total length of system-role message content;
/// `prompt_chars` is the total length of every other message — user
/// content, assistant text, assistant tool-call args (function name +
/// arguments JSON string), tool-result content. Stamped on
/// `model.streaming.start` (#361) so operators can read per-turn
/// context growth straight from the trajectory, independent of
/// whether LMStudio's `usage` field arrived (#360).
///
/// **Counting choice**: we measure what the MODEL ATTENDS TO, not the
/// wire-framing bytes. `tool_call.id` / `tool_call.kind` (always
/// `"function"`) / message-envelope fields are excluded — those are
/// transport-shape that doesn't carry semantic information the model
/// reasons over. Future telemetry layers that need wire bytes (for
/// API-cost calculations) should compute that separately rather than
/// extend this function.
/// (#479) Test whether the assistant message has at least one tool call
/// with well-formed JSON arguments. Used by the per-turn-cap salvage
/// path: when `finish_reason=length` lands with `completion_tokens` at
/// the cap, the runtime salvages the tool call(s) ONLY when their args
/// are well-formed JSON (i.e., the model finished emitting the call
/// before being truncated on a subsequent reasoning run-on). Partial
/// JSON args are NOT salvageable — dispatching with broken args would
/// produce noise. The existing bail handles that case.
fn assistant_message_has_well_formed_tool_calls(msg: &Message) -> bool {
    msg.tool_calls
        .as_ref()
        .map(|tcs| {
            !tcs.is_empty()
                && tcs
                    .iter()
                    .any(|tc| serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok())
        })
        .unwrap_or(false)
}

/// (#479) Count tool calls with well-formed JSON arguments. Companion
/// to `assistant_message_has_well_formed_tool_calls` — the boolean
/// predicate is for the detection decision; this returns the exact
/// count for the trajectory event + operator-visible eprintln. Sharing
/// the "well-formed" definition between predicate + count keeps the
/// two in sync if the definition ever evolves.
fn count_well_formed_tool_calls(msg: &Message) -> usize {
    msg.tool_calls
        .as_ref()
        .map(|tcs| {
            tcs.iter()
                .filter(|tc| {
                    serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok()
                })
                .count()
        })
        .unwrap_or(0)
}

/// (#465) Extract the `path` field from a tool call's JSON arguments —
/// used by the test-cadence-drift detector to recognize "same file
/// edited again" vs "moved to a different file" without coupling to
/// the typed `EditArgs`/`WriteArgs` structs in the tools module.
///
/// Returns `None` if the JSON doesn't parse, or if `path` is missing
/// or non-string. Callers degrade safely on `None` (don't increment
/// the repetition counter — treat as "unknown target").
fn extract_edit_target_path(raw_args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_args)
        .ok()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
        .map(|p| {
            // (#471) Lexically normalize so `./src/lib.rs`, `src/lib.rs/`,
            // and `src/../src/lib.rs` all compare equal in the same-file
            // drift check. Purely lexical — no filesystem access (the
            // sandbox path may not exist on the host, and canonicalize
            // would add a syscall per edit).
            let n = normalize_path_lexical(&p);
            if n.is_empty() {
                p
            } else {
                n
            }
        })
}

/// Inactivity soft-warning threshold (seconds) for a given budget (#466,
/// hardened in #474). Base is a linear 75% of the budget, but guarded for
/// small operator-tuned budgets: never zero (so it can't fire on loop
/// iteration 1), and at least `MIN_HEADROOM` before the host's hard kill
/// when the budget can afford it — the model needs a real window to wrap
/// up, not 1-2s. For budgets too small for full headroom it degrades to
/// "at most one tick before the kill" rather than firing immediately.
fn inactivity_soft_threshold_secs(budget_secs: u64) -> u64 {
    const RATIO: f64 = 0.75;
    const MIN_HEADROOM_SECS: u64 = 30;
    let linear = ((budget_secs as f64) * RATIO) as u64;
    let floored = linear.max(1);
    let headroom_cap = budget_secs.saturating_sub(MIN_HEADROOM_SECS);
    if headroom_cap >= 1 {
        floored.min(headroom_cap)
    } else {
        floored.min(budget_secs.saturating_sub(1)).max(1)
    }
}

/// Lexically clean a path: drop `.` components, fold `..` against the
/// preceding normal component, and drop trailing separators — without
/// touching the filesystem (unlike `Path::canonicalize`). Leading `..`
/// (no preceding component to pop) is preserved. (#471)
fn normalize_path_lexical(p: &str) -> String {
    use std::path::{Component, Path, PathBuf};
    let mut out = PathBuf::new();
    for comp in Path::new(p).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::Prefix(pre) => out.push(pre.as_os_str()),
            Component::Normal(seg) => out.push(seg),
        }
    }
    out.to_string_lossy().into_owned()
}

fn measure_request_context(messages: &[Message]) -> (usize, usize) {
    let mut system_chars = 0usize;
    let mut prompt_chars = 0usize;
    for m in messages {
        let content_len = m.content.as_ref().map(|s| s.len()).unwrap_or(0);
        let tool_args_len: usize = m
            .tool_calls
            .as_ref()
            .map(|tcs| {
                tcs.iter()
                    .map(|tc| tc.function.name.len() + tc.function.arguments.len())
                    .sum()
            })
            .unwrap_or(0);
        let total = content_len + tool_args_len;
        if m.role == "system" {
            system_chars += total;
        } else {
            prompt_chars += total;
        }
    }
    (system_chars, prompt_chars)
}

/// (#372 T2-C) Best-effort write of the parsed structured-compaction
/// output to `<runtime_dir>/compaction-<generation>.json`. Creates
/// the parent directory if needed. Write failures log to stderr but
/// do NOT propagate — persistence is observability (replay,
/// methodology research, cross-sprint memory) not correctness, per
/// #352 "persistence falls out for free" framing.
fn persist_structured_compaction_output(
    runtime_dir: &std::path::Path,
    generation: u32,
    output: &compaction::StructuredCompactionOutput,
) {
    if let Err(e) = std::fs::create_dir_all(runtime_dir) {
        eprintln!(
            "darkmux-runtime: persist compaction #{generation} — create dir failed: {e}"
        );
        return;
    }
    let path = runtime_dir.join(format!("compaction-{generation}.json"));
    let json = match serde_json::to_string_pretty(output) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "darkmux-runtime: persist compaction #{generation} — serialize failed: {e}"
            );
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, json) {
        eprintln!(
            "darkmux-runtime: persist compaction #{generation} — write to {} failed: {e}",
            path.display()
        );
    }
}

/// Extract `<think>...</think>` block contents from a string. Returns
/// each block's inner text (without the tags) in order. Returns empty
/// vec when no blocks are present.
///
/// Used to surface reasoning content as separate trajectory events
/// (#204). qwen 3.x thinking-mode models emit reasoning inline in the
/// assistant message content wrapped in these tags; we extract for the
/// flow stream + viewer but leave the original content untouched.
///
/// Implementation is a tag-scan, not a regex — keeps the runtime free
/// of regex deps and handles nested tags by treating the outermost
/// pairs as the boundary. Malformed (unclosed) tags are ignored.
fn extract_think_blocks(content: &str) -> Vec<String> {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some(open_at) = content[cursor..].find(OPEN) {
        let start = cursor + open_at + OPEN.len();
        if let Some(close_offset) = content[start..].find(CLOSE) {
            blocks.push(content[start..start + close_offset].to_string());
            cursor = start + close_offset + CLOSE.len();
        } else {
            // Unclosed tag — stop scanning to avoid runaway capture.
            break;
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lmstudio::{FunctionCall, ToolCall};

    // ─── #372 T2-C: persist_structured_compaction_output ──────────

    use crate::compaction::{CompactionMetadata, CurrentTruth, StructuredCompactionOutput};

    fn dummy_structured_output(generation: u32) -> StructuredCompactionOutput {
        StructuredCompactionOutput {
            objective: "test obj".into(),
            current_truth: CurrentTruth::default(),
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation,
                source_message_count: 5,
            truncation_patched: None,
            turns_used: None,
            max_turns: None,
            cumulative_completion_tokens_used: None,
            max_cumulative_completion_tokens: None,
            max_tokens_per_call: None,
            },
            completed_decisions: None,
            errors_to_preserve: None,
            next_concrete_actions: None,
            verify_criteria: None,
            sprint_id: None,
        }
    }

    #[test]
    fn persist_writes_compaction_json_to_runtime_dir() {
        let tmp = tempdir::TempDir::new("persist-compaction").unwrap();
        let runtime_dir = tmp.path().join(".darkmux-runtime");
        let out = dummy_structured_output(3);
        persist_structured_compaction_output(&runtime_dir, 3, &out);
        let written = runtime_dir.join("compaction-3.json");
        assert!(
            written.exists(),
            "expected compaction-3.json at {}",
            written.display()
        );
        let body = std::fs::read_to_string(&written).unwrap();
        let parsed: StructuredCompactionOutput =
            serde_json::from_str(&body).expect("written JSON round-trips");
        assert_eq!(parsed.compaction_metadata.generation, 3);
        assert_eq!(parsed.objective, "test obj");
    }

    #[test]
    fn persist_creates_runtime_dir_if_missing() {
        let tmp = tempdir::TempDir::new("persist-mkdir").unwrap();
        // Subdir that doesn't exist yet — persist must create it.
        let runtime_dir = tmp.path().join("nested").join("not-yet").join(".darkmux-runtime");
        let out = dummy_structured_output(1);
        persist_structured_compaction_output(&runtime_dir, 1, &out);
        assert!(runtime_dir.join("compaction-1.json").exists());
    }

    #[test]
    fn persist_silently_skips_when_dir_unwritable() {
        // Path under a regular file (can't be a dir) — write should
        // fail silently, NOT panic or propagate. Persistence is
        // observability, not correctness.
        let tmp = tempdir::TempDir::new("persist-unwritable").unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file not a dir").unwrap();
        let runtime_dir = blocker.join("under-a-file");
        let out = dummy_structured_output(2);
        // Should NOT panic.
        persist_structured_compaction_output(&runtime_dir, 2, &out);
    }

    // ─── measure_request_context (#361 fix) ─────────────────────────

    #[test]
    fn measure_empty_messages_returns_zero_zero() {
        let (s, p) = measure_request_context(&[]);
        assert_eq!(s, 0);
        assert_eq!(p, 0);
    }

    #[test]
    fn measure_system_and_user_routes_to_correct_bucket() {
        let messages = vec![Message::system("sys prompt"), Message::user("hello")];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, "sys prompt".len());
        assert_eq!(prompt, "hello".len());
    }

    #[test]
    fn measure_counts_assistant_tool_calls_into_prompt() {
        let assistant_with_tools = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"/workspace/file.py"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        let (system, prompt) = measure_request_context(&[assistant_with_tools]);
        assert_eq!(system, 0);
        // name + arguments lengths — sanity-check the sum.
        assert_eq!(
            prompt,
            "read".len() + r#"{"path":"/workspace/file.py"}"#.len()
        );
    }

    #[test]
    fn measure_counts_tool_result_into_prompt() {
        let messages = vec![Message {
            role: "tool".into(),
            content: Some("file contents".into()),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            name: Some("read".into()),
            reasoning_content: None,
        }];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, 0);
        assert_eq!(prompt, "file contents".len());
    }

    #[test]
    fn measure_typical_turn_buckets_correctly() {
        // System + user + assistant (with content + tool calls) + tool result.
        let messages = vec![
            Message::system("you are coder"),
            Message::user("fix the bug"),
            Message {
                role: "assistant".into(),
                content: Some("I'll read the file first.".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "read".into(),
                        arguments: r#"{"path":"/x"}"#.into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
            },
            Message {
                role: "tool".into(),
                content: Some("def foo():\n    pass".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: Some("read".into()),
                reasoning_content: None,
            },
        ];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, "you are coder".len());
        let expected_prompt = "fix the bug".len()
            + "I'll read the file first.".len()
            + "read".len()
            + r#"{"path":"/x"}"#.len()
            + "def foo():\n    pass".len();
        assert_eq!(prompt, expected_prompt);
    }

    #[test]
    fn extract_think_blocks_none() {
        assert_eq!(extract_think_blocks("just plain content"), Vec::<String>::new());
    }

    #[test]
    fn extract_think_blocks_single() {
        let content = "Before <think>my reasoning here</think> after.";
        assert_eq!(extract_think_blocks(content), vec!["my reasoning here"]);
    }

    #[test]
    fn extract_think_blocks_multiple() {
        let content =
            "<think>first thought</think>\nresponse\n<think>second thought</think>";
        assert_eq!(
            extract_think_blocks(content),
            vec!["first thought", "second thought"]
        );
    }

    #[test]
    fn extract_think_blocks_multiline() {
        let content = "<think>line one\nline two\nline three</think>";
        assert_eq!(
            extract_think_blocks(content),
            vec!["line one\nline two\nline three"]
        );
    }

    #[test]
    fn extract_think_blocks_unclosed_tag_skipped() {
        // Unclosed tag mid-content — return whatever closed blocks came
        // before, ignore the unclosed one.
        let content = "<think>closed</think> middle <think>unclosed forever";
        assert_eq!(extract_think_blocks(content), vec!["closed"]);
    }

    #[test]
    fn extract_think_blocks_empty_inside() {
        let content = "<think></think>";
        assert_eq!(extract_think_blocks(content), vec![""]);
    }

    // ─── compaction loop integration (against mock LMStudio) ──────────
    //
    // These tests verify the end-to-end loop behavior — the predicate
    // tests in compaction.rs cover "should compaction fire?"; these
    // cover "does the runtime actually invoke the compactor model when
    // the predicate trips?" That's the layer-boundary gap pre-fix
    // didn't have coverage for.
    //
    // The mock LMStudio (httpmock) lets the test:
    //   - drive a deterministic sequence of chat responses
    //   - inspect which `model` each request used (primary vs compactor)
    //   - assert the compactor was called the expected number of times
    //
    // No real LMStudio + no Docker required. The non-streaming code
    // path is exercised (streaming=false) — the streaming path's
    // compaction behavior is structurally identical (uses the same
    // `needs_compaction` + `compact` calls) and is covered by the
    // companion test below.

    use crate::lmstudio::{LmStudioClient, Message};
    use crate::tools::Tool;
    use crate::trajectory::Trajectory;
    use httpmock::prelude::*;

    /// Build a non-streaming chat-completion response body the way
    /// LMStudio would return it. Tests use this to construct
    /// deterministic turn-by-turn responses.
    fn chat_response_json(
        content: Option<&str>,
        tool_calls: Option<serde_json::Value>,
        finish_reason: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> serde_json::Value {
        let mut message = serde_json::json!({ "role": "assistant" });
        if let Some(c) = content {
            message["content"] = serde_json::json!(c);
        } else {
            message["content"] = serde_json::Value::Null;
        }
        if let Some(tc) = tool_calls {
            message["tool_calls"] = tc;
        }
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "ignored-by-test",
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        })
    }

    /// #325: terminal_reason discriminates loop outcomes. A finish_reason=
    /// stop response from the model produces TerminalReason::Stop;
    /// a loop that runs out the MAX_TURNS clock produces
    /// TerminalReason::MaxTurns (NOT an Err — that path was reserved
    /// for infrastructure failures).
    ///
    /// (#423) Mock returns turns that each report high completion_tokens
    /// (close to per-call cap). After enough turns, cumulative crosses
    /// MAX_CUMULATIVE_COMPLETION_TOKENS=250000 and the loop should
    /// escalate with `EscalationTriggered(CumulativeTokensExceeded)`
    /// BEFORE hitting MAX_TURNS. Distinguishes from MaxTurns because
    /// the cumulative bail fires earlier in the dispatch lifecycle on
    /// pathological emission patterns.
    #[test]
    #[serial_test::serial]
    fn loop_escalates_when_cumulative_completion_tokens_exceeds_cap() {
        let server = MockServer::start();
        // Each turn reports 10000 completion tokens (the per-call
        // cap). After 25 turns cumulative = 250000 == cap → next
        // iteration's pre-loop check trips the escalation.
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_burner",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                100,
                10000, // per-turn completion_tokens hits the per-call cap
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("cumtokens").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("burn budget")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // (#457) Test specifically exercises the cumulative-tokens cap.
        // After the cap became operator-opt-in (default None = unlimited),
        // we have to pass an explicit Some() here or the loop runs
        // unbounded against a mock that returns infinite identical
        // length-finish responses. 250000 matches the prior hardcoded
        // default value the test was originally written against.
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), Some(250_000), std::collections::BTreeMap::new())
            .expect("cumulative-budget escalation returns Ok(outcome)");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CumulativeTokensExceeded),
            "expected CumulativeTokensExceeded escalation, got {:?}",
            outcome.terminal_reason
        );
        // Sanity: bailed BEFORE MAX_TURNS — must have hit the cap.
        assert!(
            outcome.turns < 100,
            "cumulative bail must fire before MAX_TURNS; got turns={}",
            outcome.turns
        );
        // The cumulative-tokens sum must have crossed the cap.
        assert!(
            outcome.total_completion_tokens >= 250_000,
            "cumulative bail fires when sum >= 250000; got {}",
            outcome.total_completion_tokens
        );
    }

    /// (#423) Negative case: when each turn reports modest token
    /// usage and the loop terminates normally on stop, the
    /// cumulative-budget check must NOT trip. Asserts the normal
    /// stop path still fires for healthy dispatches.
    #[test]
    #[serial_test::serial]
    fn loop_does_not_escalate_when_under_cumulative_budget() {
        let server = MockServer::start();
        let _stop_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                100,
                500, // healthy per-turn usage, well under any cap
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("under-budget").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // (#457) Counter-test to the cap-fire path. Set Some(250_000)
        // for parity with the cap-fire test; the mock returns a stop
        // turn quickly so we never approach it.
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), Some(250_000), std::collections::BTreeMap::new())
            .expect("healthy stop should not bail");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert!(outcome.total_completion_tokens < 250_000);
    }

    /// This test pairs with the existing
    /// `loop_runs_against_mock_and_terminates_on_stop` (Stop case)
    /// to lock both terminal reasons. MaxTurns specifically asserts
    /// the loop returns Ok(outcome) — the JSON envelope path in
    /// main.rs reads outcome.terminal_reason and emits result=max_turns.
    #[test]
    #[serial_test::serial]
    fn loop_returns_maxturns_terminal_reason_when_cap_hit() {
        let server = MockServer::start();
        // Primary mock: every call returns finish_reason=tool_calls.
        // The loop will never see stop; will run MAX_TURNS=100 turns
        // and bail with the structured terminal_reason.
        let _primary = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/missing.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("maxturns").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop forever")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // (#457) Test exercises the MaxTurns terminal — needs Some(N)
        // for the cap to fire. 100 matches the prior hardcoded default.
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("MAX_TURNS path returns Ok(outcome), not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "expected MaxTurns terminal_reason after exhausting the loop"
        );
        // Sanity: hit the cap.
        assert!(
            outcome.turns >= 100,
            "expected >= MAX_TURNS turns; got {}",
            outcome.turns
        );
    }

    /// (#419) Mock returns the same `bash` tool call repeatedly;
    /// the bash command targets a nonexistent path so each dispatch
    /// returns a non-zero exit ("tool 'bash' returned error: ..."
    /// pattern). After 3 consecutive failures, the failure-rate
    /// detector should emit `dispatch.tool.repeated_failure` into
    /// the trajectory. Edge-triggered: only one event despite many
    /// more failed calls.
    #[test]
    #[serial_test::serial]
    fn loop_emits_tool_repeated_failure_event_after_third_consecutive_bash_failure() {
        let server = MockServer::start();
        // Each turn the mock returns a bash call against a path that
        // doesn't exist in the test workspace → tool returns
        // "exit: N" with non-zero exit. The dispatch wrapper still
        // returns Ok(text), but the text classifies as a failure.
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_failboat",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"false\",\"timeout_seconds\":5}",
                    },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("failure-rate").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop fail")];
        let tools = [Tool::Bash];

        let cfg = compaction::CompactionConfig::default();
        // (#457) Test relies on MaxTurns to terminate the loop — needs
        // Some(100) explicitly now that the cap is operator-opt-in.
        let _outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("loop completes (MaxTurns)");

        // Read the trajectory and find the failure-cascade event.
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory file must exist");
        let failure_events: Vec<_> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.tool.repeated_failure")
            .collect();
        assert!(
            !failure_events.is_empty(),
            "expected at least one dispatch.tool.repeated_failure event"
        );
        let first = &failure_events[0];
        assert_eq!(first["tool_name"], "bash");
        assert_eq!(first["consecutive_failures"], 3);
        // Edge-triggered: even though the loop runs 100 turns of
        // failures, we should see exactly one cascade event for the
        // single uninterrupted streak.
        assert_eq!(
            failure_events.len(), 1,
            "edge-triggered detector must emit one event per cascade, not per failed turn"
        );
    }

    /// (#418) Mock always returns the same `read` tool call with the
    /// same path; loop dispatches; cycle detector should fire a
    /// `dispatch.cycle.suspected` event into the trajectory after
    /// the third occurrence in the default window. Edge-triggered:
    /// later calls in the same dispatch do NOT add more events
    /// (unless the hash drops out of the window and re-crosses).
    #[test]
    #[serial_test::serial]
    fn loop_emits_cycle_suspected_event_after_third_identical_tool_call() {
        let server = MockServer::start();
        // Mock returns the SAME read call every time. Loop will keep
        // dispatching (`tool_calls` finish_reason) until MAX_TURNS.
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_loop",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("cycle-detect").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // Let the loop run to MAX_TURNS — the cycle should fire well
        // before. (#457) Cap is operator-opt-in now; pass Some(100)
        // explicitly so the loop terminates at the same point this
        // test was originally written against.
        let _outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("loop completes (MaxTurns)");

        // Read the trajectory and count cycle.suspected events.
        // Trajectory::open writes under `<dir>/.darkmux-runtime/trajectory.jsonl`.
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory file must exist");
        let cycle_events: Vec<_> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.cycle.suspected")
            .collect();
        assert!(
            !cycle_events.is_empty(),
            "expected at least one dispatch.cycle.suspected event in trajectory"
        );
        // First event should have count==3 (default warn threshold)
        let first = &cycle_events[0];
        assert_eq!(first["tool_name"], "read");
        assert_eq!(first["count"], 3);
        assert!(first["canonical_args"].as_str().unwrap().contains("x.txt"));
    }

    /// (Feedback injection scaffold — Step 1) End-to-end test that the
    /// `FeedbackInjector` actually delivers messages into the
    /// conversation, not just into a side queue. Drives the loop with
    /// a cycle-inducing mock and asserts BOTH:
    ///   1. `dispatch.feedback.injected` events land in the trajectory
    ///      (the observability path is wired)
    ///   2. The final `LoopOutcome.messages` contains at least one
    ///      `[darkmux-runtime]`-prefixed system message naming the
    ///      cycle (the model-facing path is wired)
    ///
    /// The code-reviewer for this PR flagged that the unit tests in
    /// `feedback.rs` exercise the primitive in isolation but the
    /// `loop_runner.rs` integration (drain → `messages.extend()`) was
    /// uncovered. Catches any future refactor that drops the
    /// `messages.extend(pending_feedback)` call.
    #[test]
    #[serial_test::serial]
    fn feedback_injection_delivers_to_conversation_when_cycle_fires() {
        // Ensure feedback injection is enabled for this test (not
        // disabled by a prior test's env mutation that didn't unset).
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");

        let server = MockServer::start();
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_loop",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("feedback-injection").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // (#457) Same MaxTurns-relying pattern as the cycle/cascade
        // tests above; needs Some(100) now that the cap is opt-in.
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("loop completes (MaxTurns)");

        // (1) Trajectory contains feedback.injected events — proves
        // the drain ran and recorded its delivery.
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory file must exist");
        let injected_events: Vec<_> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.feedback.injected")
            .collect();
        assert!(
            !injected_events.is_empty(),
            "expected at least one dispatch.feedback.injected event in trajectory \
             — the drain path must run when cycle signals fire"
        );
        let first = &injected_events[0];
        assert!(
            first["message_count"].as_u64().unwrap_or(0) >= 1,
            "feedback.injected event must report message_count >= 1"
        );
        // (#457 Step 3) Per-signal discrimination replaces Step 1's
        // combined `cycle_or_cascade` bucket. The mock fires cycles
        // (same read call repeatedly), so the kinds should include
        // `cycle_suspected`.
        let kinds = first["signal_kinds"]
            .as_array()
            .expect("signal_kinds is an array");
        assert!(
            kinds.iter().any(|k| k == "cycle_suspected"),
            "feedback.injected trajectory event must carry per-signal kinds; \
             expected `cycle_suspected` to be present, got: {kinds:?}"
        );

        // (2) The conversation contains the synthetic system message
        // — proves `messages.extend(pending_feedback)` is wired.
        let runtime_system_msgs: Vec<_> = outcome
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .filter_map(|m| m.content.as_deref())
            .filter(|c| c.starts_with("[darkmux-runtime]"))
            .collect();
        assert!(
            !runtime_system_msgs.is_empty(),
            "expected at least one [darkmux-runtime]-prefixed system message \
             in the final conversation — the cycle warning must reach the model"
        );
        // At least one should name the tool that cycled.
        assert!(
            runtime_system_msgs.iter().any(|c| c.contains("`read`")),
            "at least one runtime system message should name the cycling tool: \
             saw {:?}",
            runtime_system_msgs
        );
    }

    /// (#406) The 20% silent-bail scenario: model returned
    /// `finish_reason=stop` with `content` containing an XML-format
    /// tool call but EMPTY `tool_calls` field. The promoter must
    /// recover the call from content, flip finish_reason to
    /// `tool_calls`, and the loop must continue (NOT exit after one
    /// turn). Asserts:
    ///   - outcome.turns > 1 (the bail was promoted, not exited)
    ///   - terminal_reason is MaxTurns (mock keeps returning bail
    ///     shape; we run out the clock — that's fine, what matters
    ///     is the first turn didn't terminate as Stop)
    ///
    /// Before #406 this test would assert turns==1 + Stop, which is
    /// the silent-bail behavior that compounded across multi-dispatch
    /// dogfood to 67% chance of seeing at least one bail per
    /// five-dispatch workflow.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_in_content_when_finish_reason_is_stop() {
        let server = MockServer::start();
        // Every call returns the bail shape: finish=stop, content has
        // an XML tool_call, tool_calls field is null. Without the
        // promoter, loop exits at turn 1. With the promoter, loop
        // promotes the call, dispatches `read` (will fail on missing
        // /workspace/x.txt — that's fine, a failed tool dispatch is
        // still a successful loop iteration), and loops back.
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(
                    "Let me read the file:\n\
                    <tool_call>\
                    <function=read>\
                    <parameter=path>/workspace/x.txt</parameter>\
                    <parameter=offset>1</parameter>\
                    <parameter=limit>50</parameter>\
                    </function>\
                    </tool_call>",
                ),
                None,
                "stop",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("promoted XML tool call should drive the loop, not error");

        assert!(
            outcome.turns > 1,
            "promotion must continue the loop past turn 1; got turns={} (pre-#406 silent bail at turn 1)",
            outcome.turns
        );
        // The mock keeps returning the bail shape, so the loop runs
        // until MAX_TURNS. That's the right outcome for this synthetic
        // test — the load-bearing assertion is the turns>1 above.
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "expected MaxTurns after the promoter kept the loop alive past MAX_TURNS"
        );
    }

    // ─── (#479) per-turn-cap-approach tool-call salvage ─────────────

    /// Helper-level: assistant_message_has_well_formed_tool_calls returns
    /// true on a message with a single tool call having valid JSON args.
    #[test]
    fn salvage_helper_true_on_well_formed_tool_call() {
        let msg = Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![crate::lmstudio::ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::lmstudio::FunctionCall {
                    name: "read".to_string(),
                    arguments: r#"{"path":"/workspace/x.txt"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        assert!(assistant_message_has_well_formed_tool_calls(&msg));
    }

    /// Helper-level: returns false on a message with malformed args
    /// (incomplete JSON — the partial-truncation case the salvage
    /// path must NOT engage on).
    #[test]
    fn salvage_helper_false_on_malformed_tool_call() {
        let msg = Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![crate::lmstudio::ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::lmstudio::FunctionCall {
                    name: "read".to_string(),
                    arguments: "{partial".to_string(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        assert!(!assistant_message_has_well_formed_tool_calls(&msg));
    }

    /// Helper-level: returns false when tool_calls is empty / absent.
    #[test]
    fn salvage_helper_false_when_no_tool_calls() {
        let msg = Message {
            role: "assistant".to_string(),
            content: Some("just text".to_string()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        assert!(!assistant_message_has_well_formed_tool_calls(&msg));
    }

    /// Integration: model returns finish_reason=length with
    /// completion_tokens at the cap AND a well-formed tool call.
    /// Pre-#479 this bailed with an error. Post-#479 the tool call
    /// is salvaged, dispatched, and the loop continues.
    #[test]
    #[serial_test::serial]
    fn loop_salvages_tool_call_on_per_turn_cap_hit() {
        let server = MockServer::start();
        // First response: length-finish + valid tool call at the cap.
        // Second response: stop to terminate the loop cleanly.
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial truncated content"),
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("per-turn-cap-salvage").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        // Cap turns at 3 so the loop terminates if salvage works (it'll
        // run turn 1 → salvage dispatch → turn 2 → ... → MAX_TURNS).
        let outcome = run(
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            std::collections::BTreeMap::new(),
        )
        .expect(
            "per-turn-cap salvage should drive the loop, not return an Err — the runtime \
             must convert length+well-formed-tool-calls into tool dispatch (#479)",
        );

        assert!(
            outcome.turns >= 1,
            "salvage must let the loop continue past turn 1 (got turns={})",
            outcome.turns
        );
        // The trajectory should contain the per_turn_cap.salvaged event.
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let salvaged_seen = raw.lines().any(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
            v.get("type").and_then(|t| t.as_str())
                == Some("dispatch.per_turn_cap.salvaged")
        });
        assert!(
            salvaged_seen,
            "trajectory must record dispatch.per_turn_cap.salvaged when salvage fires"
        );
    }

    /// Salvage must still dispatch the tool call even when feedback
    /// injection is disabled. The nudge is a no-op but the salvage
    /// path itself stays active — separating queueing from routing.
    #[test]
    #[serial_test::serial]
    fn loop_salvages_tool_call_even_when_feedback_injection_disabled() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "0");
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial truncated content"),
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("salvage-feedback-disabled").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            std::collections::BTreeMap::new(),
        )
        .expect(
            "salvage routing must work independently of feedback queueing — \
             DARKMUX_FEEDBACK_INJECTION=0 disables the nudge, not the salvage",
        );

        assert!(
            outcome.turns >= 1,
            "salvage must still drive the loop when feedback is disabled (got turns={})",
            outcome.turns
        );
        // Trajectory event still fires — observability isn't gated on
        // the feedback-injection switch.
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let salvaged_seen = raw.lines().any(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
            v.get("type").and_then(|t| t.as_str())
                == Some("dispatch.per_turn_cap.salvaged")
        });
        assert!(salvaged_seen, "trajectory event must fire regardless of feedback gate");

        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
    }

    /// When salvage fires, the assistant message's truncated content
    /// must be cleared before push so the next turn's prompt doesn't
    /// carry the runaway reasoning forward (anchors the model on the
    /// failed pattern + inflates prompt_tokens). Mirrors the stall-
    /// arm's messages.pop() rationale.
    #[test]
    #[serial_test::serial]
    fn salvage_clears_truncated_content_from_history() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("31k chars of truncated reasoning would land here in the real case"),
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("salvage-clear-content").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(2),
            None,
            std::collections::BTreeMap::new(),
        )
        .expect("salvage should drive the loop");

        // Find the assistant message that landed during the salvaged
        // turn — it should have tool_calls present but content cleared
        // to None so the truncated noise doesn't anchor future turns.
        let salvaged_assistant_msg = outcome
            .messages
            .iter()
            .find(|m| m.role == "assistant" && m.tool_calls.is_some());
        let m = salvaged_assistant_msg
            .expect("salvaged turn's assistant message should be in history");
        assert!(
            m.content.is_none(),
            "salvage must clear assistant_message.content to prevent anchoring + bloat; got content={:?}",
            m.content
        );
        assert!(
            m.tool_calls.as_ref().map(|tcs| !tcs.is_empty()).unwrap_or(false),
            "salvage must preserve tool_calls for dispatch"
        );
    }

    /// Integration: model returns finish_reason=length with truncated
    /// tool-call args (malformed JSON). Salvage MUST NOT engage; the
    /// existing bail path catches the unsafe-salvage case.
    #[test]
    #[serial_test::serial]
    fn loop_does_not_salvage_on_malformed_tool_args() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial content"),
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspac",  // truncated mid-string
                    },
                }])),
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("per-turn-cap-no-salvage").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let result = run(
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            std::collections::BTreeMap::new(),
        );

        assert!(
            result.is_err(),
            "malformed JSON args must fall through to the existing bail; got Ok(...)"
        );
    }

    /// (#406) Promotion also recovers calls when `finish_reason` is
    /// `"length"`. Pre-fix the downstream match treated `"length"` as
    /// a hard context-overflow error and threw away the recovered
    /// call. Asserts the loop continues past turn 1 just like the
    /// finish_reason=stop case.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_when_finish_reason_is_length() {
        let server = MockServer::start();
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(
                    "<tool_call>\
                    <function=read>\
                    <parameter=path>/workspace/x.txt</parameter>\
                    <parameter=offset>1</parameter>\
                    <parameter=limit>50</parameter>\
                    </function>\
                    </tool_call>",
                ),
                None,
                "length", // Pre-fix: hard error. Post-fix: promotion flips to tool_calls.
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote-length").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("recovered call from length-truncated response should drive the loop");

        assert!(
            outcome.turns > 1,
            "promotion must continue the loop even when finish_reason=length; got turns={}",
            outcome.turns
        );
    }

    /// (#406) Reasoning-channel variant of the bail scenario: the XML
    /// tool call lands in `reasoning_content` rather than `content`
    /// (the Qwen 3.x thinking-mode case from V4 N=5 Run 2). The
    /// promoter must fall back from content to reasoning_content.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_in_reasoning_when_finish_reason_is_stop() {
        let server = MockServer::start();
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            // content is null; reasoning_content carries the call —
            // exactly the V4 N=5 Run 2 bail shape.
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "ignored-by-test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content":
                            "Now I should read the file:\n\
                            <tool_call>\
                            <function=read>\
                            <parameter=path>/workspace/x.txt</parameter>\
                            <parameter=offset>1</parameter>\
                            <parameter=limit>50</parameter>\
                            </function>\
                            </tool_call>",
                    },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110 },
            }));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote-reason").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("promoted XML tool call from reasoning_content should drive the loop");

        assert!(
            outcome.turns > 1,
            "reasoning-channel promotion must keep the loop alive past turn 1; got turns={}",
            outcome.turns
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::MaxTurns);
    }

    /// (#406 regression guard, Beat 47) The streaming path used to
    /// strip reasoning_content via `accumulator.take_reasoning_content`
    /// before building the response, enforcing the documented Message
    /// invariant ("outgoing request messages never emit
    /// reasoning_content"). PR #407 re-attached reasoning so the
    /// promoter could scan it; that re-attachment must be cleared
    /// BEFORE the response message gets pushed into conversation
    /// history. Otherwise the next turn's request carries the model's
    /// prior reasoning text — recursive feedback that caused 100-turn
    /// MAX_TURNS bails in attempt 2 of the validation.
    ///
    /// This test pins the invariant: an assistant message in the
    /// returned conversation MUST have reasoning_content=None,
    /// regardless of whether the model emitted reasoning.
    #[test]
    #[serial_test::serial]
    fn assistant_messages_in_history_never_carry_reasoning_content() {
        let server = MockServer::start();
        // First call: model emits reasoning + a structured tool call
        // (promotion does NOT fire — tool_calls field is populated).
        // The reasoning is set on the response; without the post-
        // promoter clear, it would leak into the next request.
        let _turn1 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"user\"");
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "Let me think about this and call a tool",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read",
                                "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}",
                            },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150 },
            }));
        });
        // Second call (after tool result): model finishes with stop.
        let _turn2 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"tool\"");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                200,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("reasoning-invariant").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("clean two-turn dispatch");

        // The first assistant message in the conversation must have
        // reasoning_content stripped — even though the model emitted
        // reasoning. The promoter scanned it; the conversation
        // history does not retain it.
        let assistant_msgs: Vec<&Message> = outcome
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .collect();
        assert!(
            !assistant_msgs.is_empty(),
            "expected at least one assistant message in history"
        );
        for (idx, m) in assistant_msgs.iter().enumerate() {
            assert!(
                m.reasoning_content.is_none(),
                "assistant message #{idx} in history must have reasoning_content=None \
                 (invariant: lmstudio.rs Message doc — request-side never emits it). \
                 Got: {:?}",
                m.reasoning_content
            );
        }
    }

    /// (#415) Every outgoing chat completion request must carry
    /// `max_tokens: Some(MAX_TOKENS_PER_CALL)` — the server-side
    /// cap that bounds runaway emission (including reasoning-channel
    /// emission, since LMStudio counts those tokens too). Asserts
    /// the request body contains the cap value.
    ///
    /// Regression guard: if a future change sets `max_tokens: None`
    /// on the agent-loop chat path, an unattended dispatch could
    /// stream tokens indefinitely until the 1500s dispatch deadline
    /// (#363) fires — the silent-runaway pattern Beat 47 run 3
    /// demonstrated empirically.
    #[test]
    #[serial_test::serial]
    fn loop_request_carries_max_tokens_cap() {
        let server = MockServer::start();
        // Captures the request body so the test can verify max_tokens.
        let captured = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"max_tokens\":10000");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("max-tokens-cap").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("clean single-turn dispatch");

        captured.assert();
        assert_eq!(outcome.turns, 1);
    }

    /// Smoke: mock returns finish_reason=stop on first call. Loop
    /// terminates cleanly, no compaction. Proves the mock + LmStudioClient
    /// + loop_runner integration plumbing works.
    #[test]
    #[serial_test::serial]
    fn loop_runs_against_mock_and_terminates_on_stop() {
        let server = MockServer::start();
        let stop_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                1234,
                10,
            ));
        });

        // LmStudioClient expects the base_url to include the /v1
        // prefix (matches the production default); httpmock's
        // server.base_url() is just the host:port. Compose the path
        // here so the mock's /v1/chat/completions matcher hits.
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-smoke").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![
            Message::system("you are a test assistant"),
            Message::user("hi"),
        ];
        let tools = [Tool::Read, Tool::Edit, Tool::Bash];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("loop should terminate cleanly on first-turn stop");

        stop_mock.assert();
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.compactions, 0);
        assert_eq!(outcome.total_prompt_tokens, 1234);
        // #325: pin the Stop terminal_reason on this clean-exit path.
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// The real signal: drive the loop into a compaction by escalating
    /// prompt_tokens past threshold. Mock sequence:
    ///   1. primary returns tool_calls + above-threshold prompt_tokens
    ///   2. (tools execute → messages grow past PRESERVE_HEAD+1+PRESERVE_TAIL=7)
    ///   3. needs_compaction fires → runtime calls compactor model
    ///   4. compactor returns summary
    ///   5. primary returns stop
    ///
    /// We pass an explicit `CompactionConfig { threshold_tokens: 1000,
    /// compactor_model: "test-compactor" }` so the mock doesn't have
    /// to fake huge prompt sizes. Distinguishes the compactor call
    /// from primary calls by inspecting the request's `model` field —
    /// they differ.
    ///
    /// Pre-#368 this test set/unset
    /// `DARKMUX_RUNTIME_COMPACT_THRESHOLD_TOKENS` env var with a 40-
    /// line EnvGuard for restore-on-drop and required serial
    /// execution. Post-#368 the runtime reads compaction config from
    /// explicit params (no env), so this is just a struct literal.
    ///
    /// Asserts:
    ///   - outcome.compactions == 1
    ///   - compactor mock was hit exactly once
    ///   - primary mock was hit at least twice (before + after compaction)
    #[test]
    fn loop_triggers_compaction_when_threshold_crossed() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = MockServer::start();

        // Primary model: every call returns tool_calls with above-
        // threshold prompt_tokens. The loop will keep calling until
        // MAX_TURNS, but we only need the FIRST primary call to fire
        // and the compactor to be invoked once before MAX_TURNS hits.
        // The mock-hits assertion below is what verifies the
        // layer-boundary signal — outcome itself is not needed here.
        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000, // above 1000-token threshold
                50,
            ));
        });

        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        // LmStudioClient expects the base_url to include the /v1
        // prefix (matches the production default); httpmock's
        // server.base_url() is just the host:port. Compose the path
        // here so the mock's /v1/chat/completions matcher hits.
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-fire").unwrap();
        // Pre-populate /workspace dir + the file the mock's tool_call
        // will try to read. Real dispatches mount /workspace as a
        // tempdir; here we just give read a target that resolves.
        std::fs::create_dir_all(tmp.path()).unwrap();
        // The runtime's `read` tool will validate paths under
        // /workspace; for the integration test we don't actually need
        // the tool to succeed — failed reads still append a `tool`
        // message and the loop continues. The key invariant: the
        // primary's escalated usage trips needs_compaction(...) on
        // the next iteration.

        let mut traj = Trajectory::open(tmp.path());
        // Pad initial messages so that after the first turn's
        // assistant-message + tool-result, we have >= 7 messages
        // (PRESERVE_HEAD=2 + 1 + PRESERVE_TAIL=4) — the second
        // condition for needs_compaction. Adding 5 extra user/assistant
        // pairs gets us there.
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        // Run. Expected to error eventually (mock loops forever on
        // tool_calls; will hit MAX_TURNS); we don't care about the
        // outcome's Ok/Err — just whether the compactor was invoked
        // along the way. The result IS the side-effect assertion below.
        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new());

        // Core assertion: compactor was invoked at least once. This is
        // the layer-boundary signal — the runtime's loop translated
        // a threshold-crossing into a compactor model call.
        assert!(
            compactor_mock.hits() >= 1,
            "compactor model was never invoked despite threshold being crossed; \
             compactor hits={}",
            compactor_mock.hits()
        );

        // QA FLAG 2 — also assert the runtime's own compactions counter
        // incremented. Catches the future-regression class where the
        // loop calls the compactor but forgets to bump the telemetry
        // (drift between observable side-effect and reported counter).
        // The loop hits MAX_TURNS so `outcome` is Err; we still want
        // to read its inner state. The Err path doesn't expose the
        // partial LoopOutcome, but the runtime emits compaction events
        // to trajectory which is the more durable signal anyway.
        // For now: if the loop ever returns Ok (would require the
        // mock to drive a stop after compaction), enforce counter
        // parity; otherwise rely on the mock-hit assertion above.
        if let Ok(o) = outcome {
            assert!(
                o.compactions >= 1,
                "runtime returned Ok but compactions counter is 0 \
                 despite mock recording {} compactor hit(s) — \
                 telemetry drift",
                compactor_mock.hits()
            );
        }
    }

    /// (#377) When `bail_after_compactions = N` is set and N
    /// compactions have fired, the loop must exit with
    /// `TerminalReason::EscalationTriggered(CompactionLimitReached)`
    /// rather than continuing to MAX_TURNS. Same mock setup as the
    /// preceding test except: bail=1 so the FIRST compaction trips
    /// the bound + the loop bails immediately after persisting the
    /// trajectory entry.
    ///
    /// This is the load-bearing chunk-3 invariant: the bound is
    /// observed, the salvageable state ships in LoopOutcome, and the
    /// terminal reason is the specific escalation variant (NOT a
    /// generic timeout or Err). Frontier handoff skill branches on
    /// the variant.
    #[test]
    fn loop_bails_with_escalation_when_compaction_limit_reached() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };

        let server = MockServer::start();

        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000,
                50,
            ));
        });

        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-bail").unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("bail should produce Ok with EscalationTriggered, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionLimitReached),
            "bail must produce the specific escalation variant, not a generic terminal"
        );
        assert_eq!(
            outcome.compactions, 1,
            "the bound-crossing compaction is counted"
        );
        assert_eq!(
            compactor_mock.hits(),
            1,
            "exactly one compactor call before the bail"
        );
        // Salvageable state: messages vec must be non-empty so the
        // frontier handoff can pick up where local-tier left off.
        assert!(
            !outcome.messages.is_empty(),
            "LoopOutcome.messages must carry salvageable state for frontier handoff"
        );
    }

    /// (#377) When `bail_after_compactions = None` is set (operator
    /// hasn't configured a bound), the loop must NOT bail — it
    /// continues through subsequent compactions as before. Catches
    /// the regression class where the bail check fires on the
    /// default None case.
    #[test]
    fn loop_does_not_bail_when_bail_after_compactions_is_none() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = MockServer::start();
        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000,
                50,
            ));
        });
        let _compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-no-bail").unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        // Loop hits MAX_TURNS (mock loops forever). The key
        // assertion: terminal_reason must be MaxTurns, NOT
        // EscalationTriggered, even though compactions fired.
        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("loop should hit MAX_TURNS, not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "with bail_after_compactions=None, MAX_TURNS is the only bound that fires"
        );
        assert!(
            outcome.compactions >= 1,
            "compaction still fires; bail just doesn't kick in"
        );
    }

    // ===== (#414 PR A) Length-finish stall recovery tests =====

    /// (#414 PR A) The Run 1 / Beat 47 shape: model returns
    /// `finish_reason=length` with NO content and NO tool_calls — pure
    /// reasoning hang. The loop must recover via nudge+retry instead
    /// of bailing. Mock uses two stages: stall on the FIRST request
    /// (the one with no nudge yet), then stop on the SECOND request
    /// (which carries the nudge). The state-discrimination relies on
    /// httpmock's `body_contains` against the nudge sentinel — the
    /// retried request will carry the nudge text in its messages
    /// payload; the first will not.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_from_length_stall_when_content_empty_and_no_tool_calls() {
        let server = MockServer::start();
        // First call: no nudge in payload → stall response.
        let _stall = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let body = req.body.as_deref().and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
                    !body.contains("darkmux-runtime] Your previous response")
                });
            then.status(200).json_body(chat_response_json(
                None,                       // content = null
                None,                       // no tool_calls
                "length",                   // per-call cap fired
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });
        // Second call: nudge present in payload → clean stop.
        let _stop = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("darkmux-runtime] Your previous response");
            then.status(200).json_body(chat_response_json(
                Some("answered after the nudge"),
                None,
                "stop",
                150,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("stall-recover").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("answer the question")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("stall recovery should drive the loop to Stop, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::Stop,
            "post-nudge turn produced clean stop"
        );
        assert!(
            outcome.turns >= 2,
            "expected at least 2 turns (stall + recovery); got {}",
            outcome.turns
        );
        // The useless turn must have been popped from history — only
        // the post-nudge assistant message survives.
        let assistant_msgs: Vec<&Message> = outcome
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .collect();
        assert_eq!(
            assistant_msgs.len(),
            1,
            "stalled turn must be popped from history; got {} assistant msgs",
            assistant_msgs.len()
        );
        assert_eq!(
            assistant_msgs[0].content.as_deref(),
            Some("answered after the nudge"),
            "the surviving assistant message is the post-recovery one"
        );
        // The nudge system message must appear in the conversation
        // (it was injected by the recovery branch).
        let nudge_present = outcome
            .messages
            .iter()
            .any(|m| m.role == "system" && m.content.as_deref().map(|c| c.contains("[darkmux-runtime]")).unwrap_or(false));
        assert!(nudge_present, "nudge system message must be present in final conversation");
    }

    /// (#414 PR A) Negative case — the OTHER length shape: model
    /// returned content (a partial answer) along with
    /// `finish_reason=length`. This is genuine truncation, NOT a
    /// runaway-reasoning stall; salvage isn't safe (the partial reply
    /// may mislead). The loop must still bail in this case.
    #[test]
    #[serial_test::serial]
    fn loop_bails_on_length_when_content_is_present() {
        let server = MockServer::start();
        let _truncated = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("here is half my answer before I got cut o"),  // real partial content
                None,
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("length-truncated").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("verbose answer")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let result = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new());

        assert!(
            result.is_err(),
            "finish_reason=length with content present must bail (truncated answer, not stall)"
        );
    }

    /// (#414 PR A) Coverage for the `tool_calls: []` empty-array
    /// shape (distinct from `tool_calls: null`/absent). Some OpenAI-
    /// compatible servers serialize the absent case as an empty
    /// array; the recovery detection should treat both identically.
    /// Also pins that content-PRESENT + tool_calls-EMPTY-ARRAY still
    /// routes to the hard-error branch (it's truncated content, not
    /// a stall).
    #[test]
    #[serial_test::serial]
    fn loop_bails_on_length_with_content_and_empty_tool_calls_array() {
        let server = MockServer::start();
        let _truncated = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("half answer before"),
                Some(serde_json::json!([])),
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("length-empty-tc-array").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let result = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new());

        assert!(
            result.is_err(),
            "length + content + empty-array tool_calls must bail (truncated answer shape)"
        );
    }

    /// (#414 PR A) Coverage for the `tool_calls: []` empty-array
    /// shape WITHOUT content. The runaway-reasoning detection should
    /// treat `tool_calls: []` identically to `tool_calls: null` and
    /// recover via nudge+retry just like the null-tool_calls case.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_from_length_stall_when_tool_calls_is_empty_array() {
        let server = MockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let body = req.body.as_deref().and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
                    !body.contains("darkmux-runtime] Your previous response")
                });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([])), // empty array, not null
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });
        let _stop = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("darkmux-runtime] Your previous response");
            then.status(200).json_body(chat_response_json(
                Some("answered after nudge"),
                None,
                "stop",
                150,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("stall-recover-empty-array").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("recovery should drive the loop to Stop");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// (#414 PR A) When the model stalls more times than
    /// [`MAX_STALL_RECOVERIES`] tolerates, the dispatch escalates via
    /// `EscalationTriggered(IntraTurnStallExhausted)` instead of
    /// burning more turns or returning Err. Asserts the escalation
    /// path delivers a salvageable outcome (consistent with the other
    /// EscalationReason cases).
    #[test]
    #[serial_test::serial]
    fn loop_escalates_when_stall_recovery_budget_exhausted() {
        let server = MockServer::start();
        // Every call returns the stall shape: length + no content +
        // no tool_calls. The loop will recover twice (consuming the
        // budget), then escalate on the third stall.
        let _stall = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                None,
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("stall-budget-exhaust").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("budget exhaustion returns Ok(EscalationTriggered)");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "expected IntraTurnStallExhausted escalation, got {:?}",
            outcome.terminal_reason
        );
        // The 3rd stall is what trips escalation: recoveries 1 and 2
        // already ran the loop back through chat(); the 3rd sees the
        // budget exhausted and escalates.
        assert_eq!(
            outcome.turns, MAX_STALL_RECOVERIES + 1,
            "expected exactly MAX_STALL_RECOVERIES+1 turns (=={}); got {}",
            MAX_STALL_RECOVERIES + 1,
            outcome.turns
        );
    }

    /// (#414 PR A) The stall-recovery trajectory event must fire each
    /// time the recovery branch runs, recording the per-turn
    /// completion-token count and the budget consumption. Operators
    /// watching `dispatch.intra_turn_stall.recovered` events get a
    /// direct rate signal alongside the existing `tool_call.promoted`
    /// rate.
    #[test]
    #[serial_test::serial]
    fn loop_emits_intra_turn_stall_recovered_trajectory_event() {
        let server = MockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let body = req.body.as_deref().and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
                    !body.contains("darkmux-runtime] Your previous response")
                });
            then.status(200).json_body(chat_response_json(
                None,
                None,
                "length",
                100,
                MAX_TOKENS_PER_CALL,
            ));
        });
        let _stop = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("darkmux-runtime] Your previous response");
            then.status(200).json_body(chat_response_json(
                Some("recovered"),
                None,
                "stop",
                150,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("stall-traj").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let _outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, std::collections::BTreeMap::new())
            .expect("recovery succeeds");

        // Read the trajectory JSONL and assert the event landed.
        // (Trajectory::open creates `.darkmux-runtime/trajectory.jsonl`
        // under the given root, so we mirror that path here.)
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let mut found_recovered = false;
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            if v.get("type").and_then(|t| t.as_str()) == Some("dispatch.intra_turn_stall.recovered") {
                found_recovered = true;
                assert_eq!(
                    v.get("completion_tokens").and_then(|x| x.as_u64()),
                    Some(MAX_TOKENS_PER_CALL as u64),
                    "completion_tokens must equal per-call cap on the runaway turn"
                );
                assert_eq!(
                    v.get("recoveries_used").and_then(|x| x.as_u64()),
                    Some(1),
                    "first recovery records recoveries_used=1"
                );
                assert_eq!(
                    v.get("recoveries_budget").and_then(|x| x.as_u64()),
                    Some(MAX_STALL_RECOVERIES as u64),
                );
            }
        }
        assert!(
            found_recovered,
            "trajectory must contain dispatch.intra_turn_stall.recovered event"
        );
    }

    // ─── (#465) extract_edit_target_path — same-file detector helper ──

    #[test]
    fn extract_edit_target_path_pulls_path_from_edit_args() {
        let args = r#"{"path":"/workspace/src/lib.rs","edits":[{"old_string":"a","new_string":"b"}]}"#;
        assert_eq!(
            extract_edit_target_path(args).as_deref(),
            Some("/workspace/src/lib.rs")
        );
    }

    #[test]
    fn extract_edit_target_path_pulls_path_from_write_args() {
        let args = r#"{"path":"/workspace/foo.md","content":"hello"}"#;
        assert_eq!(
            extract_edit_target_path(args).as_deref(),
            Some("/workspace/foo.md")
        );
    }

    #[test]
    fn extract_edit_target_path_returns_none_on_malformed_json() {
        // Malformed JSON degrades safely to None. The state machine (#472)
        // treats a None target as a no-op — it HOLDS the in-progress
        // same-file counter rather than resetting it, so a transient
        // malformed-args edit can't erase an in-progress drift run. Only a
        // real bash verification clears the slate.
        assert_eq!(extract_edit_target_path("{not valid json"), None);
    }

    // ─── (#471) path normalization in the same-file detector ─────────

    #[test]
    fn extract_edit_target_path_normalizes_current_dir_prefix() {
        let with = extract_edit_target_path(r#"{"path":"./src/lib.rs"}"#);
        let without = extract_edit_target_path(r#"{"path":"src/lib.rs"}"#);
        assert_eq!(with, without, "./src/lib.rs must equal src/lib.rs (#471)");
    }

    #[test]
    fn extract_edit_target_path_normalizes_trailing_slash() {
        let with = extract_edit_target_path(r#"{"path":"src/lib.rs/"}"#);
        let without = extract_edit_target_path(r#"{"path":"src/lib.rs"}"#);
        assert_eq!(with, without, "trailing slash must not distinguish (#471)");
    }

    #[test]
    fn extract_edit_target_path_normalizes_parent_dir_traversal() {
        let with = extract_edit_target_path(r#"{"path":"src/../src/lib.rs"}"#);
        let without = extract_edit_target_path(r#"{"path":"src/lib.rs"}"#);
        assert_eq!(with, without, "src/../src/lib.rs must equal src/lib.rs (#471)");
    }

    #[test]
    fn normalize_path_lexical_preserves_leading_parent_dir() {
        // No preceding component to fold against — keep the `..`.
        assert_eq!(normalize_path_lexical("../foo.rs"), "../foo.rs");
    }

    // ─── (#474) inactivity soft-threshold floor + headroom ───────────

    #[test]
    fn soft_threshold_default_budget_is_linear_75pct() {
        assert_eq!(inactivity_soft_threshold_secs(600), 450);
    }

    #[test]
    fn soft_threshold_never_zero_for_tiny_budget() {
        assert!(inactivity_soft_threshold_secs(1) >= 1, "must never fire on iteration 1");
    }

    #[test]
    fn soft_threshold_small_budgets_keep_some_headroom() {
        // Below the headroom floor, soft must still be < budget so a
        // warning precedes the hard kill.
        assert_eq!(inactivity_soft_threshold_secs(10), 7);
        assert_eq!(inactivity_soft_threshold_secs(30), 22);
        for b in [2u64, 5, 10, 30] {
            assert!(inactivity_soft_threshold_secs(b) < b, "budget {b}: soft must be < budget");
        }
    }

    #[test]
    fn soft_threshold_enforces_min_headroom_above_floor() {
        // budget=100: linear would be 75 (25s headroom); capped to leave
        // the 30s minimum headroom → 70.
        assert_eq!(inactivity_soft_threshold_secs(100), 70);
    }

    #[test]
    fn extract_edit_target_path_returns_none_when_path_missing() {
        // Path is the discriminator; a tool call without one cannot
        // contribute to same-file repetition detection.
        let args = r#"{"edits":[{"old_string":"a","new_string":"b"}]}"#;
        assert_eq!(extract_edit_target_path(args), None);
    }

    #[test]
    fn extract_edit_target_path_returns_none_when_path_is_not_string() {
        // Defensive: model emits {"path": 123}. Don't panic; treat
        // as malformed.
        let args = r#"{"path":123,"content":"x"}"#;
        assert_eq!(extract_edit_target_path(args), None);
    }
}
