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
///
/// (#1221) This is now the DEFAULT, overridable per dispatch via the
/// `--max-tokens-per-call` runtime flag (host tier:
/// `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL` env > `runtime.max_tokens_per_call`
/// config). The 10000 was calibrated on NON-reasoning models; on
/// thinking-family models it truncates PRODUCTIVE reasoning (a capped turn's
/// reasoning is discarded entirely), so reasoning-heavy dispatches raise it
/// explicitly. No fixed number wins both ways — content-based stopping is
/// the tracked real fix; this knob is the near-term control.
/// (#1221) The per-call bound for ANSWER output — the model's committed text,
/// not its scratch work.
///
/// Large on purpose. Chopping a long answer buys no degeneracy signal (an
/// answer is not a thought) and every continuation re-sends the accumulation,
/// so the cost of a small value here is quadratic in the number of chops.
///
/// The reasoning check-in rate is the SEPARATE constant below. These were one
/// number until they were split, and the split is what this doc block is
/// distinguishing: the text that used to sit here described the interval while
/// being attached to this constant, which is how the two got conflated in the
/// first place.
///
/// Override: `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL` env >
/// `runtime.max_tokens_per_call` config > this default.
const MAX_TOKENS_PER_CALL: u32 = 10_000;

/// (#1221) How far the model reasons between check-ins — a SAMPLING RATE, not a
/// bound on thinking. A turn may span any number of these.
///
/// Deliberately separate from `MAX_TOKENS_PER_CALL` because the two want
/// opposite values and were briefly the same number, which is a bug waiting to
/// happen in both directions:
///
/// - Sampling a THOUGHT wants SMALL. It catches a loop early, and continuing
///   costs nothing the model can perceive.
/// - Bounding an ANSWER wants LARGE. Chopping a 4000-token findings JSON into
///   four calls buys no degeneracy signal — an answer is not a thought — and
///   each continuation re-sends the whole accumulation, so the cost is
///   quadratic in the number of chops.
///
/// One number could only ever be wrong for one of them. The loop picks per
/// call, by which region the turn is in.
const REASONING_CHECKPOINT_INTERVAL: u32 = 1000;

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

/// (#854) How many consecutive turns of an IDENTICAL `usage.prompt_tokens`
/// (while the message thread keeps growing) flags the endpoint's reported
/// context count as stale. A healthy, growing conversation strictly increases
/// prompt_tokens every turn (each turn appends the assistant message + tool
/// results to the next prompt), so a value frozen for several turns is an
/// endpoint misreport — observed on a turboquant MLX build, where the count
/// stuck at 48109 for 8+ turns and silently suppressed compaction into a
/// degenerate cycle. Set conservatively (4 identical reports) so a single
/// coincidental repeat never trips it.
///
/// Assumes the endpoint reports EXACT prompt-token counts (the local LMStudio /
/// Ollama / llama.cpp path does). On an endpoint that ROUNDS/BUCKETS the count,
/// a slowly-growing thread can sit at the same bucket for several turns and trip
/// this — but the substitution below is `estimate.max(reported)`, so it can only
/// ever make compaction fire EARLIER, never suppress one (it cannot reintroduce
/// the #854 cycle); the worst case on such an endpoint is a marginally-early
/// compaction. The substitute estimator inherits the runtime's chars/4
/// (~4-chars-per-token) proxy, so on pathologically token-dense content
/// (CJK / base64 / minified) it can under-fire — still strictly better than the
/// status quo, where compaction never fired at all. A token-dense-aware divisor
/// for this path specifically would be a separate refinement if under-firing
/// surfaces on real workloads.
const STALE_PROMPT_TOKENS_TURNS: u32 = 3;

/// (#854) Update the consecutive-frozen-turns counter for the endpoint's
/// reported prompt-token count. Incremented when `current` equals the previous
/// turn's value (frozen); reset to 0 on any change — growth is healthy, and a
/// drop is the legitimate post-compaction shrink. Pure, for testability.
fn update_frozen_prompt_turns(prev: Option<u32>, current: u32, frozen: u32) -> u32 {
    match prev {
        Some(p) if current == p => frozen.saturating_add(1),
        _ => 0,
    }
}

/// (#1221) The honest fallback when there is no reasoning to hand back — an
/// empty capture, or a model family whose thinking delimiters this runtime
/// cannot rebuild. It says so plainly: a model told nothing will assume it
/// still has context it does not have, and re-deriving briefly beats answering
/// from a gap it cannot see.
const CAP_NO_REASONING_MESSAGE: &str = "[darkmux-runtime] You reached the \
per-call token budget and generation stopped. Your reasoning from that turn \
could NOT be retained, so do not assume it is available — re-derive what you \
need, briefly, then give a tool call or a final answer.";

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

/// (#799) A bash tool invocation that **failed to run** — never executed —
/// rather than running and returning a non-zero exit. Stamped onto the
/// dispatch envelope so a SIGNOFF claiming a verifier passed can be
/// mechanically contradicted (the gate cross-checks the claim against this).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedExec {
    /// The command the model asked to run (from the bash tool args).
    pub command: String,
    /// Why it's classified as failed-to-run (e.g. "command not found (exit 127)").
    pub reason: String,
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

    /// (#799) Bash invocations that FAILED TO RUN (never executed) during the
    /// dispatch — the verifier-fabrication backstop. Empty on an honest run.
    pub failed_to_run: Vec<FailedExec>,
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
    // (#1187 audit finding) ALWAYS a local-LMStudio client, never the remote
    // brain — even when `client` is configured with a `chat_url`/auth header
    // override for a remote endpoint. `compaction_cfg.compactor_model` is
    // always a local utility-model id (never a remote deployment name), so
    // routing a compaction request through a remote-configured client either
    // silently burns the remote endpoint's budget on the wrong model (Azure,
    // which ignores the body's `model` field — the deployment is in the URL)
    // or 404s and fails the WHOLE dispatch (OpenAI-style endpoints, which
    // validate `model` server-side) — and both fire on exactly the long,
    // tool-heavy dispatch this feature exists for, not on a trivial smoke.
    compactor_client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
    streaming: bool,
    compaction_cfg: &compaction::CompactionConfig,
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u32>,
    // (#1221) Per-call completion-token bound for ANSWER output;
    // None = MAX_TOKENS_PER_CALL.
    max_tokens_per_call: Option<u32>,
    // (#1221) How far the model reasons between check-ins;
    // None = REASONING_CHECKPOINT_INTERVAL. A separate knob from the answer
    // bound above because the two want opposite values — see the constants.
    reasoning_checkpoint_interval: Option<u32>,
    feedback_templates: std::collections::BTreeMap<String, String>,
    // (#1038) Optional `response_format` envelope (the role's output_schema,
    // wrapped as json_schema). When set, every model turn is grammar-constrained
    // to that shape — local-model JSON malformation becomes impossible.
    response_format: Option<serde_json::Value>,
) -> Result<LoopOutcome> {
    let mut messages = initial_messages;
    // (#1221) Resolve the per-call cap once; every use below (the request's
    // max_tokens, cap-salvage detection, the budget snapshot, the length-arm
    // diagnostics) reads this so an override stays consistent end-to-end.
    // (#1221) The per-call cap is a CHECKPOINT INTERVAL, not a ceiling — it is
    // constant, and what changes at each checkpoint is whether the reasoning is
    // handed back open (continue) or closed (conclude).
    let answer_max_tokens: u32 = max_tokens_per_call.unwrap_or(MAX_TOKENS_PER_CALL);
    let reasoning_interval: u32 =
        reasoning_checkpoint_interval.unwrap_or(REASONING_CHECKPOINT_INTERVAL);
    // Which bound this turn's NEXT call carries. A turn starts in the reasoning
    // region (we cannot know whether it will think until it answers, and
    // sampling finely is the cheap mistake); it moves to the answer region once
    // a checkpoint shows the output is plain content, or once a degeneracy
    // verdict has closed the thought and what follows is the answer.
    let mut in_answer_region = false;
    let mut per_call_cap: u32 = reasoning_interval;
    let mut checkpoints_used: u32 = 0;
    // Set when the previous iteration handed a turn back as a prefill; read by
    // the turn counter so the resumed call is not counted as a new turn.
    let mut resuming_after_checkpoint = false;
    // (#1221) The whole thought so far for the turn currently in flight, and
    // where its prefill message sits in `messages`.
    //
    // Both exist because the first implementation had neither, and a live
    // 30-checkpoint dispatch showed what that costs: `carried` held only the
    // NEWEST slice, and the push placed it BESIDE the previous prefill instead
    // of replacing it. The outgoing request ended up carrying thirty sibling
    // assistant messages, each opening its own `<think>` and each holding a
    // truncated copy of the same answer. The model was not resuming a thought;
    // it restarted the same one every call, thirty times, and would have run to
    // context exhaustion without ever concluding.
    let mut turn_reasoning_accum = String::new();
    let mut checkpoint_prefill_at: Option<usize> = None;
    // (#1221) Whether this turn's `<think>` block has already been closed by a
    // degeneracy verdict. Once closed it STAYS closed: the model is writing its
    // answer after the thought, and re-opening a think block around that answer
    // would tell it the answer was a thought.
    let mut turn_think_closed = false;
    // (#1221) Whether the truncated region of THIS turn was inside a think
    // block at all. `max_tokens` bounds EVERY request, so a turn that writes a
    // long ANSWER and never reasons hits the interval exactly like a thinking
    // turn does — and wrapping that answer in `<think>` would hand the model
    // its own committed output back as scratch work, which is precisely the
    // category error the prefill design exists to avoid. Prefill continuation
    // is general; the think delimiters are not.
    let mut turn_is_reasoning = false;
    // True when the accumulated text already begins with its own `<think>`
    // (an inline-think model's raw content), so the prefill must not add one.
    let mut accum_carries_its_own_opener = false;
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
    // (#854) Endpoint stale-token detection state. `prev_prompt_tokens` is the
    // prior turn's reported count; `frozen_prompt_turns` counts consecutive
    // turns it hasn't changed. When it sticks (the count can't gate compaction),
    // the loop substitutes a local size estimate for the compaction decision.
    let mut prev_prompt_tokens: Option<u32> = None;
    let mut frozen_prompt_turns: u32 = 0;
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
    // repeated failures of one `(tool, args)` signature (e.g., agent
    // retrying gcc inside sandbox where it doesn't exist). Sibling to the cycle
    // detector; same MVP shape (warn-only).
    let mut failure_rate_detector = FailureRateDetector::new();
    // (#799) Accumulate bash invocations that FAILED TO RUN (never executed) —
    // stamped onto the outcome/envelope as the verifier-fabrication backstop.
    let mut failed_to_run: Vec<FailedExec> = Vec::new();
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
    // (#887) Verified: the host watchdog resets its deadline only on
    // proof-of-work (tool.completed + compaction), NOT on `model.partial`
    // SSE chunks — so a within-turn hang or a proof-of-work-silent turn is
    // bounded by the hard kill at 100%. A mid-stream SOFT nudge isn't
    // actionable (can't inject into an in-progress generation), so this
    // stays host-only by design.
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
                    failed_to_run: failed_to_run.clone(),
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
                    failed_to_run: failed_to_run.clone(),
                });
            }
        }

        // Pick the bound BEFORE building the request that carries it. This
        // sat after the struct literal, so every request went out with the
        // PREVIOUS iteration's value — the switch to the answer bound always
        // lagged one full call. Measured: reasoning=50 / answer=5000 produced
        // max_tokens 50, 50, 5000, 5000, so a non-reasoning turn was still
        // checkpointed at the small reasoning interval for one extra call,
        // which is exactly the case the split exists to prevent.
        per_call_cap = if in_answer_region {
            answer_max_tokens
        } else {
            reasoning_interval
        };

        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: Some(per_call_cap),
            response_format: response_format.clone(),
        };

        let next_seq = turns + 1;
        let mut response = if streaming {
            run_streaming_turn(client, &request, next_seq, trajectory)?
        } else {
            client.chat(&request)?
        };
        // (#1221) A checkpoint continuation is the SAME logical turn resuming,
        // so it must not consume a turn. It is a new API CALL, which is why
        // this used to increment — but `turns` is what `max_turns` is checked
        // against, so counting continuations silently divides the operator's
        // turn budget by the number of checkpoints: with a 1000-token interval,
        // one long reasoning turn spent thirteen of them. The knob would mean
        // something different depending on an unrelated interval setting.
        if resuming_after_checkpoint {
            resuming_after_checkpoint = false;
        } else {
            turns += 1;
            // (#1221) A new logical turn is a new thought. Drop the previous
            // turn's accumulation and forget where its prefill lived, so a
            // later checkpoint cannot splice two turns' reasoning together or
            // delete a message belonging to a finished turn.
            turn_reasoning_accum.clear();
            checkpoint_prefill_at = None;
            turn_think_closed = false;
            turn_is_reasoning = false;
            accum_carries_its_own_opener = false;
            in_answer_region = false;
        }

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
        //
        // (#1050) ...and on a terminal (no-tool-call) turn whose content is
        // empty, promote the reasoning into content FIRST — the qwen3_5-family
        // thinking models put their whole answer there. promote_terminal_reasoning
        // does the promotion (terminal turns only) and then performs the #406
        // strip, so the invariant above still holds for tool-call turns.
        if let Some(choice) = response.choices.first_mut() {
            let finish = choice.finish_reason.clone();
            promote_terminal_reasoning(&mut choice.message, &finish);
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
            // (#854) Track endpoint staleness BEFORE overwriting the running
            // value: a count identical to last turn (while the thread grew)
            // means the endpoint froze it. Deliberately inside the `Some(usage)`
            // arm: a usage-less turn (e.g. streaming without include_usage) is
            // BRIDGED — it neither increments nor resets the counter, so it
            // can't corrupt the run of identical reports. Don't "fix" this into
            // an unconditional reset; that would zero the counter on every
            // usage-less turn and defeat the detector.
            frozen_prompt_turns =
                update_frozen_prompt_turns(prev_prompt_tokens, usage.prompt_tokens, frozen_prompt_turns);
            prev_prompt_tokens = Some(usage.prompt_tokens);
            latest_prompt_tokens = usage.prompt_tokens;
            // (#557 Slice-3) Per-turn context-window occupancy sawtooth.
            // Emitted ONCE per turn, only when a real `usage` was seen
            // (so a no-usage turn doesn't write a stale/zero context).
            // `used` is the EXACT prompt-token count; `max` is the
            // configured n_ctx (None when unconfigured). Uses `turns`
            // as the seq — the same post-increment turn counter the
            // sibling trajectory events at this point use
            // (append_model_completed, append_tool_call_promoted). NO
            // rate-limiting: per-turn IS the correct sawtooth
            // granularity (unlike model.partial's per-SSE-chunk cadence).
            trajectory.append_context_window(
                turns,
                latest_prompt_tokens,
                compaction_cfg.context_window,
            );
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
        // "At the cap" is tolerance-matched, not equality-matched: LMStudio
        // reports cap-1 live (observed across four #1222 shakedowns:
        // 9999 @ cap 10000, 29999 @ cap 30000 — it stops before the token
        // that would exceed). An exact `== per_call_cap` never matches in
        // production, which silently killed both this salvage AND the
        // #1221 cliff recovery below on real dispatches.
        let at_cap = this_turn_completion_tokens
            .is_some_and(|t| t.saturating_add(1) >= per_call_cap);
        let salvaged_per_turn_cap = finish_reason == "length"
            && at_cap
            && assistant_message_has_well_formed_tool_calls(&assistant_message);
        if salvaged_per_turn_cap {
            let salvaged_count = count_well_formed_tool_calls(&assistant_message);
            let observed_tokens = this_turn_completion_tokens.unwrap_or(per_call_cap);
            eprintln!(
                "darkmux-runtime: ⚡ per-turn-cap salvage — completion_tokens=\
                 {} hit cap {}; dispatching {} well-formed tool call(s) and \
                 nudging the model to reduce per-call reasoning.",
                observed_tokens, per_call_cap, salvaged_count
            );
            trajectory.append_per_turn_cap_salvaged(
                turns,
                observed_tokens,
                per_call_cap,
                salvaged_count,
            );
            feedback_injector
                .queue_per_turn_cap_approach(observed_tokens, per_call_cap);
            // Clear truncated content — keep tool_calls. Mirrors the
            // stall-arm's pop reason: anchoring + prompt-token bloat.
            assistant_message.content = None;
        }
        let effective_finish_reason = resolve_finish_reason(
            finish_reason.as_str(),
            assistant_message
                .tool_calls
                .as_ref()
                .is_some_and(|t| !t.is_empty()),
            salvaged_per_turn_cap,
        );

        // Append the assistant's message to the conversation before we
        // process its tool calls — that's the order the next request
        // needs to see things in. When salvage fired, the content
        // field was cleared above so the truncated reasoning doesn't
        // leak into history.
        // (#1221) A turn that CONCLUDES after checkpointing returns only the
        // SUFFIX — a prefill continuation carries just the new text, never the
        // prefix it continues. Pushing that suffix as a fresh message left the
        // accumulated body orphaned in the stale prefill one slot earlier, and
        // `main.rs` takes "the last assistant message" as the deliverable — so
        // the envelope, the JSON `content` and the operator-visible preview all
        // got the tail and nothing else.
        //
        // That is the MODAL path, not an edge case: most turns conclude rather
        // than degenerate, and the PR's own measurement is that 43-50% of
        // review-corpus turns hit the boundary. A feature built to stop
        // discarding work was discarding it again, one layer up.
        //
        // Only on a TERMINAL finish. A `length` finish is still mid-turn: the
        // checkpoint arm below pops this very message and rewrites the prefill,
        // so folding here would destroy the accumulation it depends on.
        if effective_finish_reason != "length" {
            if let Some(i) = checkpoint_prefill_at {
                let carried = messages
                    .get(i)
                    .and_then(|m| m.content.clone())
                    .unwrap_or_default();
                let mut merged = carried;
                merged.push_str(assistant_message.content.as_deref().unwrap_or(""));
                // Keep the ANSWER, not the thought. Nothing downstream strips
                // think blocks, so folding the raw accumulation in would put
                // scratch work into the committed deliverable — the same
                // category error this design exists to avoid.
                let close = crate::budget_request::THINK_CLOSE.trim();
                let answer = match merged.rfind(close) {
                    Some(at) => merged[at + close.len()..].trim_start().to_string(),
                    None => merged,
                };
                assistant_message.content = Some(answer);
                if i < messages.len() {
                    messages.remove(i);
                }
                checkpoint_prefill_at = None;
            }
        }

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
                    failed_to_run: failed_to_run.clone(),
                });
            }
            "tool_calls" => {
                let calls = assistant_message
                    .tool_calls
                    .clone()
                    .unwrap_or_default();

                if calls.is_empty() {
                    // (#1123) finish_reason=tool_calls but no tool_calls — an
                    // empty/useless completion (observed: a degraded run on
                    // devstral-24b returned a wholly empty message under this
                    // finish_reason — no content, no reasoning, no calls).
                    // Pre-#1123 this hard-killed the dispatch. It's the SAME
                    // useless-stall shape the length-arm recovers (#414,
                    // ~line 1150); route it through the same recovery — drop
                    // the useless turn + nudge + retry, bounded by the stall
                    // budget, escalating to the frontier when exhausted —
                    // instead of aborting on the first occurrence.
                    if stall_recoveries_used >= MAX_STALL_RECOVERIES {
                        eprintln!(
                            "darkmux-runtime: escalation_triggered — finish_reason=\
                             tool_calls with no tool_calls, and the intra-turn stall \
                             recovery budget ({MAX_STALL_RECOVERIES}) is exhausted; \
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
                            failed_to_run: failed_to_run.clone(),
                        });
                    }
                    messages.pop();
                    stall_recoveries_used = stall_recoveries_used.saturating_add(1);
                    trajectory.append_intra_turn_stall_recovered(
                        turns,
                        this_turn_completion_tokens,
                        stall_recoveries_used,
                        MAX_STALL_RECOVERIES,
                    );
                    messages.push(Message::system(STALL_NUDGE_MESSAGE));
                    eprintln!(
                        "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} \
                         returned finish_reason=tool_calls with no tool_calls (empty \
                         completion). Dropped the useless turn, injected a nudge; \
                         budget {stall_recoveries_used}/{MAX_STALL_RECOVERIES} used. (#1123)"
                    );
                    continue;
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
                        // (#1001) Capture the target file's content hash at
                        // firing time so the caution can be ranked down as
                        // stale once that file changes.
                        let code_hash = detector_code_hash(&canonical_args);
                        trajectory.append_cycle_suspected(
                            turns,
                            &tool_name,
                            &canonical_args,
                            code_hash.as_deref(),
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
                    // (#469) Classify success/failure with the same
                    // predicate the failure-rate detector uses, and record
                    // it on the trajectory event so the host watchdog can
                    // gate its deadline reset.
                    let tool_ok = !crate::failure_rate::is_failure_result(
                        &call.function.name,
                        &result,
                    );
                    // (#799) A verifier that never RAN (vs ran-and-failed) is the
                    // trust-critical class — stamp it so a SIGNOFF claiming it
                    // passed can be mechanically contradicted at the gate.
                    if let Some(reason) =
                        crate::failure_rate::classify_failed_to_run(&call.function.name, &result)
                    {
                        // Best-effort display text: the parsed `command` field,
                        // falling back to the raw args. The gate treats it as
                        // advisory (what the model asked to run), not a
                        // re-parseable command.
                        let command = serde_json::from_str::<serde_json::Value>(
                            &call.function.arguments,
                        )
                        .ok()
                        .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(str::to_string))
                        .unwrap_or_else(|| call.function.arguments.clone());
                        failed_to_run.push(FailedExec {
                            command,
                            reason: reason.to_string(),
                        });
                    }
                    trajectory.append_tool_completed(
                        turns,
                        tool_seq as u32,
                        &call.function.name,
                        &call.function.arguments,
                        result.len(),
                        tool_ok,
                    );
                    // (#466/#469) Proof-of-work signal for the inactivity-
                    // approach detector. Mirrors the host-side reset
                    // trigger so the runtime-side soft warning and the
                    // host-side hard kill share the same deadline
                    // semantics. Only a SUCCESSFUL tool call counts as
                    // proof-of-work (#469): a stream of failures must not
                    // keep the deadline alive — that's the fast-fail seam
                    // the cycle + failure-rate detectors also guard.
                    if tool_ok {
                        last_proof_of_work = std::time::Instant::now();
                        inactivity_soft_warning_fired_in_window = false;
                    }
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
                            let (new_last, new_count, fired_path) = cadence_drift_step(
                                path.as_deref(),
                                last_edited_path.take(),
                                consecutive_same_file_edits,
                                TEST_CADENCE_DRIFT_THRESHOLD,
                            );
                            last_edited_path = new_last;
                            consecutive_same_file_edits = new_count;
                            if let Some(fired_path) = fired_path {
                                eprintln!(
                                    "darkmux-runtime: ⚠ test-cadence drift — {} \
                                     consecutive edits to `{}` without a bash \
                                     verification call. Queueing feedback nudge.",
                                    TEST_CADENCE_DRIFT_THRESHOLD, fired_path
                                );
                                feedback_injector.queue_test_cadence_drift(
                                    TEST_CADENCE_DRIFT_THRESHOLD,
                                    &fired_path,
                                );
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
                    // classify. Edge-triggered: a signature's counter
                    // resets when that signature next succeeds, warn
                    // fires once per cascade.
                    if let Some(FailureCascadeSignal::Suspected {
                        tool_name,
                        failure_count,
                    }) = failure_rate_detector.record(
                        &call.function.name,
                        &call.function.arguments,
                        &result,
                    )
                    {
                        eprintln!(
                            "darkmux-runtime: ✕ tool-failure cascade — `{}` failed {} times \
                             since it last succeeded. The tool or its environment may need operator attention. \
                             Operator-visible only; no behavior change.",
                            tool_name, failure_count
                        );
                        // (#1001) Carry the failing tool's args so the host can
                        // derive the file the cascade is on, plus the file's
                        // firing-time hash for staleness. A non-file tool
                        // (e.g. `bash`) yields no path / no hash downstream.
                        let code_hash = detector_code_hash(&call.function.arguments);
                        trajectory.append_tool_repeated_failure(
                            turns,
                            &tool_name,
                            &call.function.arguments,
                            code_hash.as_deref(),
                            failure_count,
                        );
                        // Step 1 of feedback injection — see cycle-
                        // suspected callsite above for the rationale.
                        // `failure_count` is `u32` at the
                        // signal layer; cast to `usize` to match the
                        // injector's API (which uses `usize` for
                        // counter fields uniformly).
                        feedback_injector.queue_tool_failure_cascade(
                            &tool_name,
                            failure_count as usize,
                        );
                    }
                    messages.push(Message::tool_result(
                        call.id,
                        call.function.name,
                        result,
                    ));
                }

                // (#1391) Soft-trim OLD oversized tool-result bodies before the
                // compaction trigger is evaluated. This is a zero-model-call,
                // purely mechanical byte reclaim (head + tail kept, middle
                // elided behind a marker) that shrinks the transcript and pushes
                // the FIRST compaction out — on a tight window that can be one
                // fewer compaction per dispatch. The recent thread is protected
                // (see TOOL_RESULT_TRIM_PRESERVE_RECENT), so the model never
                // loses context it is actively reasoning over. Runs every turn;
                // idempotent on bodies already reclaimed.
                let trim_stats =
                    crate::tool_result_prune::soft_trim_old_tool_results(&mut messages);
                if trim_stats.results_trimmed > 0 {
                    eprintln!(
                        "darkmux-runtime: soft-trimmed {} old tool result(s), reclaiming {} bytes \
                         of transcript before the compaction check (#1391)",
                        trim_stats.results_trimmed, trim_stats.bytes_reclaimed
                    );
                }

                // (#854) When the endpoint's reported count is stale (frozen
                // across turns while the thread grew), it can't gate compaction
                // — it silently suppressed it into a degenerate cycle. Substitute
                // a local chars/4 size estimate as the EFFECTIVE occupancy and
                // let the SAME threshold decide: this changes nothing in normal
                // operation (frozen=0 → effective == reported), and even when
                // stale it only compacts if real occupancy actually warrants it
                // (no needless compaction if the conversation genuinely
                // plateaued). The endpoint misreport is surfaced regardless.
                let effective_prompt_tokens = if frozen_prompt_turns >= STALE_PROMPT_TOKENS_TURNS {
                    let (sys_chars, prompt_chars) = measure_request_context(&messages);
                    let estimate = ((sys_chars + prompt_chars) / 4) as u32;
                    // Emit the eureka signal once, at the staleness crossing,
                    // so a stuck-but-low count doesn't spam the trajectory.
                    if frozen_prompt_turns == STALE_PROMPT_TOKENS_TURNS {
                        eprintln!(
                            "darkmux-runtime: the endpoint's context token count has been frozen \
                             at {latest_prompt_tokens} for {frozen_prompt_turns} turns while the \
                             message thread grew — substituting a local estimate ({estimate}) for \
                             the compaction decision (the reported count can't gate it). (#854)"
                        );
                        trajectory.append_stale_context_tokens(
                            turns,
                            latest_prompt_tokens,
                            frozen_prompt_turns,
                            estimate,
                            messages.len(),
                        );
                    }
                    estimate.max(latest_prompt_tokens)
                } else {
                    latest_prompt_tokens
                };

                // Phase 6: check whether the most recent prompt's
                // token count crossed the compaction threshold, AND
                // whether the conversation is long enough to compact.
                // If so, compact BEFORE the next chat() call so the
                // next request sees a smaller message thread.
                if compaction::needs_compaction(
                    effective_prompt_tokens,
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
                    // `<RUNTIME_OUT_BASE>/.darkmux-runtime/compaction-<gen>.json`
                    // per #352 Step 5 "persistence falls out for free."
                    let summary_chars = match compaction_cfg.strategy {
                        compaction::CompactionStrategy::Narrative => compaction::compact(
                            compactor_client,
                            &mut messages,
                            compactions,
                            compaction_cfg,
                        )?,
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
                                max_tokens_per_call: per_call_cap,
                            };
                            let (parsed, summary_chars) = compaction::structured_compact(
                                compactor_client,
                                &mut messages,
                                compactions,
                                compaction_cfg,
                                Some(budget),
                            )?;
                            // Persist the JSON for downstream
                            // consumers (replay, methodology
                            // research, cross-phase memory). Best-
                            // effort: a write failure logs but does
                            // NOT fail the dispatch — observability,
                            // not correctness.
                            persist_structured_compaction_output(
                                &crate::trajectory::runtime_dir(),
                                compactions,
                                &parsed,
                            );
                            summary_chars
                        }
                    };
                    let after_count = messages.len();
                    // (#885) summary_chars now comes directly from the
                    // compaction fn — the inserted summary's true length —
                    // rather than guessing it from a fixed `messages` index.
                    // (#557 Slice-3) Token occupancy across the compaction
                    // drop. `tokens_before` is the EXACT prompt-token count
                    // that triggered this compaction (the prior turn's
                    // usage.prompt_tokens). `tokens_after` is a chars/4
                    // ESTIMATE of the now-compacted `messages` buffer — the
                    // runtime has no tokenizer, so we measure chars via the
                    // same helper the dispatch.start event uses and divide
                    // by 4. The EXACT post-compaction count lands on the
                    // next turn's `dispatch.context` `used`.
                    // (#854) `effective_prompt_tokens` == reported in normal
                    // operation, and the local estimate when the endpoint count
                    // was stale — so the event's before-size reflects occupancy
                    // rather than a frozen value. Note: in the stale case the
                    // estimate is measured AFTER this turn's pushes, so it's the
                    // NEXT prompt's occupancy (one turn ahead of what the frozen
                    // reported metric described), not a restatement of it.
                    let tokens_before = effective_prompt_tokens;
                    let (sys_chars, prompt_chars) = measure_request_context(&messages);
                    let tokens_after = ((sys_chars + prompt_chars) / 4) as u32;
                    trajectory.append_compaction(
                        compactions,
                        before_count,
                        after_count,
                        summary_chars,
                        tokens_before,
                        tokens_after,
                    );
                    // (#854) The thread just shrank, so the next report should
                    // move again — restart staleness tracking so a fresh freeze
                    // is detected cleanly and this episode isn't re-flagged.
                    frozen_prompt_turns = 0;
                    prev_prompt_tokens = None;

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
                                failed_to_run: failed_to_run.clone(),
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

                // (#1221) The cap-cliff: length-finish WITH partial content
                // (or malformed tool calls) at exactly the per-call cap.
                // Pre-fix this was a hard error that killed the WHOLE
                // dispatch — dialectic shakedown-2 (#1222): a prosecutor
                // burned the entire raised budget in one runaway turn and
                // the dispatch died, discarding seven prior productive
                // turns. A cap hit is recoverable exactly like the empty
                // stall: the truncated turn is noise — drop it, nudge, and
                // spend the same bounded recovery budget. Only a length-
                // finish BELOW the cap (context overflow: prompt_tokens
                // crossed the loaded window) stays a hard error, because
                // that's a config problem recovery cannot fix.
                // Tolerance-matched like the salvage arm: LMStudio reports
                // cap-1 live, so equality misses by one token and misroutes
                // a cap hit to the overflow hard error (run-4 killed a
                // 14-turn prosecution at 29999/30000 exactly this way).
                let cap_cliff = this_turn_completion_tokens
                    .is_some_and(|t| t.saturating_add(1) >= per_call_cap);
                if !is_useless_stall && !cap_cliff {
                    return Err(anyhow!(
                        "model returned finish_reason=length with partial content \
                         BELOW the per-call cap (completion_tokens {} < \
                         max_tokens_per_call {per_call_cap}) — context overflow: \
                         prompt_tokens crossed the model's loaded context window. \
                         Compaction may need a smaller threshold or a larger n_ctx.",
                        this_turn_completion_tokens
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "<unknown>".to_string())
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
                        failed_to_run: failed_to_run.clone(),
                    });
                }

                // (#1221) CHECKPOINT, not a cap. The model's own reasoning
                // goes back inside the think region so it RESUMES rather than
                // restarting — measured: a 40,608-char truncated turn prefilled
                // back resumed mid-sentence and produced 3,999 more tokens.
                //
                // The runtime decides, not the model. An earlier cut asked the
                // model to either conclude or request more budget; measured on
                // a real review, it FOLDED at the first checkpoint — producing
                // a four-point summary with zero findings where the same model
                // uninterrupted had produced a real one. A model invited to
                // stop will stop. So the check-in is silent: the harness reads
                // the slice, and the model never learns a boundary existed.
                //
                // The closing delimiter is the switch. Clean -> hand it back
                // OPEN and it keeps thinking. Degenerate or out of checkpoints
                // -> hand it back CLOSED and it concludes FROM that reasoning
                // rather than re-deriving it.
                //
                // Why not `content`: promoting reasoning there puts scratch
                // work where the model was trained to read its own committed
                // answer. Prefill keeps the tokens where they were generated.
                //
                // The prefill message must remain LAST — anything appended
                // after it ends the assistant turn and turns a continuation
                // back into a restart.
                // After a prefill the provider stops tagging the continued
                // thinking as reasoning — darkmux supplied the `<think>` opener
                // itself, so the model's output comes back as ordinary
                // `content`. Measured: 13 API calls produced exactly ONE
                // `model.reasoning` event. Judging only `reasoning_content`
                // therefore leaves the gate reading an EMPTY slice on every
                // checkpoint after the first — continuing not because it found
                // the reasoning clean but because it had nothing to look at.
                // Post-prefill, content IS the reasoning.
                let content_slice = assistant_message.content.as_deref().unwrap_or("");
                // An INLINE-think model (the qwen 3.x line) that is cut
                // mid-reasoning leaves an UNCLOSED `<think>`, and
                // `extract_think_blocks` deliberately bails on those — so
                // `per_turn_reasoning` is empty for exactly the shape this
                // feature exists to handle, and the turn would be misread as a
                // plain answer. Count the delimiters instead: more openers than
                // closers means the cut landed inside a thought.
                //
                // The content already carries its own `<think>`, so the
                // accumulation must NOT be prefixed with a second one.
                let opens = content_slice.matches(crate::budget_request::THINK_OPEN.trim()).count();
                let closes = content_slice.matches(crate::budget_request::THINK_CLOSE.trim()).count();
                let cut_inside_inline_think = opens > closes;
                if cut_inside_inline_think {
                    turn_is_reasoning = true;
                    accum_carries_its_own_opener = true;
                }
                let slice_owned = if cut_inside_inline_think {
                    content_slice.to_string()
                } else if per_turn_reasoning.trim().is_empty() {
                    content_slice.to_string()
                } else {
                    // The FIRST checkpoint of a turn is the only honest place to
                    // decide this: after a prefill the provider stops tagging
                    // continued thinking as reasoning (darkmux supplied the
                    // opener), so a later checkpoint of a genuinely reasoning
                    // turn looks exactly like a non-reasoning one.
                    turn_is_reasoning = true;
                    per_turn_reasoning.clone()
                };
                // The slice is appended with NO separator: the model was cut
                // mid-sentence and its next call continues from exactly that
                // character, so anything inserted here lands inside a word.
                if !turn_is_reasoning {
                    // No think block was ever opened, so this turn is writing an
                    // ANSWER and should not be sampled at the reasoning rate.
                    in_answer_region = true;
                }
                turn_reasoning_accum.push_str(&slice_owned);
                // What gets handed back is the WHOLE thought, never one slice.
                // This is also the scope the degeneracy gate needs: a model
                // re-treading ground from three checkpoints ago produces slices
                // that each look locally novel, so judging one slice in
                // isolation cannot see the cycle it exists to catch.
                let carried = turn_reasoning_accum.trim();
                if carried.is_empty() {
                    // Nothing to hand back — an empty completion at the
                    // boundary. This is the USELESS STALL the intra-turn
                    // recovery has always owned, and it must keep owning it:
                    // the checkpoint code replaced the old drop-and-nudge for
                    // every shape in this arm, which silently took the recovery
                    // away from the one shape that still needs it. The symptom
                    // was six tests going from a clean
                    // `IntraTurnStallExhausted` escalation to `MaxTurns`, with
                    // `dispatch.intra_turn_stall.recovered` never emitted.
                    //
                    // Checkpointing cannot help here — there is no reasoning to
                    // resume. Drop the turn, spend the budget, nudge.
                    messages.pop();
                    stall_recoveries_used = stall_recoveries_used.saturating_add(1);
                    trajectory.append_intra_turn_stall_recovered(
                        turns,
                        this_turn_completion_tokens,
                        stall_recoveries_used,
                        MAX_STALL_RECOVERIES,
                    );
                    messages.push(Message::system(STALL_NUDGE_MESSAGE));
                    eprintln!(
                        "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} hit                          the boundary with an EMPTY completion, so there is nothing to                          resume. Dropped the useless turn, injected a nudge; budget                          {stall_recoveries_used}/{MAX_STALL_RECOVERIES} used. (#1123/#1221)"
                    );
                } else {
                    checkpoints_used = checkpoints_used.saturating_add(1);
                    // Only judge while the thought is still open. After the
                    // close the accumulation is reasoning PLUS the answer being
                    // written, and its ratio stays low forever — judging it
                    // would re-fire the verdict on every checkpoint.
                    let tail_ratio = if turn_think_closed {
                        None
                    } else {
                        crate::reasoning_loop::tail_repetition_ratio(
                            carried,
                            crate::reasoning_loop::TAIL_WINDOW_TOKENS,
                            crate::reasoning_loop::tail_sample_tokens(reasoning_interval),
                        )
                    };
                    // Degeneracy DETECTION applies to any output; only the
                    // remedy differs. An earlier cut gated the detection itself
                    // on `turn_is_reasoning`, which left repeating plain content
                    // with no gate at all — it checkpointed forever, and the
                    // pre-existing intra-turn stall escalation that used to
                    // bound exactly that shape became unreachable.
                    let degenerate = !turn_think_closed
                        && crate::reasoning_loop::slice_is_degenerate(carried, reasoning_interval);
                    // (#1221) EVERY continuation is the same logical turn,
                    // including the one that follows a `conclude`.
                    //
                    // This read `!degenerate` first, and a live dispatch showed
                    // what that costs: a conclude reported itself as not
                    // resuming, so the next iteration ran the fresh-turn reset,
                    // wiped the accumulation, and the model regenerated the
                    // identical thought. The tail ratios of checkpoints 6-10
                    // reproduced 1-5 to four decimal places, and the run would
                    // have cycled until context exhaustion. A conclude changes
                    // the DELIMITER, never the turn.
                    resuming_after_checkpoint = true;
                    trajectory.append_checkpoint(
                        turns,
                        checkpoints_used,
                        this_turn_completion_tokens,
                        tail_ratio,
                        if degenerate { "conclude" } else { "continue" },
                    );
                    // (#1221) The gate does exactly ONE thing: decide whether
                    // this slice is repeating. It imposes no limit of its own —
                    // not a checkpoint count (a count times the interval is a
                    // token ceiling wearing a different name) and not a
                    // time-based wrap-up. Every stop that is not degeneracy
                    // belongs to the operator's existing, CONFIGURABLE e-stops:
                    // the inactivity budget, `runtime.max_tokens`,
                    // `runtime.max_turns`. A limit invented here would be one
                    // more ceiling hiding in the code, which is what this whole
                    // change exists to remove.
                    // A degenerate turn that never opened a thought has no
                    // delimiter to close, so it does not get a prefill at all —
                    // it goes back to the recovery path that already owns this
                    // shape.
                    let fall_through_to_recovery = degenerate && !turn_is_reasoning;
                    if fall_through_to_recovery {
                        // No think block, so nothing to close. This is the
                        // repeating-at-cap shape the intra-turn stall recovery
                        // already owns (drop + nudge, escalating when its
                        // budget is spent) — hand it back to that path rather
                        // than inventing a second remedy here.
                        eprintln!(
                            "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — output is \
                             repeating and this turn never opened a thought; falling through \
                             to intra-turn stall recovery. (#1221)"
                        );
                        resuming_after_checkpoint = false;
                        // Consume the recovery budget. Without this the
                        // escalation check at the top of this arm can never
                        // fire, because the checkpoint path replaced the only
                        // thing that used to spend it — so a model repeating at
                        // the boundary would loop until an e-stop instead of
                        // escalating cleanly for handoff.
                        stall_recoveries_used = stall_recoveries_used.saturating_add(1);
                        // Drop the repeating turn and start the next attempt
                        // from a clean accumulation.
                        messages.pop();
                        turn_reasoning_accum.clear();
                        checkpoint_prefill_at = None;
                    } else if degenerate {
                        eprintln!(
                            "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — the reasoning is \
                             repeating (degeneracy gate); closing the thought so the model \
                             answers from what it has. (#1221)"
                        );
                        // The close is written INTO the accumulation, so every
                        // later checkpoint keeps handing back a thought that is
                        // already closed followed by the answer so far.
                        turn_reasoning_accum.push_str(crate::budget_request::THINK_CLOSE);
                        turn_think_closed = true;
                        // Everything after the closing delimiter is the ANSWER.
                        in_answer_region = true;
                    } else if turn_think_closed {
                        eprintln!(
                            "darkmux-runtime: ⏵ checkpoint {checkpoints_used} — thought already \
                             closed; handing back the answer so far so the model finishes it. \
                             (#1221)"
                        );
                    } else {
                        eprintln!(
                            "darkmux-runtime: ⏵ checkpoint {checkpoints_used} — reasoning is not \
                             repeating; handing it back OPEN so the model continues. (#1221)"
                        );
                    }
                    // A reasoning turn resumes INSIDE its think block; a
                    // plain-answer turn resumes as itself, with no delimiters
                    // invented around it.
                    if !fall_through_to_recovery {
                    let body = if turn_is_reasoning && !accum_carries_its_own_opener {
                        crate::budget_request::continue_thinking_prefill(&turn_reasoning_accum)
                    } else {
                        turn_reasoning_accum.clone()
                    };
                    // Drop the truncated raw response...
                    messages.pop();
                    // ...and the prefill this turn put there last time, so the
                    // thread carries ONE growing assistant message rather than
                    // a chain of restarts. Guarded on length because the
                    // no-reasoning branch above can push a system message
                    // between the two.
                    if let Some(i) = checkpoint_prefill_at {
                        if i < messages.len() {
                            messages.remove(i);
                        }
                    }
                    messages.push(Message::assistant_prefill(body));
                    checkpoint_prefill_at = Some(messages.len() - 1);
                    }
                }
                let tokens_str = this_turn_completion_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                let shape = if is_useless_stall {
                    "reasoning-only up to the cap"
                } else {
                    "partial content truncated at the cap"
                };
                // The per-branch detail (extended / concluded / no-reasoning)
                // is printed above where the decision is made; this line is the
                // one-per-hit summary the operator scans for.
                eprintln!(
                    "darkmux-runtime: ⏸ per-call budget reached — turn {turns} \
                     emitted {tokens_str} completion tokens ({shape}); the turn's \
                     reasoning was NOT discarded. Recovery budget \
                     {stall_recoveries_used}/{MAX_STALL_RECOVERIES}. (#1221)"
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
/// (#1050) Reasoning-channel fallback for thinking models. On a TERMINAL turn
/// (no tool calls) with empty content, the qwen3_5-family models route their
/// entire answer into `reasoning_content` and leave `content` empty — so
/// `final_assistant` would come back empty (every dispatch to them yields
/// nothing). Promote the reasoning into `content` so the answer (e.g. the
/// pr-reviewer's grammar-constrained JSON, which lands there) isn't lost.
///
/// Then ALWAYS strip `reasoning_content` — it must never enter the conversation
/// history that's replayed to LMStudio on later turns (#406: reasoning-in-
/// history caused a recursive-feedback regression). The promotion is safe under
/// that invariant precisely because a no-tool-call turn is *terminal*: this
/// message is the final answer and is never sent back on a subsequent request.
/// A tool-call turn is left untouched (reasoning is just thinking; the tool call
/// is the action) and its reasoning is stripped as before.
///
/// Also skips promotion on a `finish_reason == "length"` turn: that
/// empty-content, reasoning-dump shape is the per-call-cap runaway the #414
/// stall-recovery handles (pop, nudge, retry). Promoting there would lift a
/// *truncated* dump into the answer AND make the content look non-empty,
/// disabling that recovery. The reported bug is empty content on *successful*
/// (`stop`) terminal turns.
/// Resolve the finish reason the loop ACTS on. Presence of tool calls is
/// ground truth; the wire's `finish_reason` is advisory: Google's
/// OpenAI-compat layer finishes tool-calling turns with `"stop"` (observed
/// live 2026-07-06 on gemini-3.1-pro — the turn carried a complete tool
/// call, the stop arm ended the dispatch at turn 1 with empty content and
/// the tool never ran). A salvaged per-turn-cap turn (#479) also acts as
/// tool_calls, as before.
fn resolve_finish_reason(
    finish_reason: &str,
    has_tool_calls: bool,
    salvaged_per_turn_cap: bool,
) -> &str {
    if salvaged_per_turn_cap || (finish_reason == "stop" && has_tool_calls) {
        "tool_calls"
    } else {
        finish_reason
    }
}

fn promote_terminal_reasoning(msg: &mut Message, finish_reason: &str) {
    let has_tools = msg.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
    let content_empty = msg.content.as_deref().map_or(true, |c| c.trim().is_empty());
    if !has_tools && content_empty && finish_reason != "length" {
        if let Some(reasoning) = msg.reasoning_content.as_deref() {
            if !reasoning.trim().is_empty() {
                msg.content = Some(reasoning.to_string());
            }
        }
    }
    // (#1221) EXCEPT on a length finish. The strip exists so a normal turn's
    // reasoning is not echoed back to the model on the next call — the usual
    // convention, and correct. But a length turn is the one case that must
    // carry its reasoning forward: the checkpoint gate hands it back inside the
    // think region so the model RESUMES instead of restarting, and it cannot do
    // that with a field this function already emptied.
    //
    // Ordering is why this has to live here rather than at the call site:
    // `promote_terminal_reasoning` runs immediately after the response lands,
    // while `per_turn_reasoning` — what the gate reads — is assembled much
    // later. Clearing here made the gate see an empty string and take the
    // "no reasoning to hand back" fallback on every single checkpoint,
    // silently reducing the feature to a no-op.
    if finish_reason != "length" {
        msg.reasoning_content = None;
    }
}

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
/// hardened in #474). The linear 75% point, floored so it never fires on
/// loop iteration 1 (budget=1 → 0 without the floor) and held strictly
/// below the budget so a soft warning always precedes the host's hard
/// kill.
///
/// We deliberately do NOT impose an absolute minimum headroom (e.g.
/// "always ≥30s before the kill"). For any budget below ~120s such a cap
/// forces the warning earlier than 75% — and for small budgets it
/// collapses to "fire on iteration 1" and becomes non-monotonic (a 31s
/// budget would warn EARLIER than a 30s one — the bug #474's first cut
/// shipped). Proportional 25% headroom is the coherent, monotonic model;
/// the hard kill at 100% is the unconditional safety net for the
/// small-budget edge.
fn inactivity_soft_threshold_secs(budget_secs: u64) -> u64 {
    const RATIO: f64 = 0.75;
    let linear = ((budget_secs as f64) * RATIO) as u64;
    // clamp(low, high): never zero; never >= budget (always some headroom).
    linear.clamp(1, budget_secs.saturating_sub(1).max(1))
}

/// One step of the same-file test-cadence-drift state machine (#465/#472).
/// Given the just-edited path (`None` when the edit args were malformed or
/// path-less), the previously-edited path, the current consecutive-edit
/// counter, and the fire threshold, returns the new
/// `(last_edited_path, counter, fired_path)`:
///
/// - same path as last → increment the counter
/// - a new path        → reset counter to 1, remember the path
/// - `None` (#472)      → HOLD state: neither increment nor reset, so a
///   transient malformed-args edit can't erase an in-progress thrash run
/// - counter reaches `threshold` → `fired_path = Some(path)` and the
///   counter + path edge-reset, so the next nudge needs another full run
///
/// Pure + total so the detector is unit-testable independent of `run()`.
fn cadence_drift_step(
    path: Option<&str>,
    last_edited_path: Option<String>,
    counter: u32,
    threshold: u32,
) -> (Option<String>, u32, Option<String>) {
    let (last, count) = match path {
        Some(p) if last_edited_path.as_deref() == Some(p) => {
            (Some(p.to_string()), counter.saturating_add(1))
        }
        Some(p) => (Some(p.to_string()), 1),
        None => (last_edited_path, counter), // #472: hold on malformed args
    };
    if count >= threshold {
        (None, 0, last) // edge-reset; surface the offending path to the caller
    } else {
        (last, count, None)
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

/// (#1001) BLAKE3 hex of a tool's target file at detector-firing time, so a
/// caution keyed to a file can later be ranked DOWN as stale when that file's
/// content has since changed (the staleness check in #1002 recomputes with the
/// same algorithm). Derives the file from the tool's `path` arg; best-effort —
/// a non-file tool (no `path`), an absent/unreadable file, or a file past the
/// size guard yields `None` (no hash, never a misleading one). The guard bounds
/// the read so a pathological file can't stall the loop. BLAKE3 (not std
/// `DefaultHasher`) because this hash is persisted and compared across
/// dispatches/versions — it must be stable forever.
fn detector_code_hash(canonical_args: &str) -> Option<String> {
    /// 10 MiB — far above any source file a coder dispatch edits; a larger
    /// "file" is almost certainly not code, so skip rather than read it all.
    const MAX_HASH_BYTES: u64 = 10 * 1024 * 1024;
    // The model's `path` is resolved by the file tools against the container
    // workspace root (`/workspace`, the Dockerfile `WORKDIR`). This bare
    // `std::fs` read resolves a relative path against the process cwd — which
    // is that same `/workspace` because of the `WORKDIR`. The coupling is
    // implicit: a relative path lands on the right file ONLY while cwd ==
    // `/workspace`. If a future change adds a configurable workspace root, this
    // must resolve against it too. Failing that, the worst case is a `None`
    // hash (a missed staleness signal), never a wrong-file hash for a path that
    // doesn't resolve — best-effort by design.
    let path = serde_json::from_str::<serde_json::Value>(canonical_args)
        .ok()?
        .get("path")?
        .as_str()?
        .to_owned();
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_HASH_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
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
/// methodology research, cross-phase memory) not correctness, per
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
/// Implementation is a tag-scan, not a regex — keeps the runtime free of
/// regex deps. It is FIRST-CLOSE-WINS: each `<think>` pairs with the next
/// `</think>`, so a (rare) nested `<think>` inside another would mis-segment
/// rather than nest by outermost boundary. Acceptable — qwen 3.x
/// thinking-mode doesn't emit nested think tags. Malformed (unclosed) tags
/// are ignored. (#905: doc corrected to match the first-close-wins behavior.)
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

    /// Google's compat layer finishes tool-calling turns with `"stop"` —
    /// tool-call presence must override it or the tool never runs and the
    /// dispatch ends at turn 1 with empty content (observed live 2026-07-06,
    /// gemini-3.1-pro). A genuine stop (no tool calls) stays stop; other
    /// reasons pass through; salvage still forces tool_calls.
    #[test]
    fn resolve_finish_reason_tool_presence_beats_stop() {
        assert_eq!(resolve_finish_reason("stop", true, false), "tool_calls");
        assert_eq!(resolve_finish_reason("stop", false, false), "stop");
        assert_eq!(resolve_finish_reason("tool_calls", true, false), "tool_calls");
        assert_eq!(resolve_finish_reason("length", false, false), "length");
        // Salvage (#479) still forces tool_calls regardless.
        assert_eq!(resolve_finish_reason("length", true, true), "tool_calls");
        // A non-stop reason with tool calls present is NOT rewritten —
        // the length arm's stall recovery owns that shape.
        assert_eq!(resolve_finish_reason("length", true, false), "length");
    }

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
            phase_id: None,
        }
    }

    // ─── #854: endpoint stale-token detection ──────────────────────

    #[test]
    fn frozen_prompt_turns_increments_only_on_identical_count() {
        // First observation seeds the baseline — never "frozen".
        assert_eq!(update_frozen_prompt_turns(None, 1000, 0), 0);
        // Growth (healthy conversation) resets to 0.
        assert_eq!(update_frozen_prompt_turns(Some(1000), 1200, 0), 0);
        assert_eq!(update_frozen_prompt_turns(Some(1000), 1200, 5), 0);
        // A drop (legitimate post-compaction shrink) resets to 0.
        assert_eq!(update_frozen_prompt_turns(Some(1200), 600, 5), 0);
        // Identical to last turn = frozen → increment.
        assert_eq!(update_frozen_prompt_turns(Some(48109), 48109, 0), 1);
        assert_eq!(update_frozen_prompt_turns(Some(48109), 48109, 2), 3);
    }

    #[test]
    fn frozen_prompt_turns_crosses_stale_threshold_after_repeated_freeze() {
        // Replays the #854 shape: a count stuck at the same value across turns.
        // Threshold is reached once the counter hits STALE_PROMPT_TOKENS_TURNS.
        let frozen = 48109u32;
        let mut count = 0u32;
        let mut prev = Some(frozen);
        // Simulate consecutive turns reporting the identical frozen value.
        for _ in 0..STALE_PROMPT_TOKENS_TURNS {
            count = update_frozen_prompt_turns(prev, frozen, count);
            prev = Some(frozen);
        }
        assert!(
            count >= STALE_PROMPT_TOKENS_TURNS,
            "expected staleness after {STALE_PROMPT_TOKENS_TURNS} identical reports, got {count}"
        );
        // A single fresh (growing) report clears it immediately.
        assert_eq!(update_frozen_prompt_turns(prev, frozen + 1, count), 0);
    }

    #[test]
    fn persist_writes_compaction_json_to_runtime_dir() {
        let tmp = tempfile::Builder::new().prefix("persist-compaction").tempdir().unwrap();
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
        let tmp = tempfile::Builder::new().prefix("persist-mkdir").tempdir().unwrap();
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
        let tmp = tempfile::Builder::new().prefix("persist-unwritable").tempdir().unwrap();
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
                extra_content: None,
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
    fn promote_terminal_reasoning_lifts_reasoning_on_terminal_turn() {
        // (#1050) Thinking model: empty content, no tool calls, answer in reasoning.
        let mut msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: Some(r#"{"verdict":"flag","findings":[]}"#.into()),
        };
        promote_terminal_reasoning(&mut msg, "stop");
        assert_eq!(
            msg.content.as_deref(),
            Some(r#"{"verdict":"flag","findings":[]}"#),
            "reasoning must promote to content on a terminal turn",
        );
        assert_eq!(
            msg.reasoning_content, None,
            "reasoning_content must be stripped so it never enters history (#406)",
        );
    }

    #[test]
    fn promote_terminal_reasoning_skips_when_tool_calls_present() {
        // Tool-call turn: reasoning is just thinking — do NOT promote.
        let mut msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                extra_content: None,
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: Some("thinking which tool to use".into()),
        };
        promote_terminal_reasoning(&mut msg, "stop");
        assert_eq!(
            msg.content, None,
            "reasoning must NOT be promoted on a tool-call turn",
        );
        assert_eq!(msg.reasoning_content, None, "reasoning still stripped (#406)");
    }

    #[test]
    fn promote_terminal_reasoning_skips_on_length_truncation() {
        // (#1050 QA) A length-capped runaway (empty content + reasoning dump, no
        // tool calls) is the #414 stall-recovery shape — do NOT promote, so the
        // pop+nudge+retry path stays reachable.
        //
        // (#1221) But the reasoning is NO LONGER STRIPPED, and that inversion is
        // the whole point. Stripping it here is what made the checkpoint gate
        // read an EMPTY slice on every check-in: `promote_terminal_reasoning`
        // cleared `reasoning_content` for every finish reason, so the ONE shape
        // that needs rescuing took the one path that discarded it. Measured
        // live: 13 API calls produced exactly one `model.reasoning` event.
        let mut msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: Some("truncated runaway reasoning...".into()),
        };
        promote_terminal_reasoning(&mut msg, "length");
        assert_eq!(
            msg.content, None,
            "must NOT promote on a length-truncated turn (preserves stall recovery)",
        );
        assert_eq!(
            msg.reasoning_content.as_deref(),
            Some("truncated runaway reasoning..."),
            "a length-truncated turn must KEEP its reasoning — it is the input the \
             checkpoint gate reads and the text handed back as the prefill (#1221)"
        );
    }

    #[test]
    fn promote_terminal_reasoning_leaves_real_content_untouched() {
        let mut msg = Message {
            role: "assistant".into(),
            content: Some("the real answer".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: Some("some thinking".into()),
        };
        promote_terminal_reasoning(&mut msg, "stop");
        assert_eq!(msg.content.as_deref(), Some("the real answer"));
        assert_eq!(msg.reasoning_content, None);
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
                    extra_content: None,
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
    pub(super) fn chat_response_json(
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
    /// (#1221) A turn that never reasoned must NOT get its answer wrapped in
    /// `<think>`.
    ///
    /// `max_tokens` bounds every request, so a turn writing a long ANSWER hits
    /// the checkpoint interval exactly like a thinking turn does. The first cut
    /// fell back to `content` when no reasoning was present and then wrapped
    /// that content in a think block — handing the model its own committed
    /// output back as scratch work. That is the category error the whole
    /// prefill design exists to avoid, inverted: a `pr-reviewer` emitting a
    /// large findings JSON would have had that JSON re-presented to it as a
    /// thought it was still having.
    #[test]
    #[serial_test::serial]
    fn a_non_reasoning_turn_resumes_its_answer_without_think_delimiters() {
        let server = MockServer::start();
        // Plain content, no `<think>` anywhere, no `reasoning_content`.
        let answer: String = (0..200).map(|j| format!("word{j} ")).collect();
        let body = answer.clone();
        let _m = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .json_body(chat_response_json(Some(&body), None, "length", 100, 200));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("noreason").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("answer")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), Some(600), Some(200), Some(200), std::collections::BTreeMap::new(), None,
        )
        .expect("non-reasoning checkpoint loop returns Ok(outcome)");

        let prefill = outcome
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.content.as_deref())
            .expect("an assistant prefill is present");

        assert!(
            !prefill.contains("<think>"),
            "a turn that never reasoned must resume as ITSELF; wrapping its \
             answer in a think block tells the model its committed output was \
             scratch work. Got: {:?}",
            &prefill[..prefill.len().min(120)]
        );
        assert!(
            prefill.starts_with("word0"),
            "the answer must be handed back verbatim from its first token, got: {:?}",
            &prefill[..prefill.len().min(60)]
        );
    }


    /// (#1221) A turn that CONCLUDES after checkpointing must keep its whole
    /// answer where `main.rs` looks for it.
    ///
    /// A prefill continuation returns only the SUFFIX. Pushing that as a fresh
    /// message left the accumulated body orphaned in the stale prefill one slot
    /// earlier, and `main.rs` takes "the last assistant message" as the
    /// deliverable — so the envelope, the JSON content and the operator preview
    /// got the tail and nothing else. Modal path, not an edge case: most turns
    /// conclude rather than degenerate.
    #[test]
    #[serial_test::serial]
    fn a_concluding_checkpointed_turn_keeps_its_whole_answer() {
        let server = MockServer::start();
        // Two MUTUALLY EXCLUSIVE mocks keyed on the request body. `mock()` takes
        // an FnOnce that runs ONCE at registration, so a call-counter inside it
        // is evaluated a single time and the mock answers identically forever —
        // which made an earlier version of this test spin to 6160 checkpoints
        // and deadlock every `#[serial]` test behind it.
        const MARKER: &str = "PARTONE-BODY-THAT-MUST-SURVIVE";
        let _first = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    !b.contains(MARKER)
                });
            then.status(200).json_body(chat_response_json(
                Some(MARKER), None, "length", 100, 200,
            ));
        });
        let _second = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    b.contains(MARKER)
                });
            then.status(200).json_body(chat_response_json(
                Some("PARTTWO-CONCLUSION"), None, "stop", 100, 20,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ckconclude2").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("t"), Message::user("answer")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(10), None, Some(200), Some(200), std::collections::BTreeMap::new(), None,
        )
        .expect("concluding checkpointed turn returns Ok");

        // Exactly what main.rs does to produce the deliverable.
        let final_assistant = outcome
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        assert!(
            final_assistant.contains(MARKER),
            "the deliverable lost everything before the final continuation — got {final_assistant:?}"
        );
        assert!(
            final_assistant.contains("PARTTWO-CONCLUSION"),
            "the deliverable must also carry the concluding text — got {final_assistant:?}"
        );
        let assistants = outcome.messages.iter().filter(|m| m.role == "assistant").count();
        assert_eq!(
            assistants, 1,
            "the stale prefill must be folded away, not left beside the conclusion"
        );
    }

    /// (#1221) A `conclude` verdict closes the THOUGHT, not the TURN.
    ///
    /// The first cut set `resuming_after_checkpoint = !degenerate`, so a
    /// conclude reported itself as not-resuming. The next iteration therefore
    /// ran the fresh-turn reset, wiped the accumulation, and the model
    /// regenerated the identical thought from scratch. Observed live: the tail
    /// ratios of checkpoints 6-10 reproduced 1-5 to four decimal places
    /// (1.0000, 0.5398, 0.3532, 0.2624, 0.2088) and the run would have cycled
    /// until context exhaustion — the gate ruling correctly and the loop
    /// discarding the ruling one iteration later.
    #[test]
    #[serial_test::serial]
    fn concluding_closes_the_thought_without_restarting_the_turn() {
        let server = MockServer::start();
        // Deliberately degenerate: one clause repeated, so the gate concludes
        // on the very first checkpoint and every later call is post-close.
        let repetitive = format!(
            "<think>\n{}",
            "the same clause again and again ".repeat(40)
        );
        let body = repetitive.clone();
        let _m = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .json_body(chat_response_json(Some(&body), None, "length", 100, 200));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ckconclude").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), Some(1000), Some(200), Some(200), std::collections::BTreeMap::new(), None,
        )
        .expect("concluding loop returns Ok(outcome)");

        let prefill = outcome
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                m.content
                    .as_deref()
                    .filter(|c| c.contains("<think>"))
                    .map(str::to_string)
            })
            .expect("a checkpoint prefill is present");

        assert_eq!(
            prefill.matches("</think>").count(),
            1,
            "the thought must close exactly ONCE and stay closed; {} closings means \
             the gate re-fired on an already-concluded thought",
            prefill.matches("</think>").count()
        );
        // The accumulation has to survive the conclude — that is the whole bug.
        // Post-close continuations append AFTER the closing delimiter.
        let (thought, answer) = prefill.split_once("</think>").expect("closed thought");
        assert!(
            thought.contains("the same clause"),
            "the reasoning must still be present after the conclude, not wiped"
        );
        assert!(
            !answer.is_empty(),
            "post-conclude output must land after the closing delimiter, not inside \
             a re-opened think block"
        );
        // A conclude is not a turn boundary: many API calls, still one turn.
        assert_eq!(
            outcome.turns, 1,
            "a conclude closes the thought, not the turn; turns={} means the loop \
             treated it as a boundary and restarted the thought",
            outcome.turns
        );
    }

    /// (#1221) The checkpoint prefill must REPLACE the previous one and carry
    /// the WHOLE thought — the two halves of the same invariant.
    ///
    /// Both were wrong in the first implementation, and unit tests did not
    /// notice because every existing test asserts `terminal_reason` and token
    /// counts, never the shape of the message thread that goes back out. A live
    /// 30-checkpoint dispatch was what exposed it: the outgoing request carried
    /// thirty sibling assistant messages, each opening its own `<think>` with a
    /// truncated copy of the same answer, so the model restarted rather than
    /// resumed and could never converge. This test reads the thread.
    #[test]
    #[serial_test::serial]
    fn checkpoint_prefill_replaces_previous_and_carries_accumulated_thought() {
        let server = MockServer::start();
        // Distinct tokens so the accumulation is verifiable by inspection and
        // the degeneracy gate stays on the `continue` branch for this run.
        // An inline-think model cut mid-thought: an UNCLOSED `<think>`, which is
        // the shape every truncated reasoning turn actually has.
        let slice: String = format!(
            "<think>\n{}",
            (0..80).map(|i| format!("step{i}")).collect::<Vec<_>>().join(" ")
        );
        let slice_body = slice.clone();
        let _m = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(&slice_body),
                None,
                // Always truncated at the cap → every call checkpoints.
                "length",
                100,
                200,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ckprefill").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think hard")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();

        // 200 completion tokens per call against a 600 cumulative cap stops the
        // run after a handful of checkpoints — enough for stacking to show.
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(100),
            Some(600),
            Some(200),
            Some(200),
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("checkpointing loop returns Ok(outcome)");

        let prefills: Vec<&Message> = outcome
            .messages
            .iter()
            .filter(|m| {
                m.role == "assistant"
                    && m.content.as_deref().is_some_and(|c| c.contains("<think>"))
            })
            .collect();

        assert_eq!(
            prefills.len(),
            1,
            "the thread must carry exactly ONE checkpoint prefill; {} of them means \
             each checkpoint appended beside the last instead of replacing it, which \
             is what made the model restart its answer every call",
            prefills.len()
        );

        let body = prefills[0]
            .content
            .as_deref()
            .expect("prefill message has content");
        // NOT asserting one `<think>` here: this mock replays its opener on
        // every call, which a real continuation never does (the model resumes
        // inside the block darkmux handed back). The invariant that matters —
        // ONE prefill message rather than a chain of restarts — is asserted
        // above, and the accumulation is asserted below.
        // The accumulation: the first slice's opening token must appear once per
        // checkpoint, not once total.
        let repeats = body.matches("step0 ").count();
        assert!(
            repeats >= 2,
            "the prefill must hand back the whole thought so far, not just the \
             newest slice — expected the first slice to still be present after \
             later checkpoints, found {repeats} occurrence(s)"
        );
    }

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
        let tmp = tempfile::Builder::new().prefix("cumtokens").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("burn budget")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // (#457) Test specifically exercises the cumulative-tokens cap.
        // After the cap became operator-opt-in (default None = unlimited),
        // we have to pass an explicit Some() here or the loop runs
        // unbounded against a mock that returns infinite identical
        // length-finish responses. 250000 matches the prior hardcoded
        // default value the test was originally written against.
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), Some(250_000), None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("under-budget").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // (#457) Counter-test to the cap-fire path. Set Some(250_000)
        // for parity with the cap-fire test; the mock returns a stop
        // turn quickly so we never approach it.
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), Some(250_000), None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("maxturns").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop forever")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // (#457) Test exercises the MaxTurns terminal — needs Some(N)
        // for the cap to fire. 100 matches the prior hardcoded default.
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("failure-rate").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop fail")];
        let tools = [Tool::Bash];

        let cfg = compaction::CompactionConfig::never_compact();
        // (#457) Test relies on MaxTurns to terminate the loop — needs
        // Some(100) explicitly now that the cap is operator-opt-in.
        let _outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        assert_eq!(first["failure_count"], 3);
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
        let tmp = tempfile::Builder::new().prefix("cycle-detect").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // Let the loop run to MAX_TURNS — the cycle should fire well
        // before. (#457) Cap is operator-opt-in now; pass Some(100)
        // explicitly so the loop terminates at the same point this
        // test was originally written against.
        let _outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("feedback-injection").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // (#457) Same MaxTurns-relying pattern as the cycle/cascade
        // tests above; needs Some(100) now that the cap is opt-in.
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("xml-promote").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
                extra_content: None,
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
                extra_content: None,
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
        let tmp = tempfile::Builder::new().prefix("per-turn-cap-salvage").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        // Cap turns at 3 so the loop terminates if salvage works (it'll
        // run turn 1 → salvage dispatch → turn 2 → ... → MAX_TURNS).
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
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

    /// (#1221) The per-call cap override reaches the whole loop: with
    /// `max_tokens_per_call = Some(5000)`, a length-finish at exactly 5000
    /// completion tokens is detected as a cap hit and salvaged. Under the
    /// built-in default (10000) this same response would MISS salvage
    /// detection and the length arm would bail with an error — so this test
    /// passing proves the override, not the default, drove the decision.
    #[test]
    #[serial_test::serial]
    fn per_call_cap_override_moves_the_salvage_threshold() {
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
                5000,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new()
            .prefix("per-call-cap-override")
            .tempdir()
            .unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            Some(5000),
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect(
            "a length-finish at the OVERRIDDEN cap must salvage — an Err here \
             means the override never reached salvage detection (#1221)",
        );
        assert!(outcome.turns >= 1);
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        assert!(
            raw.lines().any(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                v.get("type").and_then(|t| t.as_str())
                    == Some("dispatch.per_turn_cap.salvaged")
            }),
            "salvage at the overridden cap must be recorded in the trajectory"
        );
    }

    /// (#1221) The cap-cliff: length-finish with PARTIAL content at exactly
    /// the per-call cap must NOT kill the dispatch (pre-fix it returned Err,
    /// discarding every prior productive turn — dialectic shakedown-2's
    /// failure mode). It routes through the stall recovery: drop + nudge +
    /// bounded budget, ending in a clean EscalationTriggered outcome when
    /// the mock repeats the shape past the budget.
    #[test]
    #[serial_test::serial]
    fn cap_cliff_partial_content_recovers_instead_of_killing_the_dispatch() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial reasoning spill that got truncated mid-"),
                None,
                "length",
                100,
                // cap-1: the LIVE-observed shape (LMStudio stops before the
                // token that would exceed the cap) — pins the tolerance
                // match, since exact equality never occurs in production.
                MAX_TOKENS_PER_CALL - 1,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("cap-cliff").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("go")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(10),
            None,
            Some(1000),
            Some(1000),
            std::collections::BTreeMap::new(),
            None,
        )
        .expect(
            "a partial-content cap hit must recover (drop + nudge + budget), \
             not return Err — an Err here is the shakedown-2 dispatch-killing \
             cliff (#1221)",
        );
        assert!(
            matches!(
                outcome.terminal_reason,
                TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted)
            ),
            "repeating the cap-cliff past the recovery budget must end in a \
             clean escalation, got {:?}",
            outcome.terminal_reason
        );
    }

    /// (#1221) A length-finish with partial content BELOW the cap is context
    /// overflow — a config problem recovery cannot fix. It must stay a hard
    /// error (and name overflow, not the cap).
    #[test]
    #[serial_test::serial]
    fn below_cap_length_is_still_a_context_overflow_hard_error() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial content"),
                None,
                "length",
                100,
                4000,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ctx-overflow").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("go")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let err = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(10),
            None,
            Some(10_000),
            Some(10_000),
            std::collections::BTreeMap::new(),
            None,
        )
        .expect_err("below-cap partial-content length must stay a hard error");
        assert!(
            err.to_string().contains("context overflow"),
            "the error must name context overflow, got: {err:#}"
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
        let tmp = tempfile::Builder::new().prefix("salvage-feedback-disabled").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
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
        let tmp = tempfile::Builder::new().prefix("salvage-clear-content").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(2),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
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
        let tmp = tempfile::Builder::new().prefix("per-turn-cap-no-salvage").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let result = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(3),
            None,
            Some(1000),
            Some(1000),
            std::collections::BTreeMap::new(),
            None,
        );

        // (#1221 re-target) Malformed args must still never be DISPATCHED
        // (no salvage), but an at-cap malformed turn now recovers via
        // drop + nudge instead of killing the dispatch. The mock repeats,
        // so the run ends in a clean escalation — and the trajectory must
        // contain no tool.completed event (nothing was ever dispatched).
        let outcome = result.expect(
            "malformed args at the cap must recover (drop + nudge), not bail (#1221)",
        );
        assert!(matches!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted)
        ));
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        assert!(
            !raw.lines().any(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                v.get("type").and_then(|t| t.as_str()) == Some("tool.completed")
            }),
            "a malformed tool call must never be dispatched, even under recovery"
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
        let tmp = tempfile::Builder::new().prefix("xml-promote-length").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("xml-promote-reason").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("reasoning-invariant").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("max-tokens-cap").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(10_000), std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("compaction-smoke").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![
            Message::system("you are a test assistant"),
            Message::user("hi"),
        ];
        let tools = [Tool::Read, Tool::Edit, Tool::Bash];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
    /// Pre-#368 this test set/unset a compaction-threshold env var with
    /// a 40-line EnvGuard for restore-on-drop and required serial
    /// execution. Post-#368 the runtime reads compaction config from
    /// explicit params (no env — that env knob no longer exists), so
    /// this is just a struct literal.
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
                // (#1389) >= MIN_SUMMARY_CHARS and delimiter-free, so the
                // narrative floor + sanitizer accept it; the enlarged padding
                // below keeps every compaction's middle comfortably larger than
                // this summary, clearing the min-reduction guard.
                Some(
                    "Summary: the assistant repeatedly issued a read tool call against the \
                     workspace file and inspected the returned contents. No decisions were \
                     finalized and no files were modified. The next concrete action is to \
                     continue reading and then act on what the file contains.",
                ),
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
        let tmp = tempfile::Builder::new().prefix("compaction-fire").tempdir().unwrap();
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
        // (#1389) Long padding so the FIRST compaction's middle (these 4
        // messages) is comfortably larger than the mock summary, clearing the
        // min-reduction guard. Later recompactions fold the prior summary plus
        // two fresh turns, which stays larger than the summary on its own.
        let pad = "context detail that occupies transcript space ".repeat(6);
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}: {pad}")));
            initial.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        let tools = [Tool::Read];

        // Run. Expected to error eventually (mock loops forever on
        // tool_calls; will hit MAX_TURNS); we don't care about the
        // outcome's Ok/Err — just whether the compactor was invoked
        // along the way. The result IS the side-effect assertion below.
        let outcome = run(&client, &client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None);

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

    /// (#1187 audit finding) Compaction must ALWAYS use `compactor_client`, never
    /// `client` — a remote-brain dispatch's `client` talks to a remote endpoint
    /// (Azure/OpenAI) but `compaction_cfg.compactor_model` is always a local
    /// utility-model id, so routing compaction through `client` either
    /// silently burns the remote endpoint's budget on the wrong model, or
    /// 404s and fails the whole dispatch. Regression-locks the fix with TWO
    /// distinct mock servers standing in for "remote brain" (`client`) vs
    /// "local LMStudio" (`compactor_client`): if a future change reverts to
    /// routing compaction through `client`, the compactor server never
    /// receives a request and this test fails loudly.
    #[test]
    fn compaction_uses_compactor_client_not_primary_client() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let primary_server = MockServer::start();
        let compactor_server = MockServer::start();

        let _primary_mock = primary_server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
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
        let compactor_mock = compactor_server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                // (#1389) >= MIN_SUMMARY_CHARS and delimiter-free, so the
                // narrative floor + sanitizer accept it; the enlarged padding
                // below keeps every compaction's middle comfortably larger than
                // this summary, clearing the min-reduction guard.
                Some(
                    "Summary: the assistant repeatedly issued a read tool call against the \
                     workspace file and inspected the returned contents. No decisions were \
                     finalized and no files were modified. The next concrete action is to \
                     continue reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", primary_server.base_url()));
        let compactor_client =
            LmStudioClient::with_base_url(format!("{}/v1", compactor_server.base_url()));

        let tmp = tempfile::Builder::new().prefix("compaction-client-split").tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        // (#1389) Long padding so the FIRST compaction's middle (these 4
        // messages) is comfortably larger than the mock summary, clearing the
        // min-reduction guard. Later recompactions fold the prior summary plus
        // two fresh turns, which stays larger than the summary on its own.
        let pad = "context detail that occupies transcript space ".repeat(6);
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}: {pad}")));
            initial.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        let tools = [Tool::Read];

        let _outcome = run(
            &client,
            &compactor_client,
            "test-primary",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(100),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        );

        assert!(
            compactor_mock.hits() >= 1,
            "compaction must route through compactor_client (the local-LMStudio \
             server), not the primary/remote client — got 0 hits on the \
             compactor server, meaning compaction either never fired or was \
             misrouted to the primary client"
        );
    }

    /// (#854) Regression-lock for the load-bearing path: a `usage.prompt_tokens`
    /// frozen BELOW the threshold (the endpoint-misreport signature) must still
    /// drive a compaction via the local-estimate substitution, and surface
    /// exactly one `dispatch.context.stale_tokens` event. WITHOUT the fix, the
    /// reported count never crosses the threshold and compaction never fires —
    /// the degenerate cycle. Mirrors `loop_triggers_compaction_when_threshold_
    /// crossed`, but the reported count is STUCK under the threshold while the
    /// seeded thread is large enough that the chars/4 estimate clears it.
    #[test]
    fn stale_frozen_prompt_tokens_forces_compaction_and_fires_event_once() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 5000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = MockServer::start();
        // Primary: EVERY call reports prompt_tokens FROZEN at 4000 — below the
        // 5000 threshold, so the reported count never trips needs_compaction
        // (the #854 endpoint-misreport). Same read call each turn keeps the
        // mock simple; the cycle detector may also fire — harmless here.
        let _primary = server.mock(|when, then| {
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
                4000, // FROZEN, below the 5000 threshold
                50,
            ));
        });
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                // (#1389) >= MIN_SUMMARY_CHARS and delimiter-free, so the
                // narrative floor + sanitizer accept it; the enlarged padding
                // below keeps every compaction's middle comfortably larger than
                // this summary, clearing the min-reduction guard.
                Some(
                    "Summary: the assistant repeatedly issued a read tool call against the \
                     workspace file and inspected the returned contents. No decisions were \
                     finalized and no files were modified. The next concrete action is to \
                     continue reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("stale-compaction").tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());

        // Seed a LARGE middle so the chars/4 estimate exceeds the 5000-token
        // threshold once the reported count is judged stale. The big padding
        // sits between PRESERVE_HEAD (first 2) and PRESERVE_TAIL (last 4), in
        // the compactable region. ~24K chars / 4 ≈ 6000 > 5000.
        let big = "x".repeat(8000);
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding {i} {big}")));
            initial.push(Message::assistant(format!("ack {i}")));
        }
        let tools = [Tool::Read];

        // max_turns=6 bounds it to a SINGLE stale episode: the frozen counter
        // climbs 0→1→2→3 across turns 1-4, fires + compacts + resets at turn 4,
        // and the two remaining turns can't reach 3 again.
        let _outcome = run(&client, &client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(6), None, None, None, std::collections::BTreeMap::new(), None);

        // (1) The fix fired a compaction even though the reported count never
        // crossed the threshold — the #854 regression-lock.
        assert!(
            compactor_mock.hits() >= 1,
            "frozen-below-threshold prompt_tokens did NOT drive a compaction \
             (the #854 bug); compactor hits={}",
            compactor_mock.hits()
        );

        // (2) Exactly one stale_tokens eureka event for the single episode.
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory file must exist");
        let stale_events: Vec<_> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.context.stale_tokens")
            .collect();
        assert_eq!(
            stale_events.len(),
            1,
            "expected exactly one dispatch.context.stale_tokens event for one \
             stale episode, got {}",
            stale_events.len()
        );
        assert_eq!(stale_events[0]["frozen_value"], 4000);
        assert!(stale_events[0]["estimate"].as_u64().unwrap() >= 5000);
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
                // (#1389) >= MIN_SUMMARY_CHARS and delimiter-free, so the
                // narrative floor + sanitizer accept it; the enlarged padding
                // below keeps every compaction's middle comfortably larger than
                // this summary, clearing the min-reduction guard.
                Some(
                    "Summary: the assistant repeatedly issued a read tool call against the \
                     workspace file and inspected the returned contents. No decisions were \
                     finalized and no files were modified. The next concrete action is to \
                     continue reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("compaction-bail").tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        // (#1389) Long padding so the FIRST compaction's middle (these 4
        // messages) is comfortably larger than the mock summary, clearing the
        // min-reduction guard. Later recompactions fold the prior summary plus
        // two fresh turns, which stays larger than the summary on its own.
        let pad = "context detail that occupies transcript space ".repeat(6);
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}: {pad}")));
            initial.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        let tools = [Tool::Read];

        let outcome = run(&client, &client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
                // (#1389) >= MIN_SUMMARY_CHARS and delimiter-free, so the
                // narrative floor + sanitizer accept it; the enlarged padding
                // below keeps every compaction's middle comfortably larger than
                // this summary, clearing the min-reduction guard.
                Some(
                    "Summary: the assistant repeatedly issued a read tool call against the \
                     workspace file and inspected the returned contents. No decisions were \
                     finalized and no files were modified. The next concrete action is to \
                     continue reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("compaction-no-bail").tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        // (#1389) Long padding so the FIRST compaction's middle (these 4
        // messages) is comfortably larger than the mock summary, clearing the
        // min-reduction guard. Later recompactions fold the prior summary plus
        // two fresh turns, which stays larger than the summary on its own.
        let pad = "context detail that occupies transcript space ".repeat(6);
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}: {pad}")));
            initial.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        let tools = [Tool::Read];

        // Loop hits MAX_TURNS (mock loops forever). The key
        // assertion: terminal_reason must be MaxTurns, NOT
        // EscalationTriggered, even though compactions fired.
        let outcome = run(&client, &client, "test-primary", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("stall-recover").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("answer the question")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(1000), std::collections::BTreeMap::new(), None)
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

    /// (#414 PR A → #1221) The OTHER length shape: content (a partial answer)
    /// with `finish_reason=length` AT the per-call cap.
    ///
    /// History of this assertion, kept because it is the point. Pre-#1221 this
    /// BAILED, which killed the whole dispatch and discarded every prior
    /// productive turn. That was replaced by DROPPING the truncated turn — an
    /// improvement, but still built on the theory that a capped turn is noise.
    /// #1221 measured that theory and it is false: 43-50% of turns on the
    /// review corpus hit this arm, and a scraped 51K-char turn was tracing
    /// real code and naming a real bug when it was cut.
    ///
    /// So the turn is now KEPT and the model is asked to conclude from it. The
    /// assertion below is inverted rather than deleted: the count that used to
    /// read 1 ("only the escalation turn survives") now reads 3, because
    /// nothing is thrown away.
    #[test]
    #[serial_test::serial]
    fn length_with_content_at_cap_keeps_the_turn_and_asks_for_a_conclusion() {
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
        let tmp = tempfile::Builder::new().prefix("length-truncated").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("verbose answer")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(1000), Some(1000), std::collections::BTreeMap::new(), None)
            .expect("an at-cap truncation must recover, not kill the dispatch (#1221)");

        assert!(
            matches!(
                outcome.terminal_reason,
                TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted)
            ),
            "budget exhaustion on a repeating cap-cliff must escalate cleanly, got {:?}",
            outcome.terminal_reason
        );
        // The two RECOVERED turns were dropped; only the final turn (the one
        // that exhausted the budget and triggered escalation) remains in
        // history — pre-existing #414 escalation semantics: the last stall
        // turn is handoff evidence, the budget check runs before the pop.
        let kept = outcome
            .messages
            .iter()
            .filter(|m| {
                m.content
                    .as_deref()
                    .map(|c| c.contains("half my answer"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            kept, 3,
            "every truncated turn must SURVIVE in history — the work is the \
             point, and discarding it is the bug #1221 measured (got {kept})"
        );
        // (#1221) The check-in is SILENT. An earlier cut told the model it
        // had "reached the per-call reasoning budget"; measured on a real
        // review, a model invited to stop STOPS — it produced a four-point
        // summary with zero findings where the same model uninterrupted found
        // real ones. So the assertion is inverted rather than deleted: there
        // must be NO budget message at all. The harness reads the slice and
        // hands it back; the model never learns a boundary existed.
        let budget_messages = outcome
            .messages
            .iter()
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .map(|c| c.contains("budget") || c.contains("reasoning budget"))
                        .unwrap_or(false)
            })
            .count();
        assert_eq!(
            budget_messages, 0,
            "the model must never be told a checkpoint happened — a model invited \
             to wrap up will wrap up, and that measurably cost real findings \
             (got {budget_messages} budget message(s))"
        );
    }

    /// (#414 PR A → #1221) Coverage for the `tool_calls: []` empty-array
    /// shape (distinct from `tool_calls: null`/absent) WITH content at the
    /// cap. Same #1221 re-target as the content-present case: recovers via
    /// drop + nudge instead of killing the dispatch.
    #[test]
    #[serial_test::serial]
    fn length_with_content_and_empty_tool_calls_array_recovers_at_cap() {
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
        let tmp = tempfile::Builder::new().prefix("length-empty-tc-array").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(1000), Some(1000), std::collections::BTreeMap::new(), None)
            .expect("length + content + empty-array tool_calls at cap must recover (#1221)");
        assert!(matches!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted)
        ));
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
        let tmp = tempfile::Builder::new().prefix("stall-recover-empty-array").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(1000), std::collections::BTreeMap::new(), None)
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
        let tmp = tempfile::Builder::new().prefix("stall-budget-exhaust").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(1000), std::collections::BTreeMap::new(), None)
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

    /// (#1123) `finish_reason=tool_calls` with NO tool_calls (an empty
    /// completion — the shape a degraded devstral-24b run produced) must
    /// recover like the length-arm stall (#414), NOT hard-`Err` on the first
    /// occurrence. Every call returns the empty-tool_calls shape, so the loop
    /// recovers `MAX_STALL_RECOVERIES` times then escalates — same bounded
    /// behavior as the length-stall, proving the pre-#1123 hard-fail is gone.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_from_empty_tool_calls_then_escalates() {
        let server = MockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None, // no content
                None, // no tool_calls → finish_reason=tool_calls + empty array
                "tool_calls",
                100,
                50,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("empty-toolcalls").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
            .expect("empty finish_reason=tool_calls must recover+escalate, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "empty finish_reason=tool_calls should route to the stall recovery + escalate; got {:?}",
            outcome.terminal_reason
        );
        assert_eq!(outcome.turns, MAX_STALL_RECOVERIES + 1);
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
        let tmp = tempfile::Builder::new().prefix("stall-traj").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let _outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(1000), std::collections::BTreeMap::new(), None)
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

    // ─── (#1001) detector_code_hash ──────────────────────────────────
    #[test]
    fn detector_code_hash_hashes_an_existing_file_and_tracks_content() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(b"fn main() {}")
            .unwrap();
        let args = serde_json::json!({ "path": file.to_str().unwrap() }).to_string();
        let h1 = detector_code_hash(&args).expect("hashes an existing file");
        // BLAKE3 hex is 64 chars and stable for the same bytes.
        assert_eq!(h1.len(), 64);
        assert_eq!(detector_code_hash(&args).as_deref(), Some(h1.as_str()));
        // Content change → different hash (the staleness signal).
        std::fs::write(&file, b"fn main() { changed }").unwrap();
        assert_ne!(detector_code_hash(&args).as_deref(), Some(h1.as_str()));
    }

    #[test]
    fn detector_code_hash_is_none_for_non_file_or_missing() {
        // No `path` arg (e.g. a `bash` cycle) → None.
        assert!(detector_code_hash(r#"{"command":"ls"}"#).is_none());
        // Malformed args → None.
        assert!(detector_code_hash("not json").is_none());
        // A `path` that doesn't exist → None (best-effort, never a fake hash).
        assert!(detector_code_hash(r#"{"path":"/no/such/file.rs"}"#).is_none());
    }

    // ─── (#465/#472) cadence_drift_step state machine ────────────────

    #[test]
    fn cadence_step_increments_on_same_path() {
        let (last, count, fired) = cadence_drift_step(Some("a.rs"), Some("a.rs".into()), 1, 3);
        assert_eq!(last.as_deref(), Some("a.rs"));
        assert_eq!(count, 2);
        assert!(fired.is_none());
    }

    #[test]
    fn cadence_step_resets_to_one_on_new_path() {
        let (last, count, fired) = cadence_drift_step(Some("b.rs"), Some("a.rs".into()), 2, 3);
        assert_eq!(last.as_deref(), Some("b.rs"));
        assert_eq!(count, 1);
        assert!(fired.is_none());
    }

    #[test]
    fn cadence_step_holds_state_on_malformed_args() {
        // #472: a None path (malformed/path-less edit) must NOT reset an
        // in-progress run — it holds the counter and last path.
        let (last, count, fired) = cadence_drift_step(None, Some("a.rs".into()), 2, 3);
        assert_eq!(last.as_deref(), Some("a.rs"), "last path must be held");
        assert_eq!(count, 2, "counter must be held, not reset");
        assert!(fired.is_none());
    }

    #[test]
    fn cadence_step_fires_and_edge_resets_at_threshold() {
        // Third same-file edit crosses threshold=3: fires with the path,
        // then edge-resets so the next nudge needs another full run.
        let (last, count, fired) = cadence_drift_step(Some("a.rs"), Some("a.rs".into()), 2, 3);
        assert_eq!(fired.as_deref(), Some("a.rs"));
        assert_eq!(count, 0, "counter edge-resets after firing");
        assert!(last.is_none(), "last path edge-resets after firing");
    }

    #[test]
    fn cadence_step_full_sequence_with_malformed_interruption() {
        // Integration of the transitions: two same-file edits, a malformed
        // edit (held), then a third same-file edit fires — the malformed
        // args in the middle did NOT let the model dodge the detector.
        let thr = 3;
        let (last, count, fired) = cadence_drift_step(Some("a.rs"), None, 0, thr);
        assert!(fired.is_none() && count == 1);
        let (last, count, fired) = cadence_drift_step(Some("a.rs"), last, count, thr);
        assert!(fired.is_none() && count == 2);
        let (last, count, fired) = cadence_drift_step(None, last, count, thr); // malformed
        assert!(fired.is_none() && count == 2, "held across malformed args");
        let (_last, _count, fired) = cadence_drift_step(Some("a.rs"), last, count, thr);
        assert_eq!(fired.as_deref(), Some("a.rs"), "fires despite the malformed interruption");
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
        // Proportional 75% point; always strictly < budget so a warning
        // precedes the hard kill.
        assert_eq!(inactivity_soft_threshold_secs(10), 7);
        assert_eq!(inactivity_soft_threshold_secs(30), 22);
        assert_eq!(inactivity_soft_threshold_secs(100), 75);
        for b in [2u64, 5, 10, 30, 100] {
            assert!(inactivity_soft_threshold_secs(b) < b, "budget {b}: soft must be < budget");
        }
    }

    #[test]
    fn soft_threshold_is_monotonic_no_headroom_cliff() {
        // Regression for the #474 first-cut bug the QA review caught: a
        // budget=31 fired the soft warning at 1s while budget=30 fired at
        // 22s (a non-monotonic cliff in the (30, ~120] band). The
        // threshold must be non-decreasing in the budget and never jump
        // backward.
        assert_eq!(inactivity_soft_threshold_secs(30), 22);
        assert_eq!(inactivity_soft_threshold_secs(31), 23);
        let mut prev = 0;
        for b in 1u64..=600 {
            let soft = inactivity_soft_threshold_secs(b);
            assert!(soft >= prev, "budget {b}: soft {soft} regressed below {prev}");
            assert!(soft >= 1, "budget {b}: soft must never be zero");
            if b >= 2 {
                assert!(soft < b, "budget {b}: soft {soft} must leave headroom");
            }
            prev = soft;
        }
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

#[cfg(test)]
mod reasoning_feedback_probe {
    //! (#1221) Does a truncated turn's REASONING travel back to the model on
    //! the next call?
    //!
    //! This decides whether "keep the turn" preserves anything in the shape
    //! that matters. A runaway turn is `content: null` with all the substance
    //! in `reasoning_content`, so if that field is dropped on the way out,
    //! keeping the message preserves an empty husk and the model genuinely
    //! does start over.
    //!
    //! `Message::reasoning_content` is `skip_serializing_if = "Option::is_none"`
    //! and its doc claims it is "always None on the request side" — an
    //! assumption about a code path, not an enforced invariant, so it is
    //! measured here rather than believed.
    use super::*;
    use httpmock::prelude::*;

    #[test]
    #[serial_test::serial]
    fn probe_whether_truncated_reasoning_is_echoed_on_the_next_request() {
        let server = MockServer::start();
        let first = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions")
                .matches(|req| {
                    let body = String::from_utf8_lossy(req.body.as_deref().unwrap_or(&[]));
                    !body.contains("SUBSTANTIVE_REASONING_MARKER")
                });
            let mut body = tests::chat_response_json(None, None, "length", 10, MAX_TOKENS_PER_CALL);
            body["choices"][0]["message"]["reasoning_content"] =
                serde_json::json!("SUBSTANTIVE_REASONING_MARKER tracing the write path");
            then.status(200).json_body(body);
        });
        // Fires ONLY if the second request carries the reasoning back.
        let echoed = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions")
                .body_contains("SUBSTANTIVE_REASONING_MARKER");
            then.status(200).json_body(tests::chat_response_json(Some("done"), None, "stop", 10, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reasoning-echo").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client, &client, "test-model",
            vec![Message::system("t"), Message::user("go")],
            &[Tool::Read], &mut traj, false, &cfg, Some(3), None, None,
            None,
            std::collections::BTreeMap::new(), None,
        );

        eprintln!(
            "PROBE: first-request hits={}, reasoning-echoed hits={}, outcome={:?}",
            first.hits(),
            echoed.hits(),
            outcome.as_ref().map(|o| (&o.terminal_reason, o.turns)).map_err(|e| e.to_string())
        );
        if let Ok(o) = &outcome {
            for (i, m) in o.messages.iter().enumerate() {
                eprintln!(
                    "PROBE msg[{i}] role={} content={:?} reasoning={:?}",
                    m.role,
                    m.content.as_deref().map(|c| &c[..c.len().min(40)]),
                    m.reasoning_content.as_deref().map(|c| &c[..c.len().min(40)])
                );
            }
        }
        assert!(first.hits() >= 1, "the truncated turn must have been produced");
    }


}
