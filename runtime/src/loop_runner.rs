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

use crate::bounds::{self, BoundKind, BoundRef};
use crate::checkpoint;
use crate::compaction;
use crate::cycle_detector::{CycleDetector, CycleSignal};
use crate::failure_rate::{FailureCascadeSignal, FailureRateDetector};
use crate::feedback::FeedbackInjector;
use crate::lmstudio::{ChatRequest, ChunkAccumulator, LmStudioClient, Message, ToolCall};
use crate::pace;
use crate::plain_text_tool_calls::promote_plain_text_tool_calls;
use crate::reasoning_loop::{ReasoningLoopDetector, ReasoningLoopSignal};
use crate::stream_gate::{AbortReason, CutSource, StreamGate, StreamOutcome};
use crate::tools::Tool;
#[cfg(not(test))]
use crate::tools::dispatch;
#[cfg(test)]
use observed_dispatch as dispatch;
use darkmux_trajectory::{FailedExec, MalformedReason};
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
/// **(#2836 stage 2) Raised 10,000 -> 32,000, because the reason for 10,000
/// stopped being true.**
///
/// Everything above describes a FAILURE BOUNDARY, and it was set low because
/// the only way to notice a runaway was to stop and look — the per-call cap
/// WAS the looking. Since stage 1 the runtime watches the stream as it
/// arrives and runs the same degeneracy verdict at an observation cadence
/// that costs nothing, so a runaway is caught by the thing that catches
/// runaways, and this is free to be what it says it is.
///
/// Two of that paragraph's own premises are also now false. "A capped turn's
/// reasoning is discarded entirely" — it is handed back as a prefill and the
/// turn resumes. And the cap was never a spend limit: a turn is many CALLS
/// (a checkpoint continuation does not consume a turn), so the same work
/// arrives either way; a low cap only decides whether it comes as one
/// uninterrupted call or several with a re-sent prefill between each,
/// quadratic in the number of chops. Measured live: 8 calls and 20,000
/// completion tokens inside ONE turn.
///
/// **Why 32,000.** Above the largest PRODUCTIVE call observed (18,875 tokens,
/// the lifted-checkpoint proof run) with headroom, and well under the
/// runaway-emission signature this constant was written against (~50K in a
/// 14-min hang). It stays an absolute value for the reason argued above —
/// work shape, not RAM tier.
///
/// It is enforced by the ENGINE, in tokens, on the wire. A first cut of
/// stage 2 tried to enforce it client-side so it could land at a safe
/// boundary; that failed because the runtime counts characters and the true
/// ratio across 85 real calls spans 0.01 to 3.89 — no constant converts one
/// to the other. See `stream_gate::StreamGate::ingest`.
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
const MAX_TOKENS_PER_CALL: u32 = 32_000;

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

/// (#2171) The GENERATION check-in — bounds every call that does NOT carry
/// the reasoning bound above, not just reasoning ones. #2166 made the
/// reasoning check-in apply only once a dispatch has proven it reasons
/// (`dispatch_has_reasoned`); before that point every call — including a
/// non-thinking model's entire dispatch — carried `MAX_TOKENS_PER_CALL`
/// (10000) with NO check-in at all. Devstral (dense 24B, ~10-20 tok/s on an
/// M1 Max) needed 8-16 minutes to exhaust that bound, longer than the
/// default 600s inactivity budget, and LMStudio's tool-call parser buffered
/// rather than streamed — zero chunks, so `last_proof_of_work` never reset
/// and the host hard-killed the container at exit 137 with no envelope.
/// Before #2164 the 1000-token reasoning check-in incidentally kept every
/// call short enough to heartbeat; this restores that property for
/// non-reasoning calls specifically, without reintroducing the "reduce your
/// reasoning" nudge on a model that was never reasoning in the first place
/// (that nudge stays gated on the REASONING bound — see `sent_reasoning_bound`
/// at the cap-selection site).
///
/// Sized to fit a real tool-call batch (larger than the 1000-token thought
/// sample, far smaller than the 10000-token answer ceiling it replaces as
/// the effective per-call cap for non-reasoning calls).
///
/// Override: `DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL` env >
/// `runtime.generation_checkpoint_interval_tokens` config > this default.
const GENERATION_CHECKPOINT_INTERVAL: u32 = 4000;

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
/// the next turn also stalls. A stall RATE that high is the pathology
/// signal — escalate rather than burn more turns trying.
///
/// # What this budget bounds, and what it does NOT (#2229)
///
/// Until #2229 the counter had no pay-down site at all — one `= 0` init
/// and two `saturating_add` bumps — so it was a LIFETIME budget wearing
/// CONSECUTIVE wording, and a thousand productive turns between two
/// unrelated stalls still escalated the dispatch. It now DECAYS BY ONE
/// (`saturating_sub(1)`) on each dispatching call that opens a fresh
/// TURN.
///
/// **Decay, not a reset to zero.** A hard `= 0` bounds only BACK-TO-BACK
/// stalls: one dispatched call between two stalls wipes the counter
/// entirely. Under decay the counter tracks a stall RATE, so:
///
/// - **Bounded** — any sustained ratio ABOVE one stall per productive
///   turn. At 2:1 the counter nets +1 per cycle and reaches the budget;
///   pinned by `a_two_to_one_stall_ratio_still_escalates`.
/// - **Bounded** — isolated stalls arbitrarily far apart are now
///   FORGIVEN rather than escalating, which is #2229's actual bug.
/// - **NOT bounded — an exact 1:1 alternation** (stall, work, stall,
///   work, …). The counter oscillates 1,0,1,0 and never reaches the
///   budget. Measured: a model alternating a stall with one identical
///   trivial `read` ran 40 turns without escalating, and nothing else in
///   the loop stops it either — the #418 cycle detector and #419
///   failure-rate detector are both warn-only, and the HOST inactivity
///   watchdog is reset by the trivial call's own successful
///   `tool.completed`. That shape is bounded ONLY by `max_turns` /
///   `max_cumulative_tokens`, which are operator-opt-in and default
///   `None`. Closing it needs a second, absolute bound; that is a
///   deliberately deferred decision, NOT an oversight, and this note is
///   here so the next reader does not mistake the gap for a claim.
///
/// The decay fires only on a call that opened a NEW turn — a #1221
/// checkpoint continuation is the same logical turn resuming (`turns`
/// was not incremented for it), and refunding a recovery the same turn
/// just spent is not progress. That guard is the one asymmetry from
/// `MAX_CONSECUTIVE_MALFORMED_TURNS` below: that counter increments in
/// ONE arm and resets in that same arm, so it is genuinely arm-local;
/// this one increments in TWO arms (the `"tool_calls"` empty-calls
/// recovery, and the `"length"` arm via `recover_intra_turn_stall`) and
/// decays in one. Same site, NOT the same shape.
const MAX_STALL_RECOVERIES: u32 = 2;

/// (#2169 merge-gate finding 4) How many CONSECUTIVE turns of "every
/// tool_call this turn was invalid/ungranted, nothing dispatched" the loop
/// tolerates before escalating. Same shape as `MAX_STALL_RECOVERIES` above
/// (a small budget, not "warn forever") but a stricter K=3: unlike a
/// runaway-reasoning stall — which a nudge can plausibly break — a model
/// that keeps emitting names that were never in its own function-calling
/// schema is not going to self-correct by getting MORE turns to try again
/// with the same broken habit. 3 gives it the queued feedback message's one
/// real chance (queued on turn N, drained at the top of turn N+1) plus one
/// more turn to prove the nudge didn't land before giving up.
const MAX_CONSECUTIVE_MALFORMED_TURNS: u32 = 3;

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

/// (#2793) How many CONSECUTIVE compactions may leave the thread above the
/// occupancy that triggers compaction before the runtime says so out loud.
///
/// Five rather than two: a couple of unproductive compactions in a row is
/// ordinary on a thread whose per-turn tool output happens to match what the
/// compactor can reclaim, and calling that out would be noise. Five in a row
/// is a shape — the compactor's achievable floor is above its own trigger,
/// so every remaining turn pays a compactor dispatch and none of them buys a
/// turn without one. Measured on the dogfood run that produced #2793: 44
/// compactions across 50 turns.
const UNPRODUCTIVE_COMPACTION_TURNS: u32 = 5;

/// (#3013) Consecutive compactions whose next turn re-reads what the turn
/// before it read, before the run escalates. The same five as
/// [`UNPRODUCTIVE_COMPACTION_TURNS`], for the same reason: one or two repeats
/// are ordinary (the model needs a file it just lost from context); five in a
/// row is a shape.
const COMPACTION_REREAD_TURNS: u32 = 5;

/// (#465) Test-cadence-drift detector — REDESIGNED from #457's
/// edits-since-last-bash counter. The prior shape mis-fired on
/// productive multi-file edit campaigns and missed genuine
/// single-file thrash (Beat 54 N=5). New shape: track the most
/// recently edited path + a same-file repetition counter.
///
/// - Edit/write to a NEW path → reset counter to 1, remember path
///   (path is normalized lexically first, #471, so `./src/x` and
///   `src/x` aren't seen as different files)
/// - Edit/write to the SAME path as last edit → increment counter
/// - Edit/write with unparseable/path-less args → HOLD state (#472):
///   no increment, no reset — a transient malformed edit must not
///   erase an in-progress thrash run
/// - Bash → reset both (verification cleared the slate)
/// - Counter hits THRESHOLD → fire signal, edge-trigger reset
///
/// Multi-file campaign (one edit per file) never trips. Single-
/// file thrash trips at the 3rd consecutive edit. The path is
/// surfaced into the feedback nudge so the model knows which file
/// it's been thrashing on.
const TEST_CADENCE_DRIFT_THRESHOLD: u32 = 3;

/// (#2792) Smallest tool-result body the LAST-RESORT pre-send trim will touch.
///
/// Deliberately far below the soft path's 4,000-byte threshold. That constant
/// protects ordinary short results from a trim whose purpose is reclaiming
/// slack; this one runs only when the request is already over the declared
/// window and the alternative is sending something known to be refused. 512
/// keeps head + marker + tail meaningfully shorter than what it replaces while
/// still reaching the many-medium-results shape that made the first revision
/// of this bound reclaim nothing.
const HARD_TRIM_MIN_BODY_BYTES: usize = 512;

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

/// (#2836) No longer says "up to the per-call cap", because that is usually
/// false now and this is text the MODEL reads.
///
/// The message is sent on two paths. One is a call that returned
/// `tool_calls` with nothing in it. The other is a call the RUNTIME ended
/// after its own degeneracy check fired — which reached no cap at all; the
/// runtime stopped reading. Since the ceiling moved to 32,000 and the
/// check-in came off the wire entirely, a genuine cap hit is now the rare
/// case rather than the usual one.
///
/// A model told it hit a cap it did not hit may reasonably conclude its
/// output was truncated and that the right response is to be briefer, which
/// is not what either path is asking for. The wording now states only what
/// is observably true on both — no tool call, no final answer — and asks
/// for one of them. Per the model-facing doctrine in CLAUDE.md this stays
/// directive and literal, and keeps the `[darkmux-runtime]` provenance
/// prefix.
const STALL_NUDGE_MESSAGE: &str = "[darkmux-runtime] Your previous response \
ended without a tool call and without a final answer. Please either invoke \
a tool to make progress, or provide a direct final answer.";

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
    /// Salvageable state (final messages, partial work) is in the rest
    /// of [`LoopOutcome`], and the completed turns are in the
    /// trajectory, so the frontier-tier handoff skill can pick up where
    /// local-tier left off. KISS-
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
    /// (#2805) Compaction is running every turn and cannot get the thread
    /// below the occupancy that triggers it — so every remaining turn will
    /// pay a compactor dispatch and none will buy a turn without one.
    ///
    /// Distinct from [`CompactionLimitReached`], which fires on a COUNT the
    /// operator set. This fires on a STATE the runtime can prove: N
    /// consecutive compactions installed and each left occupancy at or above
    /// the trigger. #2793 added the detection; this escalates on it, because
    /// naming a runaway is not the same as ending one.
    ///
    /// Measured twice on the same workload before this existed: 50 turns /
    /// 1.05M prompt tokens, then 124 turns / 2.25M, neither converging,
    /// both stopped by hand. Every bound that could have ended them is
    /// absent by default — `max_turns` uncapped, `bail_after_compactions`
    /// disabled, and the inactivity deadline never fires on a dispatch that
    /// is busy. This is the one that does not need the operator to have
    /// predicted the failure in advance.
    CompactionUnproductive,
    /// (#3013) Every compaction succeeds, and the turn after each one
    /// inspects exactly what the turn before it did: the model re-reads the
    /// files the summary told it not to. Distinct from
    /// [`CompactionUnproductive`], which needs the thread to stay above the
    /// trigger; here each compaction is productive and the loop is in the
    /// WORK, so no occupancy counter ever grows. Fires after
    /// [`COMPACTION_REREAD_TURNS`] consecutive repeats.
    CompactionRereadLoop,
    /// (#414 PR A) Intra-turn stall recovery budget
    /// ([`MAX_STALL_RECOVERIES`], operator-overridable via
    /// `runtime.max_stall_recoveries` — #2190) exhausted. Fires when the
    /// model returned `finish_reason=length` with no content and no
    /// tool_calls more times than the budget allows — the GENUINE
    /// runaway-reasoning shape: a turn cut at a bound while still writing
    /// reasoning. The recovery nudge isn't breaking the pattern, so the
    /// dispatch escalates rather than burn more turns on the same stall.
    ///
    /// (#2190) Deliberately does NOT cover `finish_reason=tool_calls` with
    /// an empty `tool_calls` array — see [`EmptyToolCallsExhausted`] for
    /// that shape, split out because it is a DIFFERENT failure with a
    /// different cause (a protocol-shaped model/parser miss, not a
    /// reasoning cut) and conflating the two sent a live diagnosis down
    /// the wrong path twice.
    IntraTurnStallExhausted,
    /// (#2190) Same bounded recovery-budget mechanism as
    /// [`IntraTurnStallExhausted`] (same
    /// [`MAX_STALL_RECOVERIES`]/`runtime.max_stall_recoveries` budget,
    /// same drop-and-nudge recovery), but for a DIFFERENT shape: the model
    /// returned `finish_reason=tool_calls` with an EMPTY `tool_calls`
    /// array — it claimed it was calling a tool and produced none. This is
    /// NOT a reasoning-loop pathology (measured live: Devstral via
    /// LMStudio hit this three turns running at ~19k context, emitting
    /// 286-648 completion tokens each time — nowhere near any per-call
    /// bound, so "runaway reasoning" was factually wrong for this shape).
    /// Split into its own kind so the diagnosis names the real cause: a
    /// protocol-shaped model/parser miss (same Devstral+LMStudio parser
    /// family as #2169/#2182), not a stuck thought.
    EmptyToolCallsExhausted,
    /// (#2171) A turn kept hitting the GENERATION check-in
    /// (`generation_checkpoint_interval_tokens`) more times than
    /// `answer_max_tokens / generation_checkpoint_interval_tokens` allows.
    /// Deliberately bounded by a continuation COUNT, unlike the reasoning
    /// check-in's continuations (which have no count and are bounded only by
    /// the context window, [`Self::TurnContinuationsExhausted`], #3074): a
    /// thought is expected to run long,
    /// but a single answer/tool-call turn that never converges after
    /// `answer_max_tokens` worth of generation-bound continuations is a
    /// model that will not stop on its own, and the alternative is the
    /// same unbounded-continuation shape the reasoning check-in already
    /// tolerates for a region where it should NOT be tolerated.
    GenerationCheckpointBudgetExhausted,
    /// (#3074) One logical turn kept continuing past a checkpoint until the
    /// tokens it generated reached the context window. Every continuation
    /// resends the thought carried so far, so a turn that long cannot be
    /// resumed again: the next request would overflow the window. This is the
    /// bound on the open-ended REASONING check-in (and on any continuation),
    /// derived from the window rather than guessed, and it exists because
    /// `max_turns` does not count continuations (#1221) and the inactivity
    /// deadline resets on every streamed chunk. Absent a configured context
    /// window there is nothing to derive it from, so no bound applies.
    TurnContinuationsExhausted,
    /// (#2169 merge-gate finding 4) `MAX_CONSECUTIVE_MALFORMED_TURNS`
    /// consecutive turns each dispatched ZERO real tool calls — every
    /// `tool_calls` entry named either a non-tool or a real tool this
    /// dispatch wasn't granted. Nothing bounds this under default config
    /// (max_turns/max_cumulative_tokens are operator-opt-in and default
    /// `None`; #419's cascade detector is warn-only; a `model.partial`
    /// heartbeat keeps the host watchdog's deadline alive regardless of
    /// whether any of it is PRODUCTIVE) — pre-#2169 this pathology at
    /// least burned visible tool-message-per-call noise; post-#2169 it is
    /// QUIETER, so it needs its own explicit bound rather than relying on
    /// an operator noticing a repeating detector line.
    MalformedToolCallsExhausted,
}

/// (#2190) The exact snake_case `escalation_*` string `main.rs` emits as the
/// JSON envelope's `result` field for a given [`EscalationReason`] — single
/// source of truth so the envelope's `result` and the
/// `dispatch.escalation.triggered` trajectory event's `reason` field can
/// never name the same termination two different ways. Lives here (not in
/// `main.rs`) because this is where [`EscalationReason`] itself is defined —
/// a match over the enum belongs next to the enum, not duplicated at every
/// consumer.
pub fn escalation_reason_str(reason: EscalationReason) -> &'static str {
    match reason {
        EscalationReason::CompactionLimitReached => "escalation_compaction_limit_reached",
        EscalationReason::CompactionUnproductive => "escalation_compaction_unproductive",
        EscalationReason::CompactionRereadLoop => "escalation_compaction_reread_loop",
        EscalationReason::CumulativeTokensExceeded => "escalation_cumulative_tokens_exceeded",
        EscalationReason::IntraTurnStallExhausted => "escalation_intra_turn_stall_exhausted",
        // (#2190) Deliberately NOT `..._exhausted` — the issue's own spec
        // names this exact string, and it reads better than the suffix:
        // the model didn't exhaust anything, it just returned nothing.
        EscalationReason::EmptyToolCallsExhausted => "escalation_empty_tool_calls",
        EscalationReason::GenerationCheckpointBudgetExhausted => {
            "escalation_generation_checkpoint_budget_exhausted"
        }
        EscalationReason::TurnContinuationsExhausted => "escalation_turn_continuations_exhausted",
        EscalationReason::MalformedToolCallsExhausted => "escalation_malformed_tool_calls",
    }
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

    /// (#1221) The turn's ANSWER text when the loop exits with a checkpoint
    /// prefill still pending.
    ///
    /// `main.rs` derives the deliverable as "the last assistant message", which
    /// is the prefill on every non-terminal exit (escalation, cumulative cap,
    /// max turns) — so the operator got the model's raw scratch work as its
    /// answer. The loop knows which region is the answer; nothing downstream
    /// should have to infer it from message order or delimiters.
    pub final_answer: Option<String>,
    /// (#2094 finding 8) The POST-CLAMP `turn_delay_ms` this dispatch
    /// actually applied — i.e. `resolve_turn_delay_ms`'s output, not the
    /// operator's raw configured value. Distinct from the recorded rests
    /// (which describe what actually happened): this is the CADENCE the
    /// runtime resolved once at startup and would apply to every rest,
    /// known even on a dispatch that took zero rests (e.g. a single-turn
    /// dispatch) — the effective knob, not a derived average.
    pub turn_delay_effective_ms: u64,

    /// (#799) Bash invocations that FAILED TO RUN (never executed) during the
    /// dispatch — the verifier-fabrication backstop. Empty on an honest run.
    pub failed_to_run: Vec<FailedExec>,
}


/// (#2094) Injectable sleep abstraction for the global inter-turn rest.
/// `run()` uses [`RealSleeper`] in production; tests inject a recording
/// sleeper so the exact call count + duration can be asserted without
/// waiting in real time (the "no test sleeps for real longer than 10ms"
/// discipline this project holds tests to).
pub trait TurnSleeper {
    fn sleep(&self, ms: u64);
}

/// The production [`TurnSleeper`] — an actual `std::thread::sleep`. `ms ==
/// 0` is a true no-op (no syscall at all), so the unconfigured (default)
/// path costs nothing beyond the branch that decides to skip it.
pub struct RealSleeper;

impl TurnSleeper for RealSleeper {
    fn sleep(&self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// (#2094) Clamp an operator-configured `turn_delay_ms` below the
/// inactivity timeout, and produce the loud warning to log when it does.
/// A rest AT OR ABOVE the full timeout could by itself exhaust the
/// deadline before the loop ever reaches its next proof-of-work signal, so
/// anything at or above the timeout is clamped to HALF of it — never
/// honored verbatim — rather than silently letting the operator's own
/// pacing knob become the thing that kills their dispatch.
///
/// (#2094 second round, finding 4) The band was widened from "clamp at
/// the full timeout" to "clamp at HALF the timeout"
/// (`configured_ms * 2 >= budget_ms`) — a rest at, say, 60% of the
/// timeout was previously honored verbatim, but a real turn's own
/// latency plus the trajectory tailer's 250ms poll overhead sit on top of
/// it, so an unclamped rest could still leave only a sliver of headroom
/// before the deadline. Clamping at half guarantees at least half the
/// budget remains for everything else.
///
/// `budget_ms == 0` is a degenerate operator setting (an effectively
/// disabled watchdog) — never clamp against it: half of zero is zero,
/// which would silently erase an intentional rest rather than protect
/// anything. Pure + testable.
fn resolve_turn_delay_ms(configured_ms: u64, budget_secs: u64) -> (u64, Option<String>) {
    let budget_ms = budget_secs.saturating_mul(1000);
    if budget_ms == 0 || configured_ms.saturating_mul(2) < budget_ms {
        return (configured_ms, None);
    }
    let clamped = budget_ms / 2;
    let warning = format!(
        "darkmux-runtime: ⚠ turn_delay_ms={configured_ms} is at or above half the inactivity \
         timeout ({budget_ms}ms) — clamping to {clamped}ms (half the timeout) so the \
         configured rest, plus the tailer's own polling overhead, can never approach the \
         watchdog's deadline. (#2094)"
    );
    (clamped, Some(warning))
}

/// (#2094) Extend `deadline` forward by `rest_ms` — the runtime-side
/// soft-inactivity clock is EXTENDED by a harness-owned rest, never reset
/// to "just now" (that would grant more headroom than the rest actually
/// cost) and never left untouched (that would let the rest silently
/// consume inactivity budget as if the dispatch had gone quiet). Pure +
/// testable; mirrors `resolve_turn_delay_ms`'s shape.
/// A reported token count as the loop's `u32` counters hold it.
fn saturating_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// (#3013) Announce the re-read loop escalation: the operator-visible line
/// and the `EscalationTriggered` trajectory record. The caller returns the
/// `LoopOutcome`, carrying every banked turn out as the other compaction
/// bounds do.
fn announce_reread_loop(
    trajectory: &mut Trajectory,
    turns: u32,
    repeats: u32,
    model: &str,
    latest_prompt_tokens: u32,
) {
    eprintln!(
        "darkmux-runtime: {repeats} compactions in a row were followed by a turn that \
         re-read exactly what the turn before the compaction read. Each compaction \
         succeeds and each re-read refills the context, so the run cannot converge. \
         ESCALATING to the frontier rather than repeating the loop. (#3013)"
    );
    trajectory.append_escalation_triggered(
        turns,
        escalation_reason_str(EscalationReason::CompactionRereadLoop),
        model,
        latest_prompt_tokens,
    );
}

/// How the per-call-budget line names a turn's completion tokens: the
/// endpoint's count, else the runtime's estimate marked as one, else the
/// plain statement that the server reported none.
fn completion_tokens_label(reported: Option<u32>, estimate: Option<u32>) -> String {
    match (reported, estimate) {
        (Some(n), _) => format!("{n} completion tokens"),
        (None, Some(n)) => format!("~{n} completion tokens (the runtime's estimate; the server reported none)"),
        (None, None) => "an unknown number of completion tokens (not reported by the server)".to_string(),
    }
}

fn extend_deadline_by_rest(deadline: std::time::Instant, rest_ms: u64) -> std::time::Instant {
    deadline + std::time::Duration::from_millis(rest_ms)
}

/// (#2094 finding 3b) The soft-inactivity clock's COMPLETE reaction to a
/// fired rest — both effects the call site (the guarded rest block inside
/// `run_with_sleeper`'s loop) must apply together: extend the deadline
/// (via [`extend_deadline_by_rest`]) AND clear the edge-trigger warning
/// flag, since a fresh rest buys a fresh chance before the next soft
/// warning fires. Bundled into one function — rather than leaving the
/// call site to invoke `extend_deadline_by_rest` and reset the flag as
/// two separate statements — so the CALL SITE's wiring is pinned by a
/// single, directly-testable seam: a mutation that deletes the call to
/// this function is a one-line diff at the call site, not two lines that
/// could be half-deleted and half-missed.
fn absorb_rest_into_soft_inactivity_clock(
    last_proof_of_work: std::time::Instant,
    rest_ms: u64,
) -> (std::time::Instant, bool) {
    (extend_deadline_by_rest(last_proof_of_work, rest_ms), false)
}

/// (#2774 tier 2) The host-set turn-delay half of the pace file's THIRD
/// state — a duty-cycle instruction (`pause: false, turn_delay_ms:
/// Some(ms)`) rather than a full pause. Reads the pace file ONCE (its
/// caller, [`honor_pace_pause`], reads it again right after for the
/// pause branch — see that function's own doc for why this is a
/// deliberate second read rather than a shared one) and, if the file is
/// carrying a live, non-expired, non-zero `turn_delay_ms`, sleeps that
/// duration exactly once — through the SAME `resolve_turn_delay_ms`
/// budget clamp the operator's own configured `turn_delay_ms` goes
/// through (#2094), never around it — then records it as a rest through
/// the SAME `absorb_rest_into_soft_inactivity_clock` path every other
/// harness-owned rest uses, so a duty-cycled turn never looks like a
/// stall to the inactivity watchdog.
///
/// A `pause: true` file is left entirely alone here (nothing to duty-cycle
/// — the caller's while-loop handles the full pause), and staleness is
/// judged by the SAME `written_at_ms`/`max_pause_ms` heartbeat contract
/// `pause` uses — [`pace::PaceReader::pause_is_expired`], the same method
/// the pause path calls, not the raw [`pace::PaceFile::is_expired`]
/// underneath it. There is no separate rule for this field, matching the
/// module doc's "no per-reason opt-out."
///
/// (#2774 round-9 MF2) That distinction is the whole of a real defect.
/// The raw fn computes `now_ms.saturating_sub(written_at)`, which clamps
/// to `0` for a FUTURE-dated stamp — "maximally fresh, forever, as long
/// as the SAME future-dated stamp sits there unchanged", in `pace.rs`'s
/// own words. `pause_is_expired` exists precisely to add the
/// one-grace-interval guard over that, and it had been wired to ONE of
/// its two call sites. Measured on the raw fn: a pace file stamped ~10^10
/// ms in the future, read five times, slept the full 15,000ms every time
/// with no decay. The container's Docker VM clock running ahead of the
/// host is a skew direction `pace.rs` documents as real and expected, so
/// a long crawl could throttle at every turn boundary for the rest of the
/// dispatch with the governor dead and unable to re-stamp it away.
///
/// Sharing the reader's future-skew grace with the pause path is safe
/// because the two branches are mutually exclusive on any one tick: this
/// function returns before its staleness check whenever `pause` is true,
/// and the caller's poll loop breaks on `!pace.pause` before reaching its
/// own check. So at most ONE of them consults the grace per turn
/// boundary, and the deliberate double read of the pace file documented
/// on [`honor_pace_pause`] — which exists to keep these two checks from
/// interfering — is preserved, not reintroduced.
#[allow(clippy::too_many_arguments)]
fn apply_pace_duty_cycle_delay(
    pace_reader: &mut pace::PaceReader,
    out_dir: &std::path::Path,
    max_pause_ms: u64,
    inactivity_budget_secs: u64,
    sleeper: &dyn TurnSleeper,
    trajectory: &mut Trajectory,
    turns: u32,
    last_proof_of_work: &mut std::time::Instant,
    inactivity_soft_warning_fired_in_window: &mut bool,
) {
    let Some(pace) = pace_reader.read(out_dir) else { return };
    if pace.pause {
        return;
    }
    let Some(host_delay_ms) = pace.turn_delay_ms.filter(|&d| d > 0) else { return };
    if pace_reader.pause_is_expired(&pace, checkpoint::unix_ms(), max_pause_ms) {
        // Abandoned duty-cycle instruction — the writer went dark, same
        // staleness rule a pause uses, INCLUDING the stamp-in-the-future
        // guard (#2774 round-9 MF2; see this fn's own doc). Silent:
        // `honor_pace_pause`'s own expiry warning covers the "governor
        // went quiet" case for the pause path; duplicating that warning
        // here (for a NON-pause instruction that was never blocking
        // anything) would be noise.
        return;
    }
    let (delay_ms, warning) = resolve_turn_delay_ms(host_delay_ms, inactivity_budget_secs);
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    if delay_ms == 0 {
        return;
    }
    // (#2877) Recorded as the rest STARTS, carrying its planned length, so
    // a live viewer can show the rest while it happens.
    trajectory.append_paced_rest(turns, delay_ms, "thermal-duty-cycle", pace.state.as_deref());
    sleeper.sleep(delay_ms);
    (*last_proof_of_work, *inactivity_soft_warning_fired_in_window) =
        absorb_rest_into_soft_inactivity_clock(*last_proof_of_work, delay_ms);
}

/// (#2114 finding 7) The pace-file pause wait, extracted so it can be
/// called from BOTH the main loop's turn-boundary check AND the resume
/// catch-up pass (`run_with_sleeper`'s pre-loop block) — a resume into an
/// active thermal pause must not barrel through its undispatched tool
/// calls before honoring it. Blocks in ≤2s increments, re-reading the
/// pace file each increment; returns once the file says `pause: false`,
/// is absent/malformed, or has expired past `max_pause_ms` (see
/// `PaceFile::is_expired` — a stale stamp is treated as abandoned).
///
/// (#2774 tier 2) Calls [`apply_pace_duty_cycle_delay`] FIRST, as a
/// prelude — that function does its own, SEPARATE read of the pace file
/// rather than sharing this one, deliberately: a shared read would mean
/// this function's `pause_is_expired` (which mutates the reader's
/// future-skew-grace tracking, see that method's own doc) and the
/// prelude's staleness check could consume the SAME grace interval twice
/// on one tick. Reading twice costs one extra `fs::read_to_string` per
/// turn boundary — cheap, and it keeps the two staleness checks from ever
/// interfering with each other. The prelude is a no-op whenever the pace
/// file is absent, paused, or carries no `turn_delay_ms`, which covers
/// every dispatch that has never seen a duty-cycle instruction.
#[allow(clippy::too_many_arguments)]
fn honor_pace_pause(
    pace_reader: &mut pace::PaceReader,
    out_dir: &std::path::Path,
    max_pause_ms: u64,
    inactivity_budget_secs: u64,
    pace_expiry_warned: &mut bool,
    sleeper: &dyn TurnSleeper,
    trajectory: &mut Trajectory,
    turns: u32,
    last_proof_of_work: &mut std::time::Instant,
    inactivity_soft_warning_fired_in_window: &mut bool,
) {
    apply_pace_duty_cycle_delay(
        pace_reader,
        out_dir,
        max_pause_ms,
        inactivity_budget_secs,
        sleeper,
        trajectory,
        turns,
        last_proof_of_work,
        inactivity_soft_warning_fired_in_window,
    );

    const PACE_POLL_INCREMENT_MS: u64 = 2_000;
    while let Some(pace) = pace_reader.read(out_dir) {
        if !pace.pause {
            *pace_expiry_warned = false;
            break;
        }
        if pace_reader.pause_is_expired(&pace, checkpoint::unix_ms(), max_pause_ms) {
            if !*pace_expiry_warned {
                eprintln!(
                    "darkmux-runtime: ⚠ pace file at {} has been paused past \
                     max_pause_ms ({max_pause_ms}ms) — treating the pause as \
                     abandoned and continuing. (#2114)",
                    pace::pace_file_path(out_dir).display()
                );
                *pace_expiry_warned = true;
            }
            break;
        }
        let reason = pace.reason_or_default();
        // (#2877) Recorded as each increment starts; see the duty-cycle rest.
        trajectory.append_paced_rest(turns, PACE_POLL_INCREMENT_MS, &reason, pace.state.as_deref());
        sleeper.sleep(PACE_POLL_INCREMENT_MS);
        (*last_proof_of_work, *inactivity_soft_warning_fired_in_window) =
            absorb_rest_into_soft_inactivity_clock(*last_proof_of_work, PACE_POLL_INCREMENT_MS);
    }
}

/// (#2114) Whether `elapsed_secs` since the last proof-of-work reset looks
/// like a suspected host sleep/wake rather than a genuine stall: more than
/// 2x the FULL inactivity budget elapsed in a single top-of-loop check. A
/// live, responsive loop's soft-deadline check runs every iteration, so a
/// real stall would already have crossed the (smaller) soft threshold and
/// fired the warning well before reaching 2x the full budget — a jump
/// straight past that line without an intervening soft warning is the
/// signature of the process having been paused by something OUTSIDE the
/// loop's own control (a host suspend pausing the Docker VM), not the loop
/// itself going quiet. `inactivity_budget_secs == 0` never counts as a
/// jump (an unbounded budget has no "2x" to exceed).
fn is_suspected_sleep_wake_jump(elapsed_secs: u64, inactivity_budget_secs: u64) -> bool {
    inactivity_budget_secs > 0 && elapsed_secs > inactivity_budget_secs.saturating_mul(2)
}

/// (#1221) The deliverable must be TEXT, never markup.
///
/// A model handed a closed thought can still re-open one, and that scratch
/// work must not become the answer. ANCHORED deliberately: this engages only
/// when the text LEADS with an opener. An answer that merely quotes the
/// delimiter mid-sentence — which is exactly what a reviewer of this file
/// writes — is handed over verbatim. The unanchored version of this check is
/// the same bug class as the `rfind("</think>")` that truncated a quoting
/// answer, and it is not worth reintroducing to tidy up markup that a real
/// continuation never emits.
fn as_deliverable_text(s: &str) -> String {
    let t = s.trim_start();
    let leads_with_markup = t.starts_with(crate::budget_request::THINK_OPEN.trim());
    // A never-closed thought becomes the deliverable (see `deliverable`), and
    // an inline-think family's accumulation carries its own opener wherever the
    // first slice put it. Strip when the text LEADS with markup or when it
    // carries an UNMATCHED opener — an answer that merely quotes the delimiter
    // in passing quotes it in balance or not at all, and keeps its text.
    let unmatched_opener = t.matches(crate::budget_request::THINK_OPEN.trim()).count()
        > t.matches(crate::budget_request::THINK_CLOSE.trim()).count();
    if !leads_with_markup && !unmatched_opener {
        return s.to_string();
    }
    // Markup-led: strip EVERY delimiter, not just the leading one. Text
    // accumulated across checkpoints can carry more than one opener, and
    // half-stripped markup is the worst of both.
    t.replace(crate::budget_request::THINK_OPEN, "")
        .replace(crate::budget_request::THINK_OPEN.trim_end(), "")
        .replace(crate::budget_request::THINK_CLOSE, "")
        .replace(crate::budget_request::THINK_CLOSE.trim(), "")
        .trim()
        .to_string()
}

/// (#1221) Everything the loop knows about the turn currently in flight: its
/// two output regions, and the prefill message that carries them back to the
/// model.
///
/// **This type exists because the state machine was written as loose
/// variables first, and that cost two shipped defects.** Message lifetime and
/// region lifetime are ONE lifetime, but they were managed by six `let mut`s
/// mutated at seven sites, so it was possible — and it happened twice — to
/// clear the index while leaving the message it pointed at. An orphaned
/// prefill is not a cosmetic mess: nothing downstream can reconstruct the
/// answer from it, so `main.rs` hands raw `<think>` markup over as the
/// deliverable.
///
/// So every transition that touches the prefill takes `&mut Vec<Message>` and
/// does both halves. There is no method that clears the state without removing
/// the message, which is what makes the leak unrepresentable rather than
/// merely avoided.
///
/// The whole lifecycle, and nothing outside these four methods may move it:
///
/// ```text
///   begin()    a new logical turn starts — abandon anything live, reset
///   absorb()   a slice arrives at the boundary — route it into a region
///   hand_back()  a checkpoint — replace the prefill with the whole
///                accumulation so the model RESUMES instead of restarting
///   fold()     a terminal finish — the answer region becomes the deliverable
///   abandon()  recovery — the message and the state go together
/// ```
#[derive(Default)]
struct TurnAccum {
    /// The reasoning region: everything inside this turn's think block.
    thought: String,
    /// The answer region: everything the model has committed as its answer.
    answer: String,
    /// The thought's closing delimiter has been written into the prefill.
    /// Once closed it STAYS closed — re-opening a think block around an answer
    /// tells the model its answer was scratch work.
    think_closed: bool,
    /// This turn wrote reasoning at all. A turn that writes a long ANSWER and
    /// never reasons hits the interval exactly like a thinking turn does, and
    /// wrapping that answer in `<think>` is the category error prefill
    /// continuation exists to avoid.
    is_reasoning: bool,
    /// The accumulated thought already begins with the model's own `<think>`
    /// (an inline-think family's raw content), so the prefill must not add a
    /// second one.
    carries_own_opener: bool,
    /// Where this turn's prefill sits in `messages`, while one is live.
    prefill_at: Option<usize>,
}

impl TurnAccum {
    /// A new logical turn. Any prefill still live belonged to the PREVIOUS
    /// turn and must go with it — this is the transition that used to clear
    /// the index and leave the message, which is the whole reason this type
    /// exists.
    fn begin(&mut self, messages: &mut Vec<Message>) {
        self.abandon(messages);
    }

    /// Route a slice into the region it belongs to. The ONLY place the regions
    /// grow.
    fn absorb(&mut self, reasoning: &str, content: &str) {
        // Once the thought is closed, everything that follows is the answer —
        // INCLUDING text that itself contains `<think>` markup. Testing the
        // inline delimiters first sent post-close slices back into the
        // thought, so a concluded turn never accumulated an answer at all: the
        // gate then read an empty answer region, decided the call had produced
        // nothing, and dropped a turn that had produced plenty.
        // The model may close the block ITSELF, and on the primary dispatch
        // surface it is free to. The premise this region machine was built on —
        // "under `response_format` the model CANNOT emit `</think>`" — is TRUE
        // and was measured, but it only covers schema-constrained roles:
        // 17 of the 29 built-in roles declare no `output_schema`, including
        // `coder`, `code-reviewer` and `analyst`. For those the inline qwen-3.x
        // family emits its own closer, and nothing here used to watch for it.
        //
        // The cost was measured twice, independently. A live 66-call analyst
        // dispatch generated 26,181 completion tokens and delivered 1,116
        // characters; a review probe reproduced the same shape and got a
        // deliverable of `" ANSWER-PART-TWO and that is all."`. In both, the
        // answer sat in the THOUGHT region because the close that separated
        // them was read as ordinary thought text.
        //
        // This is NOT the `rfind("</think>")` that was removed. That one
        // searched the whole ACCUMULATION for the LAST occurrence and
        // TRUNCATED at it, so an answer quoting the delimiter lost its tail.
        // This splits THIS slice at the FIRST close and keeps BOTH halves —
        // nothing is discarded, so a quoted delimiter costs a misfiled
        // sentence rather than a deleted answer.
        if !self.think_closed {
            if let Some(at) = content.find(crate::budget_request::THINK_CLOSE.trim()) {
                let (before, after) = content.split_at(at);
                let after = &after[crate::budget_request::THINK_CLOSE.trim().len()..];
                self.is_reasoning = true;
                if self.thought.is_empty() && before.trim_start().starts_with(crate::budget_request::THINK_OPEN.trim()) {
                    self.carries_own_opener = true;
                }
                self.thought.push_str(reasoning);
                self.thought.push_str(before);
                self.think_closed = true;
                self.answer.push_str(after);
                return;
            }
        }
        if self.think_closed {
            // Reasoning that arrives AFTER the close is still reasoning: it
            // belongs inside the block, not in the deliverable. Appending it to
            // the thought keeps it carried back (so the model does not
            // re-derive it next call) while `prefill_body` still emits the
            // closing delimiter after it, so the block stays closed. Dropping
            // it silently — which this did — is the discard-the-work bug in
            // miniature. Rare in practice: once darkmux supplies the opener the
            // provider stops tagging continuations as reasoning (measured: 13
            // API calls, exactly one `model.reasoning` event), so this is the
            // shape that shows up on a family that keeps tagging.
            self.thought.push_str(reasoning);
            self.answer.push_str(content);
            return;
        }
        // An INLINE-think model (the qwen 3.x line) cut mid-reasoning leaves an
        // UNCLOSED `<think>`, and `extract_think_blocks` deliberately bails on
        // those — so `reasoning_content` is empty for exactly the shape this
        // feature exists to handle. Detect it from the delimiters instead.
        //
        // Anchored at the START, not counted anywhere in the string. An
        // unanchored `opens > closes` misclassifies any answer that quotes the
        // opening delimiter as reasoning; a real inline-think turn LEADS with
        // it. On a continuation the model resumes inside the block darkmux
        // handed back and emits no opener at all, which falls through to the
        // continuing-a-thought branch below.
        let trimmed = content.trim_start();
        let opener = crate::budget_request::THINK_OPEN.trim();
        let closer = crate::budget_request::THINK_CLOSE.trim();
        if trimmed.starts_with(opener) && trimmed.matches(opener).count() > trimmed.matches(closer).count() {
            self.is_reasoning = true;
            // Only the FIRST slice decides whether the accumulation carries its
            // own opener, because the flag governs whether a `<think>` is
            // prefixed to the WHOLE thought. Setting it unconditionally let a
            // later inline slice delete the opener from an accumulation that
            // began as `reasoning_content` and needed one.
            if self.thought.is_empty() {
                self.carries_own_opener = true;
            }
            self.thought.push_str(content);
            return;
        }
        if !reasoning.trim().is_empty() {
            self.is_reasoning = true;
            self.thought.push_str(reasoning);
            // BOTH fields present means the model finished thinking and had
            // begun answering when the boundary hit. Discarding `content` here
            // silently deleted committed text on the modal thinking-model
            // shape.
            if !content.is_empty() {
                self.think_closed = true;
                self.answer.push_str(content);
            }
            return;
        }
        if self.is_reasoning {
            // Continuing a thought darkmux opened. After a prefill the provider
            // stops tagging the continuation as reasoning — we supplied the
            // opener, so it comes back as ordinary content. Measured: 13 API
            // calls produced exactly ONE `model.reasoning` event.
            self.thought.push_str(content);
        } else {
            self.answer.push_str(content);
        }
    }

    /// Which bound the NEXT call carries. A turn starts in the reasoning region
    /// (we cannot know whether it will think until it answers, and sampling
    /// finely is the cheap mistake) and moves to the answer region once a
    /// checkpoint shows the output is plain content, or once a degeneracy
    /// verdict has closed the thought.
    ///
    /// (#2164) `false` here is ALSO what a brand-new, nothing-absorbed-yet
    /// turn returns — this function cannot, by itself, tell "mid-thought
    /// continuation" apart from "turn just began, unproven". That
    /// distinction is NOT made inside `TurnAccum`: `absorb()` (and the
    /// `is_reasoning` it sets) only ever runs for a turn that has already
    /// been checkpointed at least once, so a turn that completes cleanly in
    /// ONE call never touches this struct's reasoning state at all — the
    /// dispatch-scoped "has this model ever reasoned" signal the per-call-cap
    /// decision needs lives in the caller (`dispatch_has_reasoned`), derived
    /// straight from each response's `per_turn_reasoning`, not from here.
    fn in_answer_region(&self) -> bool {
        self.think_closed || (!self.is_reasoning && !self.answer.is_empty())
    }

    /// Whether the region currently being written is the thought.
    fn writing_thought(&self) -> bool {
        self.is_reasoning && !self.think_closed
    }

    /// The region the degeneracy gate should judge — whichever one is being
    /// written. Judging the thought unconditionally left a non-reasoning turn
    /// measuring an empty string, so degeneracy could never fire and a
    /// repeating answer spun forever.
    ///
    /// (#2836) **Falls back to the other region when the chosen one is
    /// empty**, because that same failure had a mirror image nobody had hit
    /// yet. A model that reasons exclusively through `reasoning_content`
    /// earns one honest degenerate verdict; the remedy is `close_thought()`,
    /// which flips the judged region to the ANSWER — and `absorb` keeps
    /// routing reasoning to the thought, so the answer never fills. From
    /// that point the judge read `""` and returned `continue` forever while
    /// the model reasoned on, unwatched. Measured live: six consecutive
    /// `judged_chars: 0, verdict: continue` records, then the dispatch died
    /// on an exhausted budget.
    ///
    /// An empty region does not mean "clean", it means the model is not
    /// writing there. A verdict computed over zero characters is a vacuous
    /// pass, and this function is where it was manufactured.
    fn carried(&self) -> &str {
        let (primary, fallback) = if self.writing_thought() {
            (self.thought.trim(), self.answer.trim())
        } else {
            (self.answer.trim(), self.thought.trim())
        };
        if primary.is_empty() {
            fallback
        } else {
            primary
        }
    }

    /// A degeneracy verdict: close the thought so the model answers FROM it
    /// rather than re-deriving it. Written into the accumulation, so every
    /// later checkpoint keeps handing back a closed thought plus the answer so
    /// far.
    fn close_thought(&mut self) {
        self.think_closed = true;
    }

    /// The message that goes back out: the WHOLE accumulation, assembled from
    /// the regions and never parsed back out of a blob.
    fn prefill_body(&self) -> String {
        let mut body = String::new();
        if !self.thought.is_empty() {
            // The delimiter contract lives in `budget_request` and is pinned by
            // its own tests; assembling the same bytes by hand here would be a
            // second copy that drifts. `carries_own_opener` is the one case
            // those helpers cannot express: the model's raw content already
            // OPENS the block, so prefixing a second one nests it.
            body.push_str(&if self.carries_own_opener {
                let mut t = self.thought.clone();
                if self.think_closed {
                    t.push_str(crate::budget_request::THINK_CLOSE);
                }
                t
            } else if self.think_closed {
                crate::budget_request::conclude_now_prefill(&self.thought)
            } else {
                crate::budget_request::continue_thinking_prefill(&self.thought)
            });
        }
        body.push_str(&self.answer);
        body
    }

    /// Hand the turn back as a prefill so the model RESUMES it.
    ///
    /// REPLACES the previous prefill rather than appending beside it. A live
    /// 30-checkpoint dispatch showed the cost of appending: thirty sibling
    /// assistant messages, each opening its own `<think>` around a truncated
    /// copy of the same answer. The model was not resuming a thought; it
    /// restarted the same one every call and could never converge.
    ///
    /// The prefill must remain LAST — anything appended after it ends the
    /// assistant turn and turns a continuation back into a restart.
    fn hand_back(&mut self, messages: &mut Vec<Message>) {
        let body = self.prefill_body();
        self.remove_prefill(messages);
        messages.push(Message::assistant_prefill(body));
        self.prefill_at = Some(messages.len() - 1);
    }

    /// A terminal finish. The deliverable is the ANSWER region plus whatever
    /// this final call added — assembled, never recovered by searching for a
    /// delimiter.
    ///
    /// A concluding turn returns only the SUFFIX (a continuation carries just
    /// the new text), so without this fold the accumulated body stays orphaned
    /// in the prefill one slot earlier and `main.rs` — which takes the last
    /// assistant message — hands over the tail and nothing else. That is the
    /// MODAL path, not an edge case: most turns conclude rather than
    /// degenerate.
    fn fold(&mut self, messages: &mut Vec<Message>, message: &mut Message) {
        if self.prefill_at.is_none() {
            return;
        }
        // Route the TERMINAL slice through the same region logic as every other
        // slice. It used to bypass `absorb` entirely and be appended raw, so a
        // model that closed its own block on the last call handed its trailing
        // scratch work AND a dangling `</think>` over as the answer. A terminal
        // turn is not a different kind of output; it is the last one.
        let tail = message.content.clone().unwrap_or_default();
        self.absorb("", &tail);
        message.content = Some(self.deliverable(""));
        self.remove_prefill(messages);
        self.clear();
    }

    /// The prefill MESSAGE has been superseded by a real assistant message,
    /// but the turn is not over — remove the message, keep the accumulation.
    ///
    /// The one place message lifetime and region lifetime legitimately part.
    /// A per-turn-cap salvage produces a genuine assistant message (cleared
    /// content, recovered tool calls) that is about to be pushed. The prefill
    /// standing in for it has done its job: it carried the accumulation back to
    /// the model, and the model has now answered.
    ///
    /// Leaving it produces TWO CONSECUTIVE assistant messages, which is an
    /// invalid conversation shape — measured: the next request returned HTTP
    /// 500. Folding instead ends the turn early and restores content that
    /// salvage deliberately cleared. Neither is right; the prefill simply needs
    /// to go while the regions stay, so the accumulation returns at the next
    /// checkpoint.
    fn supersede(&mut self, messages: &mut Vec<Message>) {
        self.remove_prefill(messages);
    }

    /// Give up on this turn: the message and the state go together. Used by the
    /// recovery path and by `begin`.
    fn abandon(&mut self, messages: &mut Vec<Message>) {
        self.remove_prefill(messages);
        self.clear();
    }

    /// The answer this turn would hand over if the run ended right now, and
    /// only while a prefill is still live — once folded, the deliverable is
    /// already the last message and there is nothing to override.
    fn pending_answer(&self) -> Option<String> {
        self.prefill_at?;
        let d = self.deliverable("");
        if d.trim().is_empty() {
            None
        } else {
            Some(d)
        }
    }

    /// What this turn hands the operator, given whatever the final call added.
    ///
    /// ONE rule, shared by `fold` and `pending_answer`, because they disagreed
    /// and that meant the SAME run produced a different deliverable depending
    /// on whether it ended on `stop` or on a cap.
    ///
    /// The answer region when the thought was CLOSED — then we can tell scratch
    /// from answer, and the scratch stays out. When it was NEVER closed we
    /// cannot, and the accumulation itself is the deliverable. That is not an
    /// edge case: measured on a live 66-call dispatch, the provider tagged
    /// reasoning on call 1 only, so every later call arrived as untagged
    /// content and was classified as more thought. The answer region was empty
    /// for the whole turn — 26,181 completion tokens generated, 1,116
    /// characters delivered, starting mid-sentence. Handing over nothing, or
    /// only the last slice, is the discard-the-turn bug this feature exists to
    /// end.
    fn deliverable(&self, tail: &str) -> String {
        let mut out = String::new();
        if !self.think_closed && !self.thought.trim().is_empty() {
            out.push_str(&self.thought);
        }
        out.push_str(&self.answer);
        out.push_str(tail);
        as_deliverable_text(&out)
    }

    /// Whether a prefill is live — i.e. whether this turn has work banked.
    fn has_prefill(&self) -> bool {
        self.prefill_at.is_some()
    }

    /// Private: the two halves that must never be done separately.
    fn remove_prefill(&mut self, messages: &mut Vec<Message>) {
        if let Some(i) = self.prefill_at.take() {
            if i < messages.len() {
                messages.remove(i);
            }
        }
    }

    fn clear(&mut self) {
        self.thought.clear();
        self.answer.clear();
        self.think_closed = false;
        self.is_reasoning = false;
        self.carries_own_opener = false;
        self.prefill_at = None;
    }
}


/// (#2165, revised #2171) Which bound governed the request that just came
/// back — derived from `sent_reasoning_bound`/`sent_generation_bound`/
/// `per_call_cap`, the values the cap-selection block already resolves
/// before every request. Emission sites downstream (salvage, intra-turn-
/// stall recovery, checkpoint continuation) call this instead of
/// re-deriving the region, so the answer can never disagree with what was
/// actually sent.
///
/// THREE-way, not two: #2171 added a GENERATION check-in
/// (`generation_checkpoint_interval_tokens`) that bounds any call NOT
/// carrying the reasoning bound, tighter than the raw answer bound
/// (`max_tokens_per_call`). `sent_reasoning_bound` and
/// `sent_generation_bound` are mutually exclusive by construction at the
/// cap-selection site (the same `if/else if/else` that picks `per_call_cap`
/// picks these two flags), so checking reasoning first, then generation,
/// then falling through to the raw answer bound reproduces exactly the
/// priority the cap-selection block already applied.
///
/// (#2836 stage 1) **Which bound a cut names depends on which bound cut it**,
/// and before this the two could not differ so nothing had to ask. Sending
/// the check-in interval as `max_tokens` made the observation interval and
/// the wire ceiling the same number; now a streamed call carries the
/// ceiling and the interval only governs when the runtime LOOKS. A cut can
/// therefore come from either, and they are different knobs with different
/// values and different provenance.
///
/// Found by a live run, not by the suite: a call that ran to the 10,000
/// token ceiling recorded `bound=reasoning_checkpoint_interval/1000` beside
/// `slice_tokens=10000`. The operator reading that record would tune the
/// check-in interval to fix a ceiling problem. `cut_bound` below routes on
/// [`CutSource`] so the record names the knob that actually acted.
fn active_bound(sent_reasoning_bound: bool, sent_generation_bound: bool, per_call_cap: u32) -> BoundRef {
    let sources = bounds::bound_sources();
    if sent_reasoning_bound {
        BoundRef::new(BoundKind::ReasoningCheckpointInterval, per_call_cap as u64, sources.reasoning_checkpoint_interval)
    } else if sent_generation_bound {
        BoundRef::new(BoundKind::GenerationCheckpointInterval, per_call_cap as u64, sources.generation_checkpoint_interval)
    } else {
        BoundRef::new(BoundKind::MaxTokensPerCall, per_call_cap as u64, sources.max_tokens_per_call)
    }
}

/// (#2836 stage 1) The bound that ACTUALLY ended this call.
///
/// A runtime abort is the observation interval doing its job, so it names
/// the check-in knob via [`active_bound`]. A server `length` finish is the
/// wire ceiling, so it names `max_tokens_per_call` and carries the ceiling's
/// value. On the non-streamed path the two are the same number and this
/// collapses to the old behavior, which is why no existing record changes.
fn cut_bound(
    cut: CutSource,
    sent_reasoning_bound: bool,
    sent_generation_bound: bool,
    per_call_cap: u32,
    wire_max_tokens: u32,
) -> BoundRef {
    // The runtime's own abort IS the observation interval acting.
    if matches!(cut, CutSource::RuntimeAbort(_)) {
        return active_bound(sent_reasoning_bound, sent_generation_bound, per_call_cap);
    }
    // Otherwise the SERVER stopped generating, and which knob that was
    // depends on which number the wire carried. On the non-streamed path
    // the wire still carries the check-in interval, so a `length` finish
    // there is the check-in firing exactly as it always did — this must
    // stay identical to pre-Stage-1 behavior, and seven regression tests
    // say so. Only when the wire carries the ceiling instead (the streamed
    // path) is a server cut a ceiling hit.
    if wire_max_tokens == per_call_cap {
        return active_bound(sent_reasoning_bound, sent_generation_bound, per_call_cap);
    }
    BoundRef::new(
        BoundKind::MaxTokensPerCall,
        wire_max_tokens as u64,
        bounds::bound_sources().max_tokens_per_call,
    )
}

/// (#1221/#1123) The mechanics both non-checkpointable shapes share: drop the
/// unusable response, spend one unit of the bounded recovery budget, record it,
/// and nudge.
///
/// What the two callers do NOT share is the accumulation. An EMPTY completion
/// says nothing about the work already banked, so that path keeps its prefill
/// and stays on the same turn — discarding five productive checkpoints because
/// the sixth call came back blank is precisely the bug this feature exists to
/// end. A DEGENERATE accumulation is different: it is proven to be repeating,
/// and handing it back guarantees more of it, so that path abandons it first.
#[allow(clippy::too_many_arguments)]
fn recover_intra_turn_stall(
    messages: &mut Vec<Message>,
    trajectory: &mut Trajectory,
    turns: u32,
    completion_tokens: Option<u32>,
    stall_recoveries_used: &mut u32,
    // (#2190) The resolved budget (operator override, or MAX_STALL_RECOVERIES)
    // — threaded in rather than read from the constant, so this shared
    // recovery path honors `runtime.max_stall_recoveries` the same as every
    // other call site.
    stall_recovery_budget: u32,
    nudge: &str,
    bound: BoundRef,
) {
    messages.pop();
    *stall_recoveries_used = stall_recoveries_used.saturating_add(1);
    trajectory.append_intra_turn_stall_recovered(
        turns,
        completion_tokens,
        *stall_recoveries_used,
        stall_recovery_budget,
        bound,
    );
    messages.push(Message::system(nudge));
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
/// (#2094) Thin wrapper over [`run_with_sleeper`] — constructs the real
/// [`RealSleeper`], defaults the workspace root to `/workspace`, and
/// resumes nothing, so every pre-#2114 call site (35+ at the time #2094
/// landed, all tests) keeps this exact signature and needs no change.
/// Tests that want to assert the turn-delay rest's exact call
/// count/duration call [`run_with_sleeper`] directly with a recording
/// sleeper instead.
///
/// (#2114) No longer `main.rs`'s entry point — production now calls
/// [`run_resumable`], which takes the out-dir root and an optional
/// checkpoint explicitly. This one is `#[cfg(test)]` because nothing in
/// the non-test binary calls it anymore, but the bulk of this file's test
/// suite still calls it and shouldn't have to care about a feature it
/// isn't testing.
///
/// (#2114 finding 3) Each call gets its OWN fresh host tempdir as its
/// out-dir rather than a hardcoded path: this fn only ever runs on the
/// TEST host (never inside the container `main.rs` drives), where neither
/// `/darkmux-out` nor the old `/workspace` exist as writable paths. Before
/// this, every turn-boundary checkpoint write inside a `run()`-based test
/// failed and logged — 1,230 "failed to write checkpoint" lines across the
/// 124 tests that exercise this wrapper, noise that could mask a real
/// failure. Hand-rolled with a PID+nanos+counter suffix so parallel test
/// threads (same PID) never collide.
///
/// (#2707) This paragraph used to end "the dir is left behind in the OS
/// temp root rather than cleaned up, same tradeoff `dispatch_internal.rs`'s
/// `host_out` makes" — which stopped being true when #2114 finding 8 added
/// the `remove_dir_all` at the bottom of this function, and a comment that
/// contradicts the code six screens below it is the kind of artifact a
/// reader trusts by mistake. The dir IS removed, on both the Ok and the Err
/// path. The comparison to `host_out` was also the wrong one: a real
/// dispatch's out-dir is kept ON PURPOSE (it holds the run's prompt,
/// trajectory and checkpoint for the operator to read afterward), so it is
/// not a tradeoff this test-only wrapper shares.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn run(
    client: &LmStudioClient,
    compactor_client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
    streaming: bool,
    compaction_cfg: &compaction::CompactionConfig,
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u32>,
    max_tokens_per_call: Option<u32>,
    reasoning_checkpoint_interval: Option<u32>,
    feedback_templates: std::collections::BTreeMap<String, String>,
    response_format: Option<serde_json::Value>,
) -> Result<LoopOutcome> {
    static TEST_OUT_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = TEST_OUT_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out_dir = std::env::temp_dir().join(format!(
        "darkmux-runtime-test-out-{}-{nanos}-{n}",
        std::process::id()
    ));
    let result = run_with_sleeper(
        client,
        compactor_client,
        model,
        initial_messages,
        tools,
        trajectory,
        streaming,
        compaction_cfg,
        max_turns,
        max_cumulative_tokens,
        max_tokens_per_call,
        reasoning_checkpoint_interval,
        // (#2171) `run()` is the test-only convenience wrapper — its own doc
        // above already keeps its signature frozen against unrelated params
        // (out_dir/resume_from). The generation check-in is the same shape:
        // a neutral `u32::MAX` here means `min(answer_max_tokens, MAX) ==
        // answer_max_tokens`, so every one of this wrapper's 120+ existing
        // call sites keeps its pre-#2171 per-call-cap behavior unchanged. A
        // test that wants to exercise the generation check-in itself calls
        // `run_with_sleeper` directly (see the `_generation_checkpoint_*`
        // tests) the same way the resume/pace tests already do for
        // out_dir/resume_from.
        Some(u32::MAX),
        // (#2190) `None` = the built-in `MAX_STALL_RECOVERIES` (2) — this
        // wrapper's frozen signature (see its own doc above) carries no
        // param for the new knob; a test exercising the override calls
        // `run_with_sleeper` directly, same pattern as the generation
        // check-in override above.
        None,
        feedback_templates,
        response_format,
        &out_dir,
        // (v3 checkpoint schema, security audit) `run()` has no role
        // concept of its own (see this fn's own doc — its 35+ callers
        // never touch resume) — a fixed literal is fine since it never
        // resumes (`None` below) and this test-only path's own
        // checkpoint-write assertions don't inspect `role_id`.
        "test-role",
        None,
        &RealSleeper,
    );
    // (#2114 finding 8) Best-effort cleanup -- without this, 124+ test runs
    // each leave a tempdir behind (checkpoint.json carries the FULL
    // conversation), so a long-lived dev machine accumulates hundreds of
    // stale dirs with real transcript content in $TMPDIR. Runs regardless
    // of Ok/Err so a failing test doesn't skip it.
    let _ = std::fs::remove_dir_all(&out_dir);
    result
}

/// (#3074) Mark `call` started on disk, then run it. The marker is written
/// first so a kill while the tool runs leaves a checkpoint a resume can read
/// (`checkpoint::ToolStart`). `dispatcher` is `tools::dispatch` in production;
/// a test passes one that looks at the checkpoint mid-call.
fn dispatch_marked(
    start: &checkpoint::ToolStart<'_>,
    call: &ToolCall,
    dispatcher: impl FnOnce(&str, &str) -> crate::tools::ToolRun,
) -> crate::tools::ToolRun {
    start.write();
    dispatcher(&call.function.name, &call.function.arguments)
}

/// (#3074) What a resume's catch-up pass did with one call. `interrupted` means
/// the call was reported to the model instead of run, so it is NOT a tool
/// success: it is recorded as an outcome of its own and proves no work.
struct CaughtUp {
    run: crate::tools::ToolRun,
    interrupted: bool,
}

/// (#3074) The failure reason a surfaced-not-run call carries on its
/// `tool.completed` record.
const INTERRUPTED_NOT_RERUN: &str = "interrupted before the kill; not re-run on resume";

impl CaughtUp {
    /// A call that never ran is `Failed` ("did not run", `ok: false`), which
    /// also keeps it out of the inactivity timer's proof-of-work.
    fn outcome(&self, tool_name: &str) -> crate::failure_rate::ToolOutcome {
        if self.interrupted {
            crate::failure_rate::ToolOutcome::Failed { reason: INTERRUPTED_NOT_RERUN.to_string() }
        } else {
            crate::failure_rate::classify_outcome(tool_name, &self.run.result)
        }
    }
}

/// (#3074) One call of a resume's catch-up pass: surface it to the model if the
/// checkpoint says it had already started, otherwise mark it and dispatch it.
fn catch_up_dispatch(
    seed: Option<&checkpoint::RunCheckpoint>,
    idx: usize,
    start: &checkpoint::ToolStart<'_>,
    call: &ToolCall,
    dispatcher: impl FnOnce(&str, &str) -> crate::tools::ToolRun,
) -> CaughtUp {
    match seed.and_then(|c| checkpoint::interrupted_call_notice(c, idx, call)) {
        Some(notice) => CaughtUp { run: crate::tools::ToolRun::text(notice), interrupted: true },
        None => CaughtUp { run: dispatch_marked(start, call, dispatcher), interrupted: false },
    }
}

// (#3074) Test seam for the LIVE loop's call site: a thread-local observer
// runs at the moment a tool would execute, then the real dispatcher does.
#[cfg(test)]
thread_local! {
    static DISPATCH_OBSERVER: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn observed_dispatch(name: &str, raw_args: &str) -> crate::tools::ToolRun {
    DISPATCH_OBSERVER.with(|o| {
        if let Some(observer) = o.borrow().as_ref() {
            observer();
        }
    });
    crate::tools::dispatch(name, raw_args)
}

/// (#2114) Production entry point for a dispatch that may pause against a
/// host-driven pace file and/or resume a prior checkpoint. Kept SEPARATE
/// from [`run`] rather than adding these two params there: `run`'s
/// pre-#2114 signature has 35+ call sites (mostly tests exercising
/// unrelated behavior — compaction, feedback injection, cycle detection —
/// that have no reason to learn about pace files or checkpoints), and
/// every one of them would otherwise need a `Path::new(trajectory::
/// RUNTIME_OUT_BASE), None,` tacked onto its argument list for a feature
/// it doesn't touch. `main.rs` (the only real caller that needs a
/// non-default out-dir root or a resume) calls this one instead.
#[allow(clippy::too_many_arguments)]
pub fn run_resumable(
    client: &LmStudioClient,
    compactor_client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
    streaming: bool,
    compaction_cfg: &compaction::CompactionConfig,
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u32>,
    max_tokens_per_call: Option<u32>,
    reasoning_checkpoint_interval: Option<u32>,
    // (#2171) The GENERATION check-in — bounds every call that does NOT
    // carry the reasoning bound. None = GENERATION_CHECKPOINT_INTERVAL.
    generation_checkpoint_interval: Option<u32>,
    // (#2190) Per-dispatch budget for intra-turn stall recoveries (empty
    // `tool_calls`, or a runaway-reasoning cut). None = MAX_STALL_RECOVERIES.
    max_stall_recoveries: Option<u32>,
    feedback_templates: std::collections::BTreeMap<String, String>,
    response_format: Option<serde_json::Value>,
    // (#2114 finding 3) Container out-dir root — where `pace.json` and
    // `checkpoint.json` live. Production is always `/darkmux-out`
    // (`trajectory::RUNTIME_OUT_BASE`, always mounted read-write — see
    // `dispatch_internal::apply_volume_mounts`); tests pass a tempdir.
    // Deliberately NOT `/workspace`: that mount is `:ro` for crawl-kind
    // dispatches (#1959) and, when writable, is the operator's own repo —
    // either produces a checkpoint write failure or an untracked file in
    // the operator's checkout.
    out_dir: &std::path::Path,
    // (v3 checkpoint schema, security audit) The role id THIS run is
    // dispatched as — stamped into every `checkpoint.json` write so a
    // LATER `--resume-from` can refuse, host-side, to resume a checkpoint
    // recorded under a different role. Never validated here (the runtime
    // has no concept of "which role is more permissive than which"); the
    // host does that comparison entirely (`dispatch_internal::stage_
    // resume_checkpoint`). See `checkpoint::RunCheckpoint::role_id`'s doc.
    role_id: &str,
    // (#2114) `Some` when this dispatch is resuming a prior checkpoint
    // (`--resume <path>` / `DARKMUX_RESUME_CHECKPOINT`) — `initial_messages`
    // is then IGNORED in favor of the checkpoint's own message history.
    resume_from: Option<checkpoint::RunCheckpoint>,
) -> Result<LoopOutcome> {
    run_with_sleeper(
        client,
        compactor_client,
        model,
        initial_messages,
        tools,
        trajectory,
        streaming,
        compaction_cfg,
        max_turns,
        max_cumulative_tokens,
        max_tokens_per_call,
        reasoning_checkpoint_interval,
        generation_checkpoint_interval,
        max_stall_recoveries,
        feedback_templates,
        response_format,
        out_dir,
        role_id,
        resume_from,
        &RealSleeper,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_sleeper(
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
    // (#2171) The GENERATION check-in — bounds every call that does NOT
    // carry the reasoning bound. None = GENERATION_CHECKPOINT_INTERVAL.
    generation_checkpoint_interval: Option<u32>,
    // (#2190) Per-dispatch budget for intra-turn stall recoveries — how
    // many useless turns (empty `tool_calls`, or a runaway-reasoning cut)
    // the loop tolerates before escalating. None = MAX_STALL_RECOVERIES (2).
    max_stall_recoveries: Option<u32>,
    feedback_templates: std::collections::BTreeMap<String, String>,
    // (#1038) Optional `response_format` envelope (the role's output_schema,
    // wrapped as json_schema). When set, every model turn is grammar-constrained
    // to that shape — local-model JSON malformation becomes impossible.
    response_format: Option<serde_json::Value>,
    // (#2114) See `run_resumable`'s doc on the same two params.
    out_dir: &std::path::Path,
    // (v3 checkpoint schema, security audit) See `run_resumable`'s doc on
    // the same param.
    role_id: &str,
    resume_from: Option<checkpoint::RunCheckpoint>,
    // (#2094) Injectable rest sleeper — see [`TurnSleeper`]'s own doc.
    sleeper: &dyn TurnSleeper,
) -> Result<LoopOutcome> {
    let mut agent = loop_phases::AgentLoop::new(
        loop_phases::Wiring {
            client,
            compactor_client,
            model,
            tools,
            trajectory,
            compaction_cfg,
            feedback_templates,
            response_format,
            out_dir,
            role_id,
            sleeper,
        },
        loop_deciders::Knobs {
            max_tokens_per_call,
            reasoning_checkpoint_interval,
            generation_checkpoint_interval,
            max_stall_recoveries,
            max_turns,
            max_cumulative_tokens,
        },
        streaming,
        initial_messages,
        resume_from.as_ref(),
    );
    // DETECTOR STATE DOES NOT SURVIVE A RESUME, ON PURPOSE. The detectors
    // above and below (cycle, failure rate, reasoning loop, malformed turns,
    // the unproductive-compaction and re-read counters) start empty on every
    // process, including one that resumes a checkpoint. Two facts make that
    // correct rather than a gap:
    //
    // 1. A host pause (the thermal governor's `pace.json`) does NOT end the
    //    process: the loop rests in bounded increments inside it (see
    //    `pace`), so a paused run keeps every detector's window. Only a
    //    kill followed by `--resume` restarts them, and that is an explicit
    //    act by the host or the operator, not a runaway.
    // 2. The bounds that stop a runaway are NOT detector state and DO carry
    //    across the resume: the turn count, the cumulative completion
    //    tokens (`max_cumulative_tokens`) and the compaction count
    //    (`bail_after_compactions`) are all restored from the checkpoint. A
    //    run killed and resumed every few turns still hits those.
    //
    // What a fresh window loses is the warn-only history of the last few
    // tool calls, which a checkpoint does not record. The alternative,
    // persisting the windows, would put a detector's schema in the
    // checkpoint's compatibility contract for a heuristic whose failure mode
    // is one extra warning.
    if let Some(outcome) = agent.resume_catch_up(resume_from.as_ref()) {
        return Ok(outcome);
    }
    agent.run()
}

/// (#2836 stage 1) What the in-stream observer needs to do its job.
///
/// Grouped rather than passed as two more positional arguments: the pair is
/// one idea (how often to look, and at what), and `run_streaming_turn`'s
/// argument list is already at the point where another bare `u32` beside a
/// `&str` reads as noise.
struct Watch<'a> {
    /// How often the runtime looks — the interval that used to ride out as
    /// `max_tokens` and truncate this call. It no longer reaches the
    /// endpoint.
    interval: u32,
    /// The turn's accumulation from earlier continuations, so the in-stream
    /// verdict judges the same scope the post-hoc one does.
    carried: &'a str,
    /// (#2889) How long the stream may go without a chunk before the loop
    /// wakes to say the model is still writing a tool call. Production is
    /// [`STREAM_TICK`]; a test passes milliseconds.
    tick: std::time::Duration,
}

/// (#2889) Cadence of the "still writing a tool call" event while the
/// endpoint is silent. LM Studio names a tool call immediately, then
/// generates its arguments without sending a byte and delivers them in one
/// chunk; without a tick nothing is written during that silence and the
/// viewer reads it as a stall. One second, under the host's two-second
/// heartbeat coalescing (`HEARTBEAT_MIN_INTERVAL` in
/// `crates/darkmux-crew/src/dispatch_internal.rs`), so every host window
/// has an event to forward and the cadence the viewer sees is the host's,
/// unchanged.
pub(crate) const STREAM_TICK: std::time::Duration = std::time::Duration::from_secs(1);

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
    // (#2114 finding 5) Mirrors the host watchdog's #1222 shakedown-3
    // fix: a `model.partial` chunk is transport-level liveness (the
    // model is actively delivering tokens — a wedged server/network
    // still dies), so it resets the runtime's own soft-inactivity clock
    // the same way tool.completed/compaction do. Before this, only the
    // HOST reset on partials; the runtime's soft warning could still
    // fire mid-stream on a long legitimate turn even though the host's
    // hard kill wouldn't.
    last_proof_of_work: &mut std::time::Instant,
    inactivity_soft_warning_fired_in_window: &mut bool,
    watch: Watch<'_>,
) -> Result<StreamOutcome> {
    let (system_chars, prompt_chars) = measure_request_context(&request.messages);
    trajectory.append_model_streaming_start(seq, system_chars, prompt_chars);
    let mut accumulator = ChunkAccumulator::new();
    let mut last_content_bytes: usize = 0;
    // (#2887 F3) Resolved ONCE and reused for the gate's own bounds AND for
    // stamping every trajectory event it writes below — two separate calls
    // to `degeneracy_policy()` (as this used to read) can only ever agree
    // by chance, since each is a fresh env read; a single value read once is
    // the one true source for both.
    let policy = crate::detection::degeneracy_policy();
    let mut gate = StreamGate::new(
        crate::stream_gate::GateBounds { interval_tokens: watch.interval },
        crate::reasoning_loop::measure_and_judge,
        watch.carried,
        // (#2846) The stream gate is the FIRST of two gates; suppressing only
        // the checkpoint gate would still let this one cut generation short,
        // which is a second variable.
        policy.measures(),
        policy.acts(),
    );
    let mut cut = CutSource::None;
    // (#2889) Ticking, so the loop wakes during a silence and can say the
    // model is still writing a tool call — see `TickingStream`'s doc.
    let stream = client.chat_streaming_ticking(request, watch.tick)?;
    for chunk_result in stream {
        // (#2836 stage 2) A SILENT stream ends the turn; it does not kill the
        // dispatch.
        //
        // The read timeout used to propagate as an `Err`, which `main.rs`
        // turns into `result: "error"` with no deliverable — every banked checkpoint of a long turn lost because
        // the endpoint stopped talking at the end of it. The runtime already
        // knows how to end a call and hand the accumulation back; this routes
        // an idle stream into that path instead of off a cliff.
        //
        // Genuine transport failures still propagate. The distinction is a
        // typed marker, not a string match.
        let chunk = match chunk_result {
            Ok(crate::lmstudio::StreamEvent::Chunk(c)) => c,
            // (#2889) No chunk this tick. Once a tool call is named, the
            // silence IS the model writing its arguments: say so, with the
            // counts unchanged. Before a name arrives a silence is prompt
            // processing or a pause, and this names nothing.
            //
            // Deliberately NOT proof of work: `last_proof_of_work` and the
            // soft warning are left alone, and the host does not reset its
            // watchdog on this event. A tick proves only that the runtime is
            // waiting, which a wedged endpoint would also produce.
            Ok(crate::lmstudio::StreamEvent::Idle) => {
                if let Some(name) = accumulator.writing_tool_name() {
                    trajectory.append_tool_call_writing(
                        seq,
                        accumulator.partial_count(),
                        accumulator.content_bytes(),
                        accumulator.generated_bytes(),
                        name,
                    );
                }
                continue;
            }
            Err(e) if e.downcast_ref::<crate::lmstudio::StreamWentSilent>().is_some() => {
                eprintln!(
                    "darkmux-runtime: ⏹ the endpoint went silent — ending this call \
                     and handing back everything it produced, rather than failing the \
                     dispatch. ({e}) (#2836)"
                );
                cut = CutSource::RuntimeAbort(AbortReason::Silent);
                break;
            }
            Err(e) => return Err(e),
        };
        let partial_index = accumulator.ingest(&chunk);
        let cumulative = accumulator.content_bytes();
        let delta_bytes = cumulative.saturating_sub(last_content_bytes);
        last_content_bytes = cumulative;
        // (#2877) `generated_chars` counts everything the model is emitting:
        // answer content, the separate-field reasoning buffer, and streamed
        // tool-call arguments, so the viewer's token-rate scope reads a real
        // rate while a model reasons and while it writes a tool call.
        let generated_chars = accumulator.generated_bytes();
        trajectory.append_model_partial(
            seq,
            partial_index,
            delta_bytes,
            cumulative,
            accumulator.has_tool_calls(),
            generated_chars,
            accumulator.writing_tool_name(),
        );
        *last_proof_of_work = std::time::Instant::now();
        *inactivity_soft_warning_fired_in_window = false;

        // (#2836 stage 1) Observe. A clean verdict costs zero tokens and
        // emits nothing the model can see — the stream simply keeps going.
        // Intervention happens ONLY on detection, which is the inversion
        // this issue is about: the loop used to cut first and decide after.
        match gate.ingest(&chunk) {
            crate::stream_gate::GateAction::Continue => {}
            // (#2844) A clean observation is RECORDED, not discarded.
            //
            // The ratio used to reach the trajectory only when the gate cut,
            // so an engine the gate never cuts produced no samples at all:
            // four clean LMStudio runs yielded zero, while splash yielded 58.
            // That is backwards from what is needed to ask whether one
            // threshold suits both engines, and it is why darkmux could not
            // answer that question about its own detector.
            //
            // These are cheap — one small record per observation boundary,
            // a few per call — and they are the only way the threshold's
            // margin can be checked against a distribution rather than
            // against the corpus it was originally set on.
            crate::stream_gate::GateAction::Observed { slice_chars, ratio, would_abort } => {
                // (#2846) `would_abort` carries the suppressed finding. The
                // record says the output WAS repeating even though the call
                // was allowed to continue, which is the counterfactual the
                // `record` (and `warn`) policy exists to produce. Recording it
                // as `false` here would make such a run indistinguishable from
                // a clean one, and the policy would measure nothing.
                trajectory.append_gate_observation(
                    seq,
                    gate.observations(),
                    slice_chars,
                    ratio,
                    watch.interval,
                    would_abort,
                    policy.as_str(),
                    // (#2887 F3) `Observed` never itself ends the call — that
                    // is the branch's whole definition (see `GateAction`'s
                    // own doc) — so this observation never acted, degenerate
                    // or not.
                    false,
                );
            }
            crate::stream_gate::GateAction::Degenerate { slice_chars, ratio, generated_chars } => {
                trajectory.append_gate_observation(
                    seq,
                    gate.observations(),
                    slice_chars,
                    ratio,
                    watch.interval,
                    true,
                    policy.as_str(),
                    // (#2887 F3) This observation's own verdict is what
                    // ends the call — the `append_gate_abort` call just
                    // below is for this SAME moment.
                    true,
                );
                eprintln!(
                    "darkmux-runtime: ⏹ observation {} — the output is repeating \
                     (degeneracy gate) after {slice_chars} characters ({generated_chars} \
                     of them from this call); ending it at a safe boundary. No tool \
                     call was in flight. (#2836)",
                    gate.observations()
                );
                // (#2836) The gate's OWN verdict, recorded. Without this a
                // runtime abort is the one cut in the system whose reason
                // cannot be read back: `slice_tokens` is null (no usage ever
                // arrives) and the `tail_ratio` on the checkpoint record
                // downstream is the POST-HOC judge's number, computed over a
                // different slice. Two judges disagreeing on one turn is
                // exactly the failure this needs to be able to see.
                trajectory.append_gate_abort(
                    seq,
                    gate.observations(),
                    slice_chars,
                    generated_chars,
                    watch.interval,
                    gate.tool_call_in_flight(),
                    policy.as_str(),
                );
                cut = CutSource::RuntimeAbort(AbortReason::Degenerate);
                // Leaving the loop drops the `TickingStream`, whose drop
                // shuts the socket down at once (#2889 review) — its reader
                // thread may be blocked in a read that would otherwise hold
                // the connection open for the whole read timeout. The half-
                // read connection never returns to ureq's pool. LMStudio
                // logs `Client disconnected. Stopping generation...`.
                break;
            }
        }
    }
    let partial_count = accumulator.partial_count();
    let total_content = accumulator.content_bytes();
    let generated_bytes = accumulator.generated_bytes();
    let reasoning_content = accumulator.take_reasoning_content();
    let mut response = accumulator.into_response();
    let tool_calls_count = response
        .choices
        .first()
        .and_then(|c| c.message.tool_calls.as_ref())
        .map(|tc| tc.len())
        .unwrap_or(0);
    // (#2836 stage 1) A runtime abort never receives the final chunk, so
    // `finish_reason` and `usage` never arrive — the loop cannot decide
    // without ending the call, which is why the terminal state is
    // synthesized here rather than read off the wire. `"length"` is
    // deliberate: it routes into the arm that already knows how to close a
    // thought and hand the accumulation back, and `CutSource` carries the
    // provenance that a token comparison could not (see `is_ours_confirmed`
    // / `is_ours_or_unknown`, which both answer correctly for an abort with
    // no usage at all).
    if matches!(cut, CutSource::RuntimeAbort(_)) {
        if let Some(choice) = response.choices.first_mut() {
            choice.finish_reason = "length".to_string();
        }
        response.usage = None;
    }
    // (B1) The only count a cut call has: what the runtime itself saw
    // stream past before it ended the call. Marked as an estimate all the
    // way to the trajectory; never folded in as a reported figure.
    let estimated_completion_tokens = matches!(cut, CutSource::RuntimeAbort(_))
        .then(|| saturating_u32(generated_bytes.div_ceil(crate::stream_gate::CHARS_PER_TOKEN) as u64));
    // (#2836) Stamped ONLY on a call that emitted no tool calls, and the
    // restriction is the whole point rather than a nicety.
    //
    // This number exists to check `CHARS_PER_TOKEN`, the constant that
    // converts the operator's token-denominated check-in interval into the
    // character cadence the gate actually counts. The gate counts TEXT —
    // reasoning and content deltas — and deliberately does not accumulate
    // tool-call argument fragments. `completion_tokens` counts everything
    // the model generated, arguments included. On a tool-calling call the
    // denominator therefore contains tokens whose characters are absent
    // from the numerator, and the ratio collapses: measured live, a median
    // of 1.58 and a floor of 0.25 on tool-calling calls against 3.93 on
    // text-only ones, where the constant is right.
    //
    // Emitting the collapsed value would have been worse than emitting
    // nothing: it reads like a calibration, so the obvious conclusion from
    // a run's median is that the constant is 2x too high and the cadence
    // should be halved. It is not, and it should not be.
    let chars_per_token = response
        .usage
        .as_ref()
        .and_then(|u| u.completion)
        .filter(|c| *c > 0 && tool_calls_count == 0)
        .map(|c| gate.generated_chars() as f32 / c as f32);
    trajectory.append_model_streaming_end(
        seq,
        partial_count,
        total_content,
        tool_calls_count,
        gate.observations(),
        chars_per_token,
    );
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
    // (#2836) Stage 0 drives the stream to its end and never ends it
    // itself, so the runtime is never the cutter here. Stage 1 feeds each
    // chunk to a `StreamGate` and reports `RuntimeAbort` when it
    // intervenes; the caller already routes on this field.
    Ok(StreamOutcome { response, cut, estimated_completion_tokens })
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

/// (#2164) Returns the `reasoning_content` this call captured BEFORE any
/// promotion or stripping happened — including on a turn where the strip
/// below is about to wipe it. A caller that needs to know whether THIS
/// response reasoned at all (e.g. `dispatch_has_reasoned`) MUST read this
/// return value, not `msg.reasoning_content` after the call returns: for a
/// terminal `tool_calls`/`stop` turn, `reasoning_content` has already been
/// cleared to `None` by the time this function returns, and the field never
/// makes it into `per_turn_reasoning` (assembled later, downstream of this
/// call) at all. Found live during #2164 review: a probe with
/// `reasoning_content` + tool calls + `finish_reason: "tool_calls"` sent
/// turn 2 out under the ANSWER bound and fired the "no reasoning region"
/// detector — for a model that DID reason, because the field was already
/// gone by the time anything downstream looked for it.
fn promote_terminal_reasoning(msg: &mut Message, finish_reason: &str) -> Option<String> {
    let captured_before_strip = msg.reasoning_content.clone();
    let has_tools = msg.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
    let content_empty = msg.content.as_deref().is_none_or(|c| c.trim().is_empty());
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
    captured_before_strip
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
/// (#1959) Keep only the tool calls whose arguments actually parse.
///
/// The companion to `count_well_formed_tool_calls` — that one reports, this
/// one enforces, and for most of this feature's life only the reporting half
/// existed. Applied ONLY on the salvage path: everywhere else a malformed
/// tool call is the model's own output and belongs in the transcript, where
/// the failure-rate detector can see it. Here it is an artifact of OUR cut.
fn retain_well_formed_tool_calls(msg: &mut Message) {
    if let Some(tcs) = msg.tool_calls.as_mut() {
        tcs.retain(tool_call_is_well_formed);
        // An empty vector is not the same as no tool calls: `resolve_finish_reason`
        // asks whether any remain, and a `Some([])` would answer "yes".
        if tcs.is_empty() {
            msg.tool_calls = None;
        }
    }
}

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

// ─── (#2169) malformed structured tool-call names ──────────────────────

/// The `tool` role message body every invalid-name call's `tool_call_id`
/// gets. The OpenAI-compatible chat-completions protocol LM Studio
/// implements requires exactly one `tool` message per id in the
/// assistant's `tool_calls` array — omitting one for calls that get
/// coalesced into a single feedback note is rejected by the endpoint on
/// the NEXT request (a missing tool_call_id reference). Rather than
/// repeat the full "N tool call(s) carried names that are not tools…"
/// text N times (which is the shape #2169 exists to STOP — one turn was
/// observed producing 48 near-duplicate error strings), every invalid
/// call gets this short constant body; the ONE synthetic feedback
/// message queued alongside it (`FeedbackInjector::queue_malformed_tool_names`)
/// carries the real explanation + the tool list, and is what the model
/// actually reads to correct course. This is the "one message per id
/// with a short constant body" option named in PR #2169's description —
/// `Message::tool_result` already builds exactly this shape, so no new
/// message constructor was needed.
const MALFORMED_TOOL_CALL_RESULT_BODY: &str =
    "[darkmux-runtime] not executed — this call's name is not a real tool. \
     See the system note this turn for the full explanation and the list of \
     tools you can actually call.";

/// Sanitize a malformed tool-call `name` before any part of it rides into
/// a trajectory event (and from there, a flow record, and potentially an
/// HTTP header on a hook delivery — #2178's `sanitize_header_value` in
/// `darkmux-flow` filters the SAME allowlist at that later boundary; this
/// is the same idea applied at the point of origin so the value is
/// already clean by the time it reaches any downstream sink). Model
/// output is untrusted content — Devstral's sliced-`[TOOL_CALLS]` name
/// has been observed carrying newlines and arbitrary punctuation from
/// quoted code.
///
/// Allowlist: tab, space, and the visible ASCII range `0x21..=0x7E` —
/// anything else (including CR/LF) becomes `_`. Capped at 40 chars: enough
/// to recognize the pattern (a snippet of quoted code, the `[TOOL_CALLS]`
/// marker itself), never enough to leak the model's full generation into
/// telemetry.
fn sanitize_sample_name_prefix(raw: &str) -> String {
    const MAX_LEN: usize = 40;
    raw.chars()
        .map(|c| {
            let is_allowed = c.is_ascii() && {
                let b = c as u32;
                b == 0x09 || b == 0x20 || (0x21..=0x7E).contains(&b)
            };
            if is_allowed {
                c
            } else {
                '_'
            }
        })
        // Every char post-filter is a single ASCII byte, so char-count
        // truncation here is also byte-count truncation of the result.
        .take(MAX_LEN)
        .collect()
}

/// Classify why `name` fell outside `allowed_tool_names` — see
/// [`MalformedReason`]'s doc for what distinguishes the two cases.
/// (#479, #2963) Whether a tool call's arguments parse: the one predicate
/// the salvage's `retain_well_formed_tool_calls` and `plan_tool_calls` share.
fn tool_call_is_well_formed(tc: &ToolCall) -> bool {
    serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok()
}

fn classify_invalid_tool_call(name: &str) -> MalformedReason {
    if crate::tools::Tool::from_name(name).is_some() {
        MalformedReason::RealToolNotGranted
    } else {
        MalformedReason::NotATool
    }
}

/// Partition a turn's structured tool calls into THREE buckets — (1)
/// dispatchable, (2) a real darkmux tool this dispatch wasn't granted, (3)
/// not a real tool at all — BEFORE any of them reach `tools::dispatch`
/// (#2169). (#2963) It follows the turn's plan (`plan_tool_calls`, made
/// before `model.completed` was written) rather than re-deciding by name,
/// so the calls that run are exactly the ones that record left unmarked; a
/// `Discarded` call (cut off mid-arguments by the #479 salvage) falls in no
/// bucket. See [`MalformedReason`]'s doc for why buckets (2) and (3)
/// are kept separate rather than one "invalid" bucket.
///
/// Structured `tool_calls` come back from LM Studio's API already
/// parsed — unlike the plain-text promoter (`plain_text_tool_calls.rs`),
/// which validates every name against `allowed_tool_names` before it ever
/// becomes a `ToolCall`, nothing upstream of this point checked a
/// STRUCTURED call's name pre-#2169. Partitioning here — before
/// `calls_snapshot` is captured, before the dispatch loop, before the
/// cycle/failure-rate detectors ever see a call — means neither an
/// invalid-name nor an ungranted-real-tool call can ever reach
/// `tools::dispatch`, never reach `FailureRateDetector::record`/
/// `CycleDetector::record` (so neither can contribute to the #419
/// consecutive-failure cascade), and never gets checkpointed as a
/// "pending" call a resume would replay.
///
/// Applies uniformly to every source of a turn's `calls`: an organic
/// `finish_reason=tool_calls` response, AND a #479 per-turn-cap salvage
/// (salvage sets `assistant_message.tool_calls` to the well-formed-JSON
/// subset via `retain_well_formed_tool_calls`, then routes through the
/// SAME `"tool_calls"` match arm this partition lives in) — salvage
/// filters on JSON well-formedness, this filters on name membership; the
/// two are orthogonal and a salvaged batch gets both. Composes the same
/// way with #2171/#2176's generation-checkpoint-bound salvage, which
/// re-enters this exact arm too — see
/// `generation_bound_salvage_and_malformed_names_compose_on_the_same_turn`
/// in this module's tests for a probe through `run_with_sleeper` that
/// exercises both on one turn.
fn partition_by_plan(
    calls: Vec<ToolCall>,
    plan: &[CallFate],
) -> (Vec<ToolCall>, Vec<ToolCall>, Vec<ToolCall>) {
    let mut granted = Vec::new();
    let mut ungranted = Vec::new();
    let mut not_a_tool = Vec::new();
    for (call, fate) in calls.into_iter().zip(plan) {
        match fate {
            CallFate::Runs => granted.push(call),
            CallFate::NotGranted => ungranted.push(call),
            CallFate::NotATool => not_a_tool.push(call),
            CallFate::Discarded => {}
        }
    }
    (granted, ungranted, not_a_tool)
}

/// (#2963) What becomes of one tool call a model returned. Decided once per
/// turn, before `model.completed` is written (`plan_tool_calls`), and then
/// followed by the dispatch (`partition_by_plan`), so the record's
/// `runs: false` marks and what actually runs cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallFate {
    /// Dispatched to its tool.
    Runs,
    /// A real tool this dispatch was not granted (#2169).
    NotGranted,
    /// Not a tool at all (#2169).
    NotATool,
    /// Never reaches the partition: cut off mid-arguments and dropped by the
    /// #479 salvage, or on a turn that dispatches nothing (the #2836 discard,
    /// a `length` finish, a context overflow).
    Discarded,
}

/// (#2963) The fate of each of a turn's tool calls, in their order. A turn
/// that does not dispatch its calls (`dispatches` false) discards them all;
/// a #479 salvage discards the ones whose arguments do not parse
/// (`tool_call_is_well_formed`, the predicate `retain_well_formed_tool_calls`
/// uses); the rest are sorted by name, as #2169's partition always did: a
/// granted call runs, a real ungranted tool and a non-tool do not.
fn plan_tool_calls(
    calls: &[ToolCall],
    dispatches: bool,
    salvaged: bool,
    allowed_tool_names: &HashSet<String>,
) -> Vec<CallFate> {
    calls
        .iter()
        .map(|call| {
            if !dispatches || (salvaged && !tool_call_is_well_formed(call)) {
                CallFate::Discarded
            } else if allowed_tool_names.contains(&call.function.name) {
                CallFate::Runs
            } else {
                match classify_invalid_tool_call(&call.function.name) {
                    MalformedReason::RealToolNotGranted => CallFate::NotGranted,
                    // `Unknown` is a reader's value (#3035); this classifier
                    // never returns it.
                    MalformedReason::NotATool | MalformedReason::Unknown => CallFate::NotATool,
                }
            }
        })
        .collect()
}

/// Handle ONE reason-bucket of a turn's non-dispatchable tool calls
/// (#2169): never dispatch them, coalesce into ONE feedback message worded
/// SPECIFICALLY for `reason` (see [`MalformedReason`]'s doc — the two
/// reasons need different wording, not just a different counter), emit ONE
/// `dispatch.tool.malformed_names` trajectory event carrying `reason` +
/// this bucket's own count + a sample name, and satisfy the OpenAI
/// tool-message-per-`tool_call_id` protocol with
/// `MALFORMED_TOOL_CALL_RESULT_BODY`'s short constant body (same body for
/// both reasons — the reason-specific explanation lives in the ONE
/// feedback message, not repeated per call). No-op when `calls` is empty.
///
/// Called once per bucket at the call site — a turn carrying BOTH an
/// ungranted-real-tool call and a not-a-tool call gets TWO events, TWO
/// feedback messages, correctly separated telemetry, rather than one
/// muddled bucket.
#[allow(clippy::too_many_arguments)]
fn handle_invalid_tool_calls(
    calls: &[ToolCall],
    reason: MalformedReason,
    allowed_tool_names: &HashSet<String>,
    model: &str,
    turns: u32,
    trajectory: &mut Trajectory,
    feedback_injector: &mut FeedbackInjector,
    messages: &mut Vec<Message>,
) {
    if calls.is_empty() {
        return;
    }
    let count = calls.len() as u32;
    // One representative name is enough to recognize the pattern; the
    // trajectory event's `count` already names how many there were.
    let sample_name_prefix = sanitize_sample_name_prefix(&calls[0].function.name);
    eprintln!(
        "darkmux-runtime: ⚠ {reason:?} — {count} structured tool call(s) this turn \
         (model={model}, sample=\"{sample_name_prefix}\"); none dispatched, coalesced \
         into one feedback message. (#2169)",
    );
    trajectory.append_malformed_tool_names(turns, count, model, &sample_name_prefix, reason);

    let mut tool_names: Vec<&str> = allowed_tool_names.iter().map(String::as_str).collect();
    tool_names.sort_unstable();
    let tools_str = tool_names.join(", ");

    match reason {
        MalformedReason::NotATool | MalformedReason::Unknown => {
            feedback_injector.queue_malformed_tool_names(count as usize, &tools_str);
        }
        MalformedReason::RealToolNotGranted => {
            // Unlike the not-a-tool case, these names ARE meaningful — the
            // model named a real tool, just one it doesn't have. Naming
            // which one(s) in the feedback is the corrective signal.
            let mut offending: Vec<&str> =
                calls.iter().map(|c| c.function.name.as_str()).collect();
            offending.sort_unstable();
            offending.dedup();
            feedback_injector.queue_tool_not_granted(
                count as usize,
                &offending.join(", "),
                &tools_str,
            );
        }
    }

    for call in calls {
        messages.push(Message::tool_result(
            call.id.clone(),
            call.function.name.clone(),
            MALFORMED_TOOL_CALL_RESULT_BODY,
        ));
    }
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

/// (#3074) Whether one logical turn has generated as many tokens as the
/// context window holds. Without a configured window there is nothing to
/// derive the bound from, so it never is.
fn turn_fills_window(context_window: Option<u32>, turn_tokens: u32) -> bool {
    context_window.is_some_and(|window| turn_tokens >= window)
}

/// Which bound ended a turn's run of checkpoint continuations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuationLimit {
    /// (#2171) The generation check-in's continuation count.
    GenerationBudget,
    /// (#3074) The turn generated as many tokens as the context window holds.
    ContextWindow,
}

impl ContinuationLimit {
    /// The bound that applies, the more specific count first.
    fn reached(generation_budget_exhausted: bool, window_filled: bool) -> Option<Self> {
        match (generation_budget_exhausted, window_filled) {
            (true, _) => Some(Self::GenerationBudget),
            (false, true) => Some(Self::ContextWindow),
            (false, false) => None,
        }
    }

    fn reason(self) -> EscalationReason {
        match self {
            Self::GenerationBudget => EscalationReason::GenerationCheckpointBudgetExhausted,
            Self::ContextWindow => EscalationReason::TurnContinuationsExhausted,
        }
    }
}

/// The numbers [`continuation_limit_outcome`] reports.
struct ContinuationFacts<'a> {
    turns: u32,
    turn_tokens: u32,
    checkpoints: u32,
    model: &'a str,
    latest_prompt_tokens: u32,
    turn_delay_ms: u64,
    generation_interval: u32,
    generation_continuations: u32,
    max_generation_continuations: u32,
    answer_max_tokens: u32,
}

/// The escalation for a turn that ran out of checkpoint continuations: say so
/// on stderr, record it, and hand the banked work on.
fn continuation_limit_outcome(
    trajectory: &mut Trajectory,
    limit: ContinuationLimit,
    f: ContinuationFacts<'_>,
    final_answer: Option<String>,
    messages: Vec<Message>,
    failed_to_run: Vec<FailedExec>,
) -> LoopOutcome {
    let detail = match limit {
        ContinuationLimit::GenerationBudget => format!(
            "hit the generation check-in ({} tokens) {} times, exceeding the budget of {} \
             continuations (answer_max_tokens {} / generation_checkpoint_interval_tokens {}). \
             (#2171)",
            f.generation_interval,
            f.generation_continuations,
            f.max_generation_continuations,
            f.answer_max_tokens,
            f.generation_interval
        ),
        ContinuationLimit::ContextWindow => format!(
            "generated {} tokens across {} checkpoints, as many as the context window holds, \
             so it cannot be resumed again. (#3074)",
            f.turn_tokens, f.checkpoints
        ),
    };
    eprintln!(
        "darkmux-runtime: escalation_triggered — turn {} {detail} Emitting EscalationTriggered \
         for frontier handoff with everything banked so far ATTACHED.",
        f.turns
    );
    trajectory.append_escalation_triggered(
        f.turns,
        escalation_reason_str(limit.reason()),
        f.model,
        f.latest_prompt_tokens,
    );
    LoopOutcome {
        final_answer,
        terminal_reason: TerminalReason::EscalationTriggered(limit.reason()),
        messages,
        turn_delay_effective_ms: f.turn_delay_ms,
        failed_to_run,
    }
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
    // (#3074) `0` is UNBOUNDED: the host sets no deadline, so there is no
    // kill to warn about and the threshold can never be reached.
    if budget_secs == 0 {
        return u64::MAX;
    }
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

/// (#2792) The occupancy the compaction decision is made against.
///
/// `latest_prompt_tokens` is what the endpoint reported for the request that
/// ALREADY WENT OUT. By the time this is consulted, the loop has appended the
/// assistant turn and every tool result it produced, so that number describes
/// a SMALLER conversation than the one about to be sent. Deciding on it alone
/// lets one large tool result carry the next request straight past the
/// declared context window with nothing compacting in between — measured on a
/// real run as a turn reporting 6,290 followed by a request of 38,446 against
/// a declared 32,000.
///
/// That is a hard failure, not a slow one: a model loaded at the window its
/// profile declares answers the oversized request with HTTP 400 ("the number
/// of tokens to keep from the initial prompt is greater than the context
/// length"). It went unnoticed only because the loaded instance was larger
/// than the declared window and the request fit anyway.
///
/// So the local chars/4 estimate over the CURRENT thread is taken every turn,
/// and the larger of the two wins. Taking the max — rather than preferring the
/// estimate — keeps the endpoint's own count authoritative whenever it is the
/// bigger number (it is the ground truth for everything already sent, and
/// chars/4 under-counts some tokenizations), so this can only ever move a
/// compaction EARLIER, never later. #854 introduced this estimate for the
/// narrower stale-count case; the estimate was always the right input, and the
/// staleness gate was the accident.
///
/// **What this does NOT guarantee, stated so the bound is not mistaken for a
/// promise (#2792 merge-gate).** Measuring occupancy correctly is necessary
/// and not sufficient: the compaction it asks for can still be vetoed, and
/// then the oversized request goes out anyway. Each of these was proven
/// against a declared 32,000 window:
///
/// - `conversation_long_enough_to_compact` needs `PRESERVE_HEAD + 1 +
///   PRESERVE_TAIL` = 7 messages. Turn 2 after a large first read is 4
///   messages, so nothing compacts — and "read this large file and summarize
///   it" is the modal opening move for a coder role.
/// - Weight sitting in the PRESERVED TAIL is untouchable. Measured: a thread
///   at 50,047 still measured 50,009 after annihilating its entire
///   compactable middle — better than any real compaction could do.
/// - `compactor_model: None` vetoes unconditionally.
/// - Turn 1 has no compaction gate at all.
/// - The `"length"` (#1221 checkpoint) arm re-enters the request build with
///   no occupancy check.
///
/// So this narrows the window; it does not close it. Closing it needs a
/// pre-send bound against `context_window` with a defined non-fatal
/// behavior, tracked separately. What IS now true is the thing #2792 was
/// filed about: the compaction trigger sees the thread about to be SENT
/// rather than the one already sent.
///
/// An operator-facing diagnostic for the remaining cases was written and then
/// CUT from this PR (round-3 merge gate). Placed before the compaction it
/// described, it predicted that compaction's outcome from the previous turn's
/// and printed a confident falsehood on this PR's own fixture — "~30084
/// tokens … the endpoint will REFUSE this request" on a turn whose next
/// request was 146 tokens — and burned its once-per-dispatch latch doing so,
/// silencing the genuine case it existed for. It belongs AFTER the compaction
/// block with occupancy recomputed, which is a different change; tracked
/// separately rather than landed half-right on a release blocker.
fn effective_prompt_occupancy(
    messages: &[Message],
    latest_prompt_tokens: u32,
    anchor: Option<PromptAnchor>,
) -> u32 {
    let (sys_chars, prompt_chars) = measure_request_context(messages);
    // `tools_chars: 0` — this decision never had access to the tools schema
    // and still doesn't. With an anchor the schema's real cost is already
    // inside `anchor.tokens`.
    //
    // THE ANCHORLESS PATH KEEPS chars/4 (round-5 merge gate). The 2.75 ruler
    // is right for the pre-send BOUND, a last-resort guard that must err
    // high. Letting it reach this decision as well would move the compaction
    // TRIGGER 45% earlier on every turn with no ground truth to justify it —
    // turn one, any usage-less turn, every post-compaction and post-trim turn
    // — which is a real behavior change to the thing that actually keeps the
    // thread bounded, and nothing here measured it. So the anchored estimate
    // is used exactly where the anchored branch applies, and this decision is
    // otherwise left on the ruler it has always used.
    let message_chars = sys_chars + prompt_chars;
    let estimate = match anchor {
        Some(a) if message_chars >= a.chars => estimate_prompt_tokens(message_chars, 0, anchor),
        _ => (message_chars / 4) as u32,
    };
    estimate.max(latest_prompt_tokens)
}

/// (#2792 round-4) Chars-per-token ruler for content the endpoint has not
/// counted yet.
///
/// Deliberately below chars/4, and the number is measured rather than picked.
/// On the dogfood run that reopened #2792, one turn added 94,312 characters
/// and the endpoint's prompt count grew by 32,144 tokens — **2.93 characters
/// per token**. That is what a thread of tool results and tool-call arguments
/// costs; chars/4 is a prose ruler (3.94 on this project's own TypeScript
/// fixture) applied to content that is mostly not prose.
///
/// The conservatism is nearly free BECAUSE of the anchor below: it is applied
/// only to one turn's new characters, never to the whole thread, so erring
/// low here does not shrink the usable window the way a flat 2.75 ruler over
/// everything would.
const UNCOUNTED_CHARS_PER_TOKEN: f64 = 2.75;

/// (#2792 round-4) What the endpoint counted, for exactly which characters.
///
/// `usage.prompt_tokens` is ground truth — the tokenizer's own answer for the
/// request that just went out, including the system message, the chat
/// template's per-message envelope, and the tools schema, none of which
/// `measure_request_context` can see. Pairing it with the characters measured
/// for that same request turns the next turn's estimate from "guess the whole
/// thread" into "carry an exact number forward and guess only the delta".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromptAnchor {
    /// Message characters measured for the request that was sent, on the same
    /// basis `measure_request_context` returns. Deliberately EXCLUDES the
    /// tools schema: the schema is constant across a dispatch, so leaving it
    /// out of both sides cancels in the delta, while `tokens` below already
    /// carries its true cost.
    pub(crate) chars: usize,
    /// `usage.prompt_tokens` the endpoint reported for that request.
    pub(crate) tokens: u32,
}

/// (#2792 round-4) Estimate the prompt tokens of the thread about to be sent.
///
/// With an anchor, everything up to the last send is exact and only the new
/// characters are estimated. Against the measured dogfood turn this lands
/// within 6% (40,585 estimated vs 38,434 actual) where chars/4 was 27% low
/// (30,132) — low enough that the bound did not fire on the one send it
/// existed to catch.
///
/// Without an anchor (turn 1, or a thread that SHRANK since the last send
/// because something trimmed it, which invalidates the delta) it falls back
/// to the flat ruler over everything, tools schema included.
///
/// In every case the chars/4 estimate this decision used BEFORE the anchor
/// existed is kept as a floor, so an endpoint that freezes or under-reports
/// its count cannot talk the bound down. See the body.
pub(crate) fn estimate_prompt_tokens(
    message_chars: usize,
    tools_chars: usize,
    anchor: Option<PromptAnchor>,
) -> u32 {
    let total = message_chars + tools_chars;
    let anchored = match anchor {
        Some(a) if message_chars >= a.chars => {
            let added = (message_chars - a.chars) as f64 / UNCOUNTED_CHARS_PER_TOKEN;
            a.tokens.saturating_add(added as u32)
        }
        _ => (total as f64 / UNCOUNTED_CHARS_PER_TOKEN) as u32,
    };
    // THE ANCHOR MAY NEVER LOWER AN ESTIMATE. `usage.prompt_tokens` is ground
    // truth only while the endpoint is telling the truth, and #854 exists
    // because it sometimes FREEZES — reporting the same count turn after turn
    // while the thread grows. An anchor built on a frozen count has its
    // `chars` refreshed every turn and its `tokens` stuck, so the delta
    // collapses to nothing and the estimate follows it down. Two #854
    // regression tests caught exactly that, which is the whole reason this
    // floor is here.
    //
    // chars/4 is the ruler this decision used before the anchor existed, so
    // keeping it as a FLOOR makes the change one-directional: never lower
    // than it was, higher whenever ground truth says the thread is heavier
    // than characters suggest. A lying endpoint degrades to the old behavior
    // instead of defeating the bound.
    let flat_floor = (total / 4) as u32;
    anchored.max(flat_floor)
}

/// (#2792 round-4) The inverse of [`estimate_prompt_tokens`]: how many MESSAGE
/// characters fit in `window` tokens, which is what `hard_trim_to_fit` takes.
///
/// Inverting the same function the bound decided with is the point — a budget
/// derived on a different ruler than the measurement would either trim to a
/// target that still measures over (an infinite no-op) or over-trim.
///
/// When the anchor alone already meets or exceeds the window, there is no
/// headroom to spend and the anchor cannot be reasoned down (a trim removes
/// characters the endpoint already counted, so the pairing no longer holds);
/// the flat ruler is the honest degradation.
/// (#2792 round-5) Why the bound could not bring the request inside the
/// window, as a pure selection over what the trim actually found.
///
/// Extracted so the four arms can be tested. The one that mattered was
/// unreachable from any test: it needed a thread holding trimmable results
/// that the trimmer declined, which the round-4 budget defect produced and
/// the round-5 fix removes.
///
/// The arm this function exists to keep honest is the last one. It used to
/// read "the weight is not in tool results", and it is reached precisely when
/// there ARE trimmable tool results — so it asserted the opposite of its own
/// condition, and sent the operator to raise `n_ctx` when clearing results
/// would have worked. That is the second time this diagnostic has stated
/// something its inputs contradict.
pub(crate) fn why_the_bound_could_not_fit(
    results_trimmed: usize,
    trimmable: usize,
    already_elided: usize,
) -> &'static str {
    if results_trimmed > 0 {
        "even after this trim, what remains does not fit"
    } else if trimmable == 0 {
        "no single tool result is large enough to trim, so the weight is \
         spread across many small ones"
    } else if already_elided == trimmable {
        "every trimmable tool result has already been elided"
    } else {
        "the trimmer could not reduce the remaining results any further"
    }
}

pub(crate) fn message_chars_budget(
    window: u32,
    tools_chars: usize,
    anchor: Option<PromptAnchor>,
) -> usize {
    // Inverse of the flat branch, and of the chars/4 floor.
    let flat_budget =
        ((window as f64 * UNCOUNTED_CHARS_PER_TOKEN) as usize).saturating_sub(tools_chars);
    let floor_budget = (window as usize).saturating_mul(4).saturating_sub(tools_chars);
    let anchored = match anchor {
        Some(a) if window > a.tokens => {
            let headroom = (window - a.tokens) as f64 * UNCOUNTED_CHARS_PER_TOKEN;
            a.chars.saturating_add(headroom as usize)
        }
        // THE TWO FUNCTIONS MUST BRANCH ON THE SAME REGION (round-5 merge
        // gate). This arm used to fall through to `flat_budget` alone, which
        // branches on a DIFFERENT predicate than the estimate does: the
        // estimate takes its anchored branch whenever `message_chars >=
        // a.chars`, regardless of how `a.tokens` compares to the window. So
        // for an anchor at or past the window the budget handed back
        // 2.75 x window characters — larger than the thread — and
        // `hard_trim_to_fit` returned at its `current <= target_bytes` early
        // exit having trimmed NOTHING, while the estimate above still read
        // 59% over. The bound went inert, printed its over-window figure, and
        // sent the request.
        //
        // That is reachable, and specifically in the configuration this bound
        // was written to police: `a.tokens` is verbatim `usage.prompt_tokens`,
        // so `a.tokens >= window` means the endpoint ACCEPTED a request at or
        // past the declared window — which is what happens when the loaded
        // n_ctx exceeds the n_ctx the profile declares. This file's own note
        // on `effective_prompt_occupancy` records that as the observed state.
        //
        // When the anchor alone meets the window, no char count at or above
        // `a.chars` can get under it on the anchored ruler. The only region
        // that can is BELOW the anchor, where the flat branch applies — so
        // the budget is the flat inverse, capped strictly under `a.chars` so
        // the estimate actually lands in that branch.
        Some(a) => flat_budget.min(a.chars.saturating_sub(1)),
        None => flat_budget,
    };
    // The estimate is the MAX of the anchored figure and the chars/4 floor,
    // so the budget is the MIN of their inverses — whichever constraint binds
    // first.
    anchored.min(floor_budget)
}

pub(crate) fn measure_request_context(messages: &[Message]) -> (usize, usize) {
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

    // ---------------------------------------------------------------
    // (#2094) turn_delay_ms: the pure clamp + deadline-extension arithmetic,
    // tested in isolation. The full loop is scripted-mock-server-driven
    // below; these two functions are the piece that's cheapest to falsify
    // directly per-guard.
    // ---------------------------------------------------------------

    #[test]
    fn resolve_turn_delay_ms_below_timeout_passes_through_unclamped() {
        let (ms, warning) = resolve_turn_delay_ms(3000, 600);
        assert_eq!(ms, 3000);
        assert!(warning.is_none());
    }

    #[test]
    fn resolve_turn_delay_ms_at_or_above_timeout_clamps_to_half_and_warns() {
        // 10s budget = 10000ms; a configured 10000ms is AT the timeout.
        let (ms, warning) = resolve_turn_delay_ms(10_000, 10);
        assert_eq!(ms, 5_000, "clamped to half the timeout");
        let w = warning.expect("must warn when clamping");
        assert!(w.contains("10000"), "names the configured value: {w}");
        assert!(w.contains("5000"), "names the clamped value: {w}");

        // Strictly above the timeout clamps identically.
        let (ms, warning) = resolve_turn_delay_ms(999_999, 10);
        assert_eq!(ms, 5_000);
        assert!(warning.is_some());
    }

    #[test]
    fn resolve_turn_delay_ms_zero_budget_never_clamps() {
        // A 0-second inactivity timeout is a degenerate operator setting
        // (an effectively disabled watchdog) — clamping against it would
        // silently erase an intentional rest (half of zero is zero).
        let (ms, warning) = resolve_turn_delay_ms(5_000, 0);
        assert_eq!(ms, 5_000);
        assert!(warning.is_none());
    }

    // ─── #2094 second round, finding 4: widen the clamp band to half ─────

    #[test]
    fn resolve_turn_delay_ms_at_half_the_timeout_now_clamps_though_well_below_the_full_timeout() {
        // 10s budget = 10000ms. A configured 6000ms is well BELOW the full
        // timeout (the old band's threshold) but AT/ABOVE half of it — the
        // widened band clamps it, because 6000ms plus a real turn's
        // latency plus the tailer's own 250ms poll overhead can still
        // approach a 10000ms deadline in practice.
        let (ms, warning) = resolve_turn_delay_ms(6_000, 10);
        assert_eq!(ms, 5_000, "clamped to half the timeout");
        let w = warning.expect("must warn when clamping");
        assert!(w.contains("6000"), "names the configured value: {w}");
        assert!(w.contains("5000"), "names the clamped value: {w}");
    }

    #[test]
    fn resolve_turn_delay_ms_exactly_at_half_the_timeout_clamps() {
        // Boundary: configured_ms * 2 == budget_ms clamps (>=, not >).
        let (ms, warning) = resolve_turn_delay_ms(5_000, 10);
        assert_eq!(ms, 5_000);
        assert!(warning.is_some());
    }

    #[test]
    fn resolve_turn_delay_ms_just_below_half_the_timeout_passes_through_unclamped() {
        // One ms under the boundary must NOT clamp.
        let (ms, warning) = resolve_turn_delay_ms(4_999, 10);
        assert_eq!(ms, 4_999);
        assert!(warning.is_none());
    }

    #[test]
    fn extend_deadline_by_rest_moves_the_deadline_forward_by_exactly_the_rest() {
        let now = std::time::Instant::now();
        let extended = extend_deadline_by_rest(now, 500);
        assert_eq!(extended, now + std::time::Duration::from_millis(500));
        assert!(extended > now, "the deadline must move strictly forward");
    }

    #[test]
    fn extend_deadline_by_rest_zero_is_a_true_no_op() {
        let now = std::time::Instant::now();
        assert_eq!(extend_deadline_by_rest(now, 0), now);
    }

    #[test]
    fn sleep_wake_jump_fires_past_2x_budget() {
        assert!(is_suspected_sleep_wake_jump(1_201, 600), "just over 2x");
        assert!(is_suspected_sleep_wake_jump(3_600, 600), "well over 2x");
    }

    #[test]
    fn sleep_wake_jump_does_not_fire_at_or_below_2x_budget() {
        assert!(!is_suspected_sleep_wake_jump(1_200, 600), "exactly 2x is not a jump");
        assert!(!is_suspected_sleep_wake_jump(700, 600), "past the soft threshold, still not 2x");
        assert!(!is_suspected_sleep_wake_jump(0, 600), "no elapsed time at all");
    }

    #[test]
    fn sleep_wake_jump_never_fires_on_an_unbounded_budget() {
        assert!(
            !is_suspected_sleep_wake_jump(1_000_000, 0),
            "budget=0 (unbounded) has no 2x line to exceed"
        );
    }

    /// (#2094 boundary case) The soft-inactivity check in the loop is
    /// `last_proof_of_work.elapsed() >= threshold_secs`. Extending the
    /// deadline pushes `last_proof_of_work` FORWARD — potentially past
    /// `Instant::now()` in a fast test, since the injected sleeper never
    /// actually blocks (no real wall-clock time passes during a "rest").
    /// `Instant::elapsed()` on a reference point in the future must
    /// saturate to ZERO, not panic or underflow — which is exactly why a
    /// rest can never itself read as having crossed the soft-warning
    /// threshold: after an extension, `elapsed()` can only report LESS
    /// time-toward-threshold, never more. This is the actual mechanism
    /// that makes "the rest cannot trip the deadline" true.
    #[test]
    fn extending_the_deadline_into_the_future_makes_elapsed_read_as_zero_not_negative() {
        let now = std::time::Instant::now();
        let extended = extend_deadline_by_rest(now, 5_000);
        assert_eq!(
            extended.elapsed(),
            std::time::Duration::ZERO,
            "a deadline extended into the future must never report negative/underflowed elapsed time"
        );
    }

    // ---------------------------------------------------------------
    // (#2774 tier 2) `apply_pace_duty_cycle_delay` — the host-set
    // turn-delay half of the pace file's third state, exercised in
    // isolation with an injected sleeper (no real sleeps, no wall-clock
    // assertions — every assertion is on recorded call counts/durations
    // and on `rest_ms`/`rests`).
    // ---------------------------------------------------------------

    #[derive(Default)]
    struct DutyCycleSleeper {
        calls: std::cell::RefCell<Vec<u64>>,
    }
    impl TurnSleeper for DutyCycleSleeper {
        fn sleep(&self, ms: u64) {
            self.calls.borrow_mut().push(ms);
        }
    }

    fn write_pace(dir: &std::path::Path, body: &str) {
        std::fs::write(pace::pace_file_path(dir), body).unwrap();
    }

    #[test]
    fn duty_cycle_delay_sleeps_once_for_the_host_set_value() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            r#"{"pause": false, "reason": "thermal-duty-cycle", "state": "fair", "turn_delay_ms": 15000}"#,
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            600, // inactivity_budget_secs — well above the 15s delay, no clamp
            &sleeper,
            &mut traj,
            3,
            &mut last_pow,
            &mut soft_fired,
        );

        assert_eq!(sleeper.calls.borrow().as_slice(), &[15_000], "sleeps exactly the host-set duration");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 15_000);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 1);
    }

    /// (#2877) A rest is recorded when it STARTS, so a live viewer can show
    /// "rest 15s" during the rest. Recorded after the sleep, the record
    /// arrived as the rest ended and the viewer read the 15s as tools or a
    /// stall. This sleeper counts the `runtime.rest` events already in the
    /// trajectory at the moment it is asked to sleep.
    struct RestVisibleAtSleepSleeper {
        traj_path: std::path::PathBuf,
        rests_seen_at_sleep: std::cell::RefCell<Vec<usize>>,
        flip_pause_off: Option<std::path::PathBuf>,
    }
    impl TurnSleeper for RestVisibleAtSleepSleeper {
        fn sleep(&self, _ms: u64) {
            let body = std::fs::read_to_string(&self.traj_path).unwrap_or_default();
            let n = body.lines().filter(|l| l.contains("\"type\":\"runtime.rest\"")).count();
            self.rests_seen_at_sleep.borrow_mut().push(n);
            if let Some(dir) = &self.flip_pause_off {
                write_pace(dir, r#"{"pause": false, "reason": "thermal", "state": "fair"}"#);
            }
        }
    }

    #[test]
    fn duty_cycle_rest_is_recorded_before_the_sleep_not_after() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            r#"{"pause": false, "reason": "thermal-duty-cycle", "state": "fair", "turn_delay_ms": 15000}"#,
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = RestVisibleAtSleepSleeper {
            traj_path: tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
            rests_seen_at_sleep: Default::default(),
            flip_pause_off: None,
        };
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;
        apply_pace_duty_cycle_delay(
            &mut reader, tmp.path(), 900_000, 600, &sleeper, &mut traj, 3,
            &mut last_pow, &mut soft_fired,
        );
        assert_eq!(sleeper.rests_seen_at_sleep.borrow().as_slice(), &[1], "the rest event exists when the sleep begins");
    }

    #[test]
    fn pause_poll_rest_is_recorded_before_each_sleep_not_after() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": true, "reason": "thermal", "state": "serious"}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = RestVisibleAtSleepSleeper {
            traj_path: tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
            rests_seen_at_sleep: Default::default(),
            flip_pause_off: Some(tmp.path().to_path_buf()),
        };
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;
        let mut expiry_warned = false;
        honor_pace_pause(
            &mut reader, tmp.path(), 900_000, 600, &mut expiry_warned, &sleeper, &mut traj, 3,
            &mut last_pow, &mut soft_fired,
        );
        assert_eq!(sleeper.rests_seen_at_sleep.borrow().as_slice(), &[1], "the poll increment's rest event exists when its sleep begins");
    }

    /// (#2774 review F3) The WIRING, not the function. Seven tests call
    /// `apply_pace_duty_cycle_delay` directly and `honor_pace_pause` — the
    /// only thing that calls it in production — had no direct test at all,
    /// so deleting all eleven lines of the call left the whole crate green
    /// while tier 2's ONLY mechanism did nothing. This drives
    /// `honor_pace_pause` with a duty-cycle pace file and asserts the
    /// injected sleeper saw the host-set delay.
    #[test]
    fn honor_pace_pause_applies_the_host_set_duty_cycle_delay() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            r#"{"pause": false, "reason": "thermal-duty-cycle", "state": "fair", "turn_delay_ms": 15000}"#,
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;
        let mut expiry_warned = false;

        honor_pace_pause(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &mut expiry_warned,
            &sleeper,
            &mut traj,
            3,
            &mut last_pow,
            &mut soft_fired,
        );

        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            &[15_000],
            "the turn boundary must actually rest for the host-set duty-cycle delay — \
             this is tier 2's only mechanism"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 15_000);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 1);
        // `pause: false` means the pause loop itself breaks immediately, so
        // the 15s above is the duty-cycle prelude and nothing else.
        assert_eq!(sleeper.calls.borrow().len(), 1, "no 2s pause-poll increments on a non-paused file");
    }

    /// The other half: with no `turn_delay_ms` at all, `honor_pace_pause`
    /// rests for nothing. Without this, the test above could pass off a
    /// hard-coded sleep as the wiring.
    #[test]
    fn honor_pace_pause_rests_for_nothing_when_the_file_carries_no_turn_delay() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": false, "reason": "thermal", "state": "nominal"}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;
        let mut expiry_warned = false;

        honor_pace_pause(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &mut expiry_warned,
            &sleeper,
            &mut traj,
            3,
            &mut last_pow,
            &mut soft_fired,
        );

        assert!(sleeper.calls.borrow().is_empty(), "nothing to duty-cycle, nothing to wait for");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 0);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 0);
    }

    #[test]
    fn duty_cycle_delay_goes_through_the_same_budget_clamp_as_the_configured_delay() {
        // budget=10s -> half=5000ms; a host-set 15000ms must clamp exactly
        // like an operator-configured turn_delay_ms would (#2094's own
        // clamp, reused rather than duplicated for this new caller).
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": false, "turn_delay_ms": 15000}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            10,
            &sleeper,
            &mut traj,
            1,
            &mut last_pow,
            &mut soft_fired,
        );

        assert_eq!(sleeper.calls.borrow().as_slice(), &[5_000], "clamped to half the 10s budget");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 5_000);
    }

    #[test]
    fn duty_cycle_delay_is_a_no_op_when_the_pace_file_is_paused() {
        // pause:true means tier 3, not tier 2 — the caller's own while-loop
        // owns that case; this prelude must do nothing so it never
        // double-rests on top of a full pause.
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": true, "reason": "thermal", "turn_delay_ms": 15000}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &sleeper,
            &mut traj,
            1,
            &mut last_pow,
            &mut soft_fired,
        );

        assert!(sleeper.calls.borrow().is_empty(), "a paused pace file must never trigger a duty-cycle sleep");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 0);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 0);
    }

    #[test]
    fn duty_cycle_delay_is_a_no_op_with_no_turn_delay_field() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": false}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &sleeper,
            &mut traj,
            1,
            &mut last_pow,
            &mut soft_fired,
        );

        assert!(sleeper.calls.borrow().is_empty());
    }

    #[test]
    fn duty_cycle_delay_is_a_no_op_when_zero() {
        // `turn_delay_ms: 0` is written by nothing today, but must be a
        // true no-op rather than a zero-length "sleep" call — mirrors the
        // `#2094` static path's own `ms > 0` guard in `RealSleeper::sleep`.
        let tmp = tempfile::tempdir().unwrap();
        write_pace(tmp.path(), r#"{"pause": false, "turn_delay_ms": 0}"#);
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &sleeper,
            &mut traj,
            1,
            &mut last_pow,
            &mut soft_fired,
        );

        assert!(sleeper.calls.borrow().is_empty());
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 0);
    }

    #[test]
    fn duty_cycle_delay_is_a_no_op_when_the_pace_file_has_expired() {
        // A `turn_delay_ms` instruction whose writer went dark past
        // `max_pause_ms` is abandoned, same staleness rule a pause uses —
        // must not be honored forever just because nobody re-stamped it.
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            r#"{"pause": false, "turn_delay_ms": 15000, "written_at_ms": 1}"#,
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            /* max_pause_ms */ 1_000,
            600,
            &sleeper,
            &mut traj,
            1,
            &mut last_pow,
            &mut soft_fired,
        );

        assert!(sleeper.calls.borrow().is_empty(), "an abandoned duty-cycle instruction must not be honored");
    }

    /// (#2774 round-9 MF2) A FUTURE-dated stamp must decay, not be
    /// honored forever.
    ///
    /// `PaceFile::is_expired` computes `now_ms.saturating_sub(written_at)`,
    /// which clamps to `0` for a stamp ahead of the reader's clock — so
    /// the file reads as maximally fresh for as long as that same stamp
    /// sits there, which `pace.rs`'s module doc names as "exactly the
    /// infinite-hold failure mode the heartbeat design exists to prevent."
    /// `PaceReader::pause_is_expired` adds the one-grace-interval guard
    /// over it, and had been wired to ONE of its two call sites: the pause
    /// path had it, the duty-cycle prelude called the raw fn.
    ///
    /// Measured against the raw fn: five reads through a fresh
    /// `PaceReader` slept the full 15,000ms every time — `calls ==
    /// [15000; 5]`, no decay. The container's Docker VM clock running
    /// ahead of the host is a skew direction `pace.rs` documents as real
    /// and expected, so a long crawl would throttle at every turn boundary
    /// for the rest of the dispatch with the governor dead and unable to
    /// re-stamp it away.
    #[test]
    fn a_future_dated_duty_cycle_stamp_decays_instead_of_holding_forever() {
        let tmp = tempfile::tempdir().unwrap();
        // ~10^10 ms ahead of any plausible `unix_ms()` — the same shape a
        // VM clock running ahead of the host produces.
        let far_future = checkpoint::unix_ms().saturating_add(10_000_000_000);
        write_pace(
            tmp.path(),
            &format!(
                r#"{{"pause": false, "turn_delay_ms": 15000, "written_at_ms": {far_future}}}"#
            ),
        );
        // ONE reader across every tick — the grace is tracked on the
        // reader, exactly as it is in production where `run_with_sleeper`
        // owns a single `PaceReader` for the whole dispatch.
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        for turn in 0..5 {
            apply_pace_duty_cycle_delay(
                &mut reader,
                tmp.path(),
                900_000,
                600,
                &sleeper,
                &mut traj,
                turn,
                &mut last_pow,
                &mut soft_fired,
            );
        }

        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            &[15_000],
            "one grace interval, then the same unchanged future-dated stamp is treated as an \
             abandoned instruction — not honored at every turn boundary for the rest of the \
             dispatch"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 15_000);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 1);
    }

    /// The inverse of the test above, so the guard cannot be "fixed" by
    /// making every duty-cycle instruction expire after one tick. A live,
    /// correctly-stamped instruction is honored at EVERY turn boundary.
    #[test]
    fn a_live_duty_cycle_instruction_is_honored_at_every_turn_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            &format!(
                r#"{{"pause": false, "turn_delay_ms": 15000, "written_at_ms": {}}}"#,
                checkpoint::unix_ms()
            ),
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        for turn in 0..5 {
            apply_pace_duty_cycle_delay(
                &mut reader,
                tmp.path(),
                900_000,
                600,
                &sleeper,
                &mut traj,
                turn,
                &mut last_pow,
                &mut soft_fired,
            );
        }

        assert_eq!(sleeper.calls.borrow().as_slice(), &[15_000; 5]);
    }

    #[test]
    fn duty_cycle_delay_records_a_distinguishable_trajectory_event() {
        // (#2774 constraint 1: must not look like a stall) The event must
        // carry a reason a reader can tell apart from an ordinary
        // operator-configured turn_delay rest AND from a full thermal
        // pause — "thermal-duty-cycle" is neither "turn_delay" nor
        // "thermal"/"thermal-critical".
        let tmp = tempfile::tempdir().unwrap();
        write_pace(
            tmp.path(),
            r#"{"pause": false, "state": "fair", "turn_delay_ms": 15000}"#,
        );
        let mut reader = pace::PaceReader::new();
        let sleeper = DutyCycleSleeper::default();
        let mut traj = Trajectory::open(tmp.path());
        let mut last_pow = std::time::Instant::now();
        let mut soft_fired = false;

        apply_pace_duty_cycle_delay(
            &mut reader,
            tmp.path(),
            900_000,
            600,
            &sleeper,
            &mut traj,
            7,
            &mut last_pow,
            &mut soft_fired,
        );
        drop(traj);

        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let body = std::fs::read_to_string(&traj_file).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(body.lines().find(|l| !l.contains("trajectory.header")).unwrap()).unwrap();
        assert_eq!(event["reason"], "thermal-duty-cycle");
        assert_eq!(event["state"], "fair");
        assert_eq!(event["seq"], 7);
        assert_eq!(event["ms"], 15_000);
    }

    /// (#2094 finding 3b) The CALL SITE's bundled effect, exercised
    /// through the exact scenario the finding names: a rest whose
    /// duration consumes more than 75% of the inactivity budget.
    ///
    /// Constructed entirely via `Instant` arithmetic (subtraction), the
    /// same trick the tests above already use — no real sleep. `now -
    /// Duration::from_secs(9)` is a value that is GENUINELY 9 real seconds
    /// in the past relative to whenever `.elapsed()` is called on it next
    /// (computed by subtraction at construction time, not by waiting), so
    /// this is a legitimate clock reading, not a faked one.
    #[test]
    fn a_rest_consuming_over_75pct_of_budget_prevents_the_soft_warning_from_firing() {
        let budget_secs = 10u64;
        let soft_threshold_secs = inactivity_soft_threshold_secs(budget_secs);
        assert_eq!(soft_threshold_secs, 7, "sanity: 75% of a 10s budget floors to 7s");

        // Absent the fix, this dispatch has already gone 9s without a
        // proof-of-work reset — past the 7s soft threshold, so the warning
        // WOULD fire on the next check.
        let last_proof_of_work = std::time::Instant::now() - std::time::Duration::from_secs(9);
        assert!(
            last_proof_of_work.elapsed().as_secs() >= soft_threshold_secs,
            "sanity: without the rest, the soft warning WOULD already be due to fire"
        );

        // The rest itself: 8000ms, comfortably over 75% of the 10s budget
        // (7500ms) — GPU-relief pacing, not a stall.
        let (extended, warning_flag) =
            absorb_rest_into_soft_inactivity_clock(last_proof_of_work, 8_000);

        assert!(!warning_flag, "a rest must clear the edge-trigger warning flag");
        assert!(
            extended.elapsed().as_secs() < soft_threshold_secs,
            "the rest must buy back enough headroom that an immediate soft \
             check does not fire — the harness-owned idle time must not be \
             mistaken for a stall"
        );
    }

    /// The two effects a fired rest has on the soft-inactivity clock,
    /// pinned as a UNIT so the call site cannot apply one without the
    /// other (deleting the call to this function at the loop's rest block
    /// is what finding 3b's mutation proof exercises).
    #[test]
    fn absorb_rest_into_soft_inactivity_clock_extends_and_clears_the_flag() {
        let now = std::time::Instant::now();
        let (extended, warning_flag) = absorb_rest_into_soft_inactivity_clock(now, 500);
        assert_eq!(extended, now + std::time::Duration::from_millis(500));
        assert!(!warning_flag);
    }

    // ---------------------------------------------------------------
    // (#2094) The inter-turn rest, driven through a real scripted loop —
    // proves the wiring (guard placement, sleeper injection, trajectory +
    // outcome accounting), not just the arithmetic tested in isolation
    // above.
    // ---------------------------------------------------------------

    /// Records every sleep call without blocking — the harness never
    /// actually waits (the "no test sleeps for real longer than 10ms"
    /// discipline this project holds tests to).
    #[derive(Default)]
    struct RecordingSleeper {
        calls: std::cell::RefCell<Vec<u64>>,
    }
    impl TurnSleeper for RecordingSleeper {
        fn sleep(&self, ms: u64) {
            self.calls.borrow_mut().push(ms);
        }
    }

    /// Register a 3-response script on `server`: two `tool_calls` turns
    /// followed by a `stop`. Mocks are mutually exclusive on how many
    /// `"role":"tool"` substrings the accumulating request body carries —
    /// the same keyed-mock trick
    /// `an_empty_call_does_not_discard_the_work_already_banked` uses,
    /// generalized to 3 states. Reuses the exact tool/args
    /// `assistant_messages_in_history_never_carry_reasoning_content` does
    /// (a `read` call on `/workspace/x.txt`), known to round-trip cleanly
    /// with no Docker/real LMStudio involved.
    ///
    /// `#[track_caller]` (#2599): this helper has 3 callers, and without
    /// it every unhit-mock panic from `GuardedMockServer`'s Drop check
    /// would name the SAME line inside this function for all 3 — the
    /// `#[track_caller]` on `GuardedMockServer::register` only propagates
    /// through a chain of `#[track_caller]` functions, so an un-annotated
    /// intermediate like this one used to stop that propagation cold.
    /// With it, a shadowed mock this helper registers is attributed to
    /// whichever TEST called it, not to this function's own body.
    ///
    /// Not red-provable here, by nature rather than by gap: all 3 real
    /// callers register mocks that are always hit, so `GuardedMockServer`'s
    /// Drop-time check never fires at this call site and removing the
    /// annotation leaves the whole suite green (confirmed by mutation —
    /// you cannot red-prove a diagnostic for a bug that does not exist).
    /// The mechanism itself is proved on the ANALOG self-test instead —
    /// `test_support::self_tests::
    /// a_helper_marked_track_caller_reports_the_calling_tests_own_line`
    /// builds a helper with a deliberately unhit mock and shows two
    /// distinct callers report two distinct lines — and that proof
    /// transfers here because both helpers share the same shape (an
    /// un-annotated intermediate between the test and
    /// `GuardedMockServer::register`), not because this call site was
    /// itself observed failing without the annotation.
    #[track_caller]
    fn register_three_turn_tool_then_stop_script(server: &crate::test_support::GuardedMockServer) {
        use httpmock::prelude::*;
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        let tc1 = tool_calls.clone();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(None, Some(tc1.clone()), "tool_calls", 100, 20));
        });
        let tc2 = tool_calls.clone();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1
            });
            then.status(200).json_body(chat_response_json(None, Some(tc2.clone()), "tool_calls", 120, 20));
        });
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 2
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });
    }

    #[test]
    #[serial_test::serial]
    fn a_three_turn_dispatch_rests_exactly_twice_between_turns() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::set_var("DARKMUX_TURN_DELAY_MS", "500");

        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-delay-3").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let sleeper = RecordingSleeper::default();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 3, "sanity: three logical turns");
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [500, 500],
            "rests fire BETWEEN turns only — 2 rests for 3 turns, never before the first"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 1000, "LoopOutcome carries the same sum the sleeper saw");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 2);
        assert_eq!(
            outcome.turn_delay_effective_ms, 500,
            "(#2094 finding 8) the POST-CLAMP cadence actually applied, not the raw config"
        );

        drop(traj);
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let body = std::fs::read_to_string(&traj_file).unwrap();
        let rest_events = body.lines().filter(|l| l.contains("\"type\":\"runtime.rest\"")).count();
        assert_eq!(rest_events, 2, "one runtime.rest trajectory event per rest");
    }

    /// (#2877, pre-PR review) The turn-delay rest, the most common one, had
    /// no test that it is recorded BEFORE its sleep: moving `append_rest`
    /// back after the sleep left the whole crate green. Same real loop as
    /// the test above; at each sleep the matching rest event must already
    /// be in the trajectory.
    #[test]
    #[serial_test::serial]
    fn turn_delay_rest_is_recorded_before_each_sleep_not_after() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::set_var("DARKMUX_TURN_DELAY_MS", "500");
        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-delay-order").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let sleeper = RestVisibleAtSleepSleeper {
            traj_path: tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
            rests_seen_at_sleep: Default::default(),
            flip_pause_off: None,
        };
        run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 2);
        assert_eq!(
            sleeper.rests_seen_at_sleep.borrow().as_slice(),
            &[1, 2],
            "each turn-delay rest is on disk when its sleep begins"
        );
    }

    /// (#2114) A sleeper that, on its SECOND call, flips `pace.json` to
    /// `pause: false` — simulating a governor rewriting the file WHILE the
    /// loop is inside a poll increment's sleep. Records every call like
    /// `RecordingSleeper`.
    struct PaceFlippingSleeper {
        calls: std::cell::RefCell<Vec<u64>>,
        out_dir: std::path::PathBuf,
    }
    impl TurnSleeper for PaceFlippingSleeper {
        fn sleep(&self, ms: u64) {
            self.calls.borrow_mut().push(ms);
            std::fs::write(pace::pace_file_path(&self.out_dir), r#"{"pause": false}"#).unwrap();
        }
    }

    #[test]
    #[serial_test::serial]
    fn pace_file_pause_then_resume_mid_sleep_emits_rest_events_and_continues() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("pace-flip").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // Pause is already active when the dispatch starts.
        std::fs::write(
            pace::pace_file_path(tmp.path()),
            r#"{"pause": true, "reason": "thermal"}"#,
        )
        .unwrap();

        let sleeper = PaceFlippingSleeper {
            calls: std::cell::RefCell::new(Vec::new()),
            out_dir: tmp.path().to_path_buf(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok even though it paused mid-run");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 3,
            "the dispatch still completes normally once the pause lifts"
        );
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [2_000],
            "one bounded ≤2s poll increment: pause was true on entry, the sleeper's own write \
             flips it to false MID-sleep, so the very next re-read breaks the poll loop \
             without a second increment"
        );

        drop(traj);
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let body = std::fs::read_to_string(&traj_file).unwrap();
        let rest_events: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["type"] == "runtime.rest")
            .collect();
        assert_eq!(rest_events.len(), 1, "one paced-rest event for the one increment taken");
        assert_eq!(rest_events[0]["ms"], 2_000);
        assert_eq!(rest_events[0]["reason"], "thermal", "the pace file's reason is stamped on the event");
    }

    /// (#2774 round-9, the sweep's first CONSIDER) The duty-cycle twin of
    /// the pause test above, through the SAME real mock-server loop.
    ///
    /// Seven unit tests called `apply_pace_duty_cycle_delay` directly and
    /// one called `honor_pace_pause`, but nothing drove tier 2 through
    /// `run_with_sleeper` the way `pace_file_pause_then_resume_mid_sleep_…`
    /// drives the pause path — so a defect in which staleness rule the
    /// prelude uses (round-9 MF2) had no end-to-end test that could see
    /// it. This one pins both halves of what tier 2 promises: the delay is
    /// applied at every turn boundary, and the dispatch still RUNS TO
    /// COMPLETION with its full turn count, because a duty cycle is
    /// pacing, not a stop.
    #[test]
    #[serial_test::serial]
    fn a_duty_cycle_instruction_paces_every_turn_and_the_dispatch_still_completes() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("duty-cycle-e2e").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // Exactly what `darkmux_crew::thermal_governor` writes on entering
        // tier 2 — `pause: false` with a `turn_delay_ms`, freshly stamped
        // so the heartbeat contract reads it as live.
        std::fs::write(
            pace::pace_file_path(tmp.path()),
            format!(
                r#"{{"pause": false, "reason": "thermal-duty-cycle", "state": "fair", "turn_delay_ms": 15000, "written_at_ms": {}}}"#,
                checkpoint::unix_ms()
            ),
        )
        .unwrap();

        // A plain recorder: nothing rewrites the pace file, so the
        // instruction stays live for the whole dispatch — the production
        // shape while a machine sits in the `fair` band.
        let sleeper = RecordingSleeper::default();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &sleeper,
        )
        .expect("a duty-cycled dispatch still completes");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 3,
            "a duty cycle is PACING, not a stop: every turn still runs, just slower"
        );
        let calls = sleeper.calls.borrow().clone();
        assert!(
            !calls.is_empty() && calls.iter().all(|&ms| ms == 15_000),
            "every rest this dispatch took is the host-set duty-cycle delay: {calls:?}"
        );

        drop(traj);
        let body =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        let rest_events: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["type"] == "runtime.rest")
            .collect();
        assert_eq!(
            rest_events.len(),
            calls.len(),
            "one runtime.rest artifact event per rest actually taken"
        );
        assert!(
            rest_events.iter().all(|e| e["reason"] == "thermal-duty-cycle"),
            "the run's own artifact must say it was duty-cycled, not merely that it rested: \
             {rest_events:?}"
        );
        assert!(
            rest_events.iter().all(|e| e["state"] == "fair"),
            "…and the reading the governor decided on: {rest_events:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_battery_pause_parks_the_dispatch_and_it_resumes_with_nothing_lost() {
        // (#2706) The end-to-end pause-and-resume proof for the BATTERY
        // reason, driven through the real loop rather than asserted about
        // it. The runtime treats `reason` as opaque text
        // (`PaceFile::reason_or_default`), so a battery pause must be
        // honored exactly as a thermal one is — and, decisively, the
        // dispatch must still run to completion with the SAME turn count
        // once the pause lifts. "Nothing lost" is not a claim here; it is
        // three recorded turns.
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("battery-pause").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // Exactly what `darkmux_crew::power_policy::BatteryGovernor` writes
        // when charge crosses the operator's floor — reason and state
        // spelled the way that governor spells them.
        std::fs::write(
            pace::pace_file_path(tmp.path()),
            r#"{"pause": true, "reason": "battery", "state": "38%"}"#,
        )
        .unwrap();

        let sleeper = PaceFlippingSleeper {
            calls: std::cell::RefCell::new(Vec::new()),
            out_dir: tmp.path().to_path_buf(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &sleeper,
        )
        .expect("the dispatch completes once power returns");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 3,
            "a battery pause is a REST, not a stop: every turn the dispatch would have run still \
             runs once the pause lifts"
        );
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [2_000],
            "the pause is honored in bounded ≤2s increments, so a flip back to `pause: false` is \
             picked up within one increment rather than after a long sleep"
        );

        drop(traj);
        let body =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        let rest_events: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["type"] == "runtime.rest")
            .collect();
        assert_eq!(rest_events.len(), 1);
        assert_eq!(
            rest_events[0]["reason"], "battery",
            "the run's own artifact must say WHY it rested, not merely that it did"
        );
        assert_eq!(
            rest_events[0]["state"], "38%",
            "and how much charge was left — the governor stamps the reading its decision was \
             made on, and the runtime echoes it verbatim"
        );
    }

    /// (#2114 finding 1) A sleeper that, on its FIRST call — i.e. while the
    /// loop is INSIDE the pace-wait poll, still parked at the boundary —
    /// reads `checkpoint.json` and asserts it already reflects THIS
    /// boundary's turn count, not the previous one. If the checkpoint
    /// write happens after the pace wait (the bug), a kill signal that
    /// arrives while parked here — a real host SIGKILL, simulated by this
    /// test just reading the file instead of exiting — would find either
    /// no checkpoint at all or one a full turn behind. The second call
    /// flips the pace file to `pause: false` so the dispatch can finish.
    struct AssertCheckpointFreshWhileParkedSleeper {
        calls: std::cell::RefCell<u32>,
        out_dir: std::path::PathBuf,
        expected_turns_while_parked: u32,
    }
    impl TurnSleeper for AssertCheckpointFreshWhileParkedSleeper {
        fn sleep(&self, _ms: u64) {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                let checkpoint = checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(
                    &self.out_dir,
                ))
                .expect(
                    "(#2114 finding 1) a checkpoint must already be on disk while parked at \
                     the pause boundary — a kill here must not lose a whole turn",
                );
                assert_eq!(
                    checkpoint.turns, self.expected_turns_while_parked,
                    "(#2114 finding 1) the on-disk checkpoint must reflect THIS boundary's \
                     turn count, not a stale one written before the pace wait"
                );
                std::fs::write(
                    pace::pace_file_path(&self.out_dir),
                    r#"{"pause": false}"#,
                )
                .unwrap();
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn checkpoint_is_fresh_while_parked_at_a_pause_boundary() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        // A resumed dispatch (turns == 1 from the START, no request sent
        // yet in THIS process) rather than a fresh 3-turn one: it puts the
        // loop at a REAL post-turn-1 boundary on its very first iteration,
        // so pausing there meaningfully exercises "does the on-disk
        // checkpoint reflect turns==1" — a fresh dispatch paused before
        // turn 1 would trivially have no checkpoint yet regardless of this
        // fix, since nothing has happened.
        let server = crate::test_support::GuardedMockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("pace-fresh-checkpoint").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),
                Message::user("read x.txt"),
                Message {
                    role: "assistant".into(),
                    content: None,
                    tool_calls: Some(serde_json::from_value(tool_calls).unwrap()),
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                },
                Message::tool_result("call_1", "read", "<turn 1 file contents>"),
            ],
            turns: 1,
            total_completion_tokens: 20,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        // Pause is already active when the dispatch starts — the resumed
        // loop's FIRST iteration is already at the turns==1 boundary, so
        // it parks there immediately, before ever writing a checkpoint IN
        // THIS PROCESS. Nothing on disk yet is exactly the scenario a real
        // kill-then-restart hits: the in-memory `resume_seed` is not
        // itself a file on disk.
        std::fs::write(
            pace::pace_file_path(tmp.path()),
            r#"{"pause": true, "reason": "thermal"}"#,
        )
        .unwrap();

        let sleeper = AssertCheckpointFreshWhileParkedSleeper {
            calls: std::cell::RefCell::new(0),
            out_dir: tmp.path().to_path_buf(),
            expected_turns_while_parked: 1,
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok even though it paused mid-run");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(*sleeper.calls.borrow(), 1, "sanity: the sleeper's assertion actually ran");
    }

    #[test]
    #[serial_test::serial]
    fn checkpoint_written_after_turn_1_has_matching_message_count() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        let tc1 = tool_calls.clone();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(None, Some(tc1.clone()), "tool_calls", 100, 20));
        });
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 1
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 120, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("checkpoint-turn1").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("2-turn scripted dispatch (tool call, then stop) returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 2, "sanity: a tool-call turn followed by a stop turn");

        let ckpt = checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(tmp.path()))
            .expect("checkpoint written at the turn-1/turn-2 boundary, before turn 2's request");
        assert_eq!(ckpt.turns, 1, "captured right after turn 1 completed");
        assert_eq!(
            ckpt.messages.len(),
            4,
            "system + user + assistant(tool_calls) + tool result — matches the loop's own \
             `messages` at that boundary"
        );
        assert!(ckpt.pending_hand_back.is_none(), "a clean turn boundary, not a #1221 continuation");
    }

    #[test]
    #[serial_test::serial]
    fn resume_from_two_turn_checkpoint_begins_at_turn_three_without_rerunning_tool_calls() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        // Turns 1 and 2 — a RESUMED dispatch must NEVER hit these; it
        // starts directly at the request a fresh dispatch would send as
        // its THIRD call.
        let tc1 = tool_calls.clone();
        let turn1_mock = server.mock_expect_zero(
            "a resumed dispatch must NEVER hit turn 1 again — assert_hits(0) below already \
             pins this; this declares the zero legitimate to GuardedMockServer too",
            move |when, then| {
                when.method(POST).path("/v1/chat/completions").matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    b.matches("\"role\":\"tool\"").count() == 0
                });
                then.status(200).json_body(chat_response_json(None, Some(tc1.clone()), "tool_calls", 100, 20));
            },
        );
        let tc2 = tool_calls.clone();
        let turn2_mock = server.mock_expect_zero(
            "a resumed dispatch must NEVER re-request turn 2 either — assert_hits(0) below \
             already pins this; this declares the zero legitimate to GuardedMockServer too",
            move |when, then| {
                when.method(POST).path("/v1/chat/completions").matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    b.matches("\"role\":\"tool\"").count() == 1
                });
                then.status(200).json_body(chat_response_json(None, Some(tc2.clone()), "tool_calls", 120, 20));
            },
        );
        let turn3_mock = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 2
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-test").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let assistant_tool_call = |tc: &serde_json::Value| Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(serde_json::from_value(tc.clone()).unwrap()),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        // A checkpoint as if turns 1+2 already ran: system + user +
        // assistant(tool_calls) + tool_result, twice.
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),
                Message::user("read x.txt"),
                assistant_tool_call(&tool_calls),
                Message::tool_result("call_1", "read", "<turn 1 file contents>"),
                assistant_tool_call(&tool_calls),
                Message::tool_result("call_1", "read", "<turn 2 file contents>"),
            ],
            turns: 2,
            total_completion_tokens: 40,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        turn1_mock.assert_hits(0);
        turn2_mock.assert_hits(0);
        turn3_mock.assert_hits(1);

        // (#2263) What THIS invocation recorded is turn 3 alone: one call,
        // turn 3's own usage (140 prompt + 5 completion from
        // `chat_response_json` above), never the checkpoint's 220+40 folded
        // in, or this run's cost gets misattributed to whatever model made
        // the resumed call.
        let this_run = crate::trajectory::recorded(tmp.path());
        assert_eq!(this_run.turns(), 1, "this invocation made exactly ONE call (turn 3)");
        assert_eq!(this_run.tokens.prompt, 140);
        assert_eq!(this_run.tokens.completion, 5);
        assert_eq!(this_run.compactions(), 0, "cfg is never_compact");
        // The turn it stopped on is the task's own turn 3, not this run's
        // first call.
        assert_eq!(this_run.turn_detail.keys().copied().max(), Some(3));
    }

    /// (#2263) A resume from a #1221 hand-back checkpoint CONTINUES the
    /// checkpoint's turn rather than starting the next: its call is recorded
    /// under `seq` = the checkpoint's turn count, and the whole dispatch has
    /// made that many turns, not one more.
    #[test]
    #[serial_test::serial]
    fn a_hand_back_resume_continues_the_checkpoints_turn() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-hand-back").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![Message::system("test"), Message::user("think it through")],
            turns: 3,
            total_completion_tokens: 60,
            compactions: 0,
            pending_hand_back: Some(checkpoint::PendingHandBack {
                thought: "working through the first half".to_string(),
                answer: String::new(),
                think_closed: false,
                is_reasoning: true,
                carries_own_opener: false,
            }),
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &[], &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("a hand-back resume returns Ok");
        drop(traj);

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        let this_run = crate::trajectory::recorded(tmp.path());
        assert_eq!(this_run.turn_detail.keys().copied().collect::<Vec<_>>(), vec![3], "the continued turn keeps its seq");
    }

    /// (#2263) The inverted case: a dispatch that was NEVER resumed records
    /// every turn it ran and every call's usage. `resume_from: None` below is
    /// the only difference from the resumed scenario above; a 2-turn
    /// scripted dispatch (tool call, then stop) matches
    /// `checkpoint_written_after_turn_1_has_matching_message_count`'s own
    /// fixture shape.
    #[test]
    #[serial_test::serial]
    fn a_never_resumed_dispatch_records_every_turn_and_its_usage() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        let tc1 = tool_calls.clone();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(None, Some(tc1.clone()), "tool_calls", 100, 20));
        });
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 1
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 120, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("no-resume-parity").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("2-turn scripted dispatch (tool call, then stop) returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        let recorded = crate::trajectory::recorded(tmp.path());
        assert_eq!(recorded.turns(), 2);
        assert_eq!(recorded.compactions(), 0);
        assert_eq!(recorded.tokens.prompt, 220, "100 (turn 1) + 120 (turn 2)");
        assert_eq!(recorded.tokens.completion, 25, "20 (turn 1) + 5 (turn 2)");
    }

    /// A reply whose `usage` reports a completion count but no prompt count
    /// (the shared provider parse keeps what was sent) still finishes its
    /// turn, counts the completion it reported, and calibrates nothing on a
    /// prompt count it never received: no context-window event, no zero.
    #[test]
    #[serial_test::serial]
    fn a_reply_without_a_prompt_count_counts_what_it_reported_and_calibrates_nothing() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": "done" }, "finish_reason": "stop" }],
                "usage": { "completion_tokens": 5 },
            }));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("usage-no-prompt").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &[], &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a short usage block does not fail the turn");
        drop(traj);

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        let recorded = crate::trajectory::recorded(tmp.path());
        assert_eq!(recorded.tokens.completion, 5);
        assert_eq!(recorded.tokens.prompt, 0);
        let raw = std::fs::read_to_string(darkmux_trajectory::trajectory_path(tmp.path())).unwrap();
        assert!(!raw.contains("\"dispatch.context\""), "no prompt count, no context-window event: {raw}");
    }

    #[test]
    #[serial_test::serial]
    fn resume_mid_turn_dispatches_only_the_undispatched_tool_calls() {
        // (#2114 finding 2) A 3-tool turn killed after tool 1 must resume
        // by dispatching ONLY tools 2 and 3 — never re-running tool 1,
        // whose result the checkpoint already recorded.
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        // A request shaped like the ORIGINAL turn 1 (no tool results yet)
        // must NEVER land — proves the resume doesn't re-request the
        // model for a turn it already has an assistant message for.
        let original_turn1_mock = server.mock_expect_zero(
            "the resume must never re-request turn 1 — assert_hits(0) below already pins \
             this; this declares the zero legitimate to GuardedMockServer too",
            |when, then| {
                when.method(POST).path("/v1/chat/completions").matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    b.matches("\"role\":\"tool\"").count() == 0
                });
                then.status(200).json_body(chat_response_json(Some("should not be reached"), None, "stop", 100, 5));
            },
        );
        // The next real request comes only once ALL THREE tool results
        // (the checkpoint's one plus the two the resume dispatches) are
        // present.
        let turn2_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 3
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-mid-turn").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let call1 = make_call("call_1", 1);
        let call2 = make_call("call_2", 2);
        let call3 = make_call("call_3", 3);
        let assistant_message = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call1.clone(), call2.clone(), call3.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        // Simulates a kill right after tool 1's result landed but before
        // tools 2 and 3 ran: `messages` carries the assistant's 3-call
        // turn plus exactly ONE tool result, and `pending_tool_calls`
        // names the two that never got to run.
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),
                Message::user("read x.txt"),
                assistant_message,
                Message::tool_result("call_1", "read", "<call 1 result>"),
            ],
            turns: 1,
            total_completion_tokens: 20,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2, call3]),
            // call_1 (index 0) already completed, so the next pending
            // call (call_2) resumes at tool_seq 1.
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        original_turn1_mock.assert_hits(0);
        turn2_mock.assert_hits(1);

        let tool_results: Vec<&Message> =
            outcome.messages.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_results.len(), 3, "checkpoint's 1 + resumed 2 = 3, never 4");
        let call_1_results = tool_results
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .count();
        assert_eq!(call_1_results, 1, "call_1 must NOT be re-dispatched and re-appended");
        for id in ["call_2", "call_3"] {
            assert_eq!(
                tool_results.iter().filter(|m| m.tool_call_id.as_deref() == Some(id)).count(),
                1,
                "{id} must be dispatched exactly once during the resume catch-up pass"
            );
        }

        // The checkpoint written after the LAST resumed tool call must show
        // a clean boundary (no calls still pending) so a SUBSEQUENT kill
        // wouldn't try to re-derive an already-finished batch.
        let final_mid_turn_checkpoint =
            checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(tmp.path())).unwrap();
        assert!(
            final_mid_turn_checkpoint.pending_tool_calls.is_none()
                || final_mid_turn_checkpoint.turns > 1,
            "either the last mid-turn checkpoint cleared pending_tool_calls, or a later \
             clean-boundary checkpoint (turns > 1) has already superseded it"
        );
    }

    /// (#3074) Resume a checkpoint killed with a bash call pending; returns the
    /// text of the tool result the model is handed for it. `head_started` is the
    /// marker under test. The host has no `/workspace`, so an executed bash call
    /// here comes back as a spawn error rather than output, which is enough to
    /// tell "ran" from "surfaced".
    fn resume_with_pending_bash(tmp: &std::path::Path, head_started: bool) -> String {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");
        let server = crate::test_support::GuardedMockServer::start();
        let _next = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let mut traj = Trajectory::open(tmp);
        let call = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "bash".into(),
                arguments: r#"{"command":"echo ran >> out.txt","timeout_seconds":5}"#.into(),
            },
            extra_content: None,
        };
        let assistant = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![Message::system("test"), Message::user("append"), assistant],
            turns: 1,
            total_completion_tokens: 20,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call]),
            pending_tool_calls_seq_base: 0,
            pending_head_started: head_started,
            written_at_unix_ms: checkpoint::unix_ms(),
        };
        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &[Tool::Bash], &mut traj, false,
            &compaction::CompactionConfig::never_compact(),
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp, "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");
        outcome
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .and_then(|m| m.content.clone())
            .expect("the model must get a tool result for call_1")
    }

    #[test]
    #[serial_test::serial]
    fn resume_surfaces_a_started_mutating_call_instead_of_replaying_it() {
        let tmp = tempfile::Builder::new().prefix("resume-started").tempdir().unwrap();
        let result = resume_with_pending_bash(tmp.path(), true);
        assert!(result.contains("interrupted") && result.contains("NOT re-run"), "{result}");
        assert!(!result.contains("spawning bash"), "the call must not have been executed: {result}");
    }

    #[test]
    #[serial_test::serial]
    fn resume_still_dispatches_a_pending_call_that_had_not_started() {
        let tmp = tempfile::Builder::new().prefix("resume-unstarted").tempdir().unwrap();
        let result = resume_with_pending_bash(tmp.path(), false);
        assert!(result.contains("spawning bash"), "an unstarted pending call runs on resume: {result}");
        assert!(!result.contains("interrupted"), "{result}");
    }

    /// The marker has to be on disk BEFORE the tool runs, or a kill mid-tool
    /// leaves nothing to find. The stand-in dispatcher reads the checkpoint it
    /// can see at the moment the tool would execute.
    #[test]
    fn a_mutating_call_is_marked_started_before_it_is_dispatched() {
        let out_dir = tempfile::tempdir().unwrap();
        let call = ToolCall {
            id: "call_probe".into(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall { name: "bash".into(), arguments: "{}".into() },
            extra_content: None,
        };
        let pending = [call.clone()];
        let messages = [Message::system("test")];
        let start = checkpoint::ToolStart {
            out_dir: out_dir.path(),
            role_id: "test-role",
            messages: &messages,
            turns: 1,
            total_completion_tokens: 0,
            compactions: 0,
            pending: &pending,
            seq_base: 0,
        };
        let seen = std::cell::RefCell::new(None);
        let run = dispatch_marked(&start, &call, |name, _args| {
            *seen.borrow_mut() =
                checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(out_dir.path())).ok();
            crate::tools::ToolRun::text(format!("ran {name}"))
        });
        assert_eq!(run.result, "ran bash");
        let during = seen.into_inner().expect("a checkpoint existed while the tool ran");
        assert!(during.pending_head_started, "the marker must be set while the call runs");
        assert_eq!(during.pending_tool_calls.unwrap()[0].id, "call_probe");
    }

    /// (#3074 review) A resumed call that is reported instead of run is not a
    /// success: the trajectory records it as `failed` (ok:false), which is also
    /// what keeps the inactivity timer from resetting as if work happened.
    #[test]
    #[serial_test::serial]
    fn a_surfaced_resume_call_is_recorded_as_not_run_not_as_success() {
        let tmp = tempfile::Builder::new().prefix("resume-outcome").tempdir().unwrap();
        let _ = resume_with_pending_bash(tmp.path(), true);
        let body = std::fs::read_to_string(darkmux_trajectory::trajectory_path(tmp.path())).unwrap();
        let completed: Vec<serde_json::Value> = body
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "tool.completed")
            .collect();
        assert_eq!(completed.len(), 1, "{body}");
        assert_eq!(completed[0]["ok"], serde_json::json!(false), "{}", completed[0]);
        assert_eq!(completed[0]["outcome"], "failed");
        assert!(
            completed[0]["failure_reason"].as_str().unwrap().contains("not re-run"),
            "{}",
            completed[0]
        );
    }

    /// (#3074 review) The LIVE loop's call site marks a mutating call started
    /// before the tool runs. `dispatch` is the cfg(test) observer here, which
    /// reads checkpoint.json at the moment the tool would execute; swapping the
    /// site back to a plain dispatch leaves nothing on disk and fails this.
    #[test]
    #[serial_test::serial]
    fn the_live_loop_marks_a_mutating_call_started_before_it_runs() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");
        let server = crate::test_support::GuardedMockServer::start();
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"role\":\"tool\"");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 200, 10));
        });
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"role\":\"user\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "bash", "arguments": "{\"command\":\"true\"}" },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("live-marker").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = std::rc::Rc::clone(&seen);
        let out = tmp.path().to_path_buf();
        DISPATCH_OBSERVER.with(|o| {
            *o.borrow_mut() = Some(Box::new(move || {
                *observed.borrow_mut() =
                    checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(&out)).ok();
            }));
        });
        let outcome = run_with_sleeper(
            &client, &client, "test-model",
            vec![Message::system("test"), Message::user("run it")],
            &[Tool::Bash], &mut traj, false,
            &compaction::CompactionConfig::never_compact(),
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("two-turn dispatch returns Ok");
        DISPATCH_OBSERVER.with(|o| *o.borrow_mut() = None);
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        let during = seen.borrow_mut().take().expect("a checkpoint existed while the tool ran");
        assert!(during.pending_head_started, "the live site must mark the call started first");
        assert_eq!(during.pending_tool_calls.unwrap()[0].id, "call_1");
    }

    /// (#3074) The catch-up pass's own wiring: a started head never reaches
    /// the dispatcher, and a call that did go to it was marked first.
    #[test]
    fn catch_up_dispatch_skips_a_started_head_and_marks_the_rest() {
        let out_dir = tempfile::tempdir().unwrap();
        let mk = |id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall { name: "bash".into(), arguments: "{}".into() },
            extra_content: None,
        };
        let pending = [mk("a"), mk("b")];
        let messages = [Message::system("test")];
        let start = |idx: usize| checkpoint::ToolStart {
            out_dir: out_dir.path(),
            role_id: "test-role",
            messages: &messages,
            turns: 1,
            total_completion_tokens: 0,
            compactions: 0,
            pending: &pending[idx..],
            seq_base: idx as u32,
        };
        let seed = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".into(),
            messages: messages.to_vec(),
            turns: 1,
            total_completion_tokens: 0,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(pending.to_vec()),
            pending_tool_calls_seq_base: 0,
            pending_head_started: true,
            written_at_unix_ms: 0,
        };
        let never = |_: &str, _: &str| -> crate::tools::ToolRun { panic!("a started head must not be dispatched") };
        let head = catch_up_dispatch(Some(&seed), 0, &start(0), &pending[0], never);
        assert!(head.interrupted);
        assert!(head.run.result.contains("NOT re-run"), "{}", head.run.result);
        assert!(
            !checkpoint::checkpoint_file_path(out_dir.path()).exists(),
            "a surfaced call is not dispatched, so it is not marked again"
        );
        let second = catch_up_dispatch(Some(&seed), 1, &start(1), &pending[1], |_, _| {
            let during =
                checkpoint::read_checkpoint(&checkpoint::checkpoint_file_path(out_dir.path())).unwrap();
            assert!(during.pending_head_started && during.pending_tool_calls_seq_base == 1);
            crate::tools::ToolRun::text("ran".into())
        });
        assert!(!second.interrupted);
        assert_eq!(second.run.result, "ran");
    }

    #[test]
    #[serial_test::serial]
    fn resume_catch_up_preserves_tool_seq_continuity_in_trajectory() {
        // (#2114 finding N6) trajectory.jsonl's tool_seq numbering for a
        // resumed call must pick up exactly where the killed run left off
        // (pending_tool_calls_seq_base), not restart from 0 — otherwise
        // the SAME tool call shows two different tool_seq values across a
        // kill-and-resume: whatever the original run logged before the
        // kill, and then 0 again from the catch-up pass.
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        let _turn2_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 3
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-tool-seq").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let call1 = make_call("call_1", 1);
        let call2 = make_call("call_2", 2);
        let call3 = make_call("call_3", 3);
        let assistant_message = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call1.clone(), call2.clone(), call3.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        // Same shape as the sibling mid-turn-resume test: killed after
        // call_1 (tool_seq 0, already logged by the ORIGINAL — now dead —
        // process before this checkpoint was taken). seq_base=1 says
        // call_2 must log as tool_seq 1, call_3 as tool_seq 2.
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),
                Message::user("read x.txt"),
                assistant_message,
                Message::tool_result("call_1", "read", "<call 1 result>"),
            ],
            turns: 1,
            total_completion_tokens: 20,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2, call3]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);

        drop(traj);
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let body = std::fs::read_to_string(&traj_file).unwrap();
        let tool_completed_events: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["type"] == "tool.completed")
            .collect();
        assert_eq!(
            tool_completed_events.len(),
            2,
            "exactly the two catch-up-dispatched calls (call_1's tool.completed was logged \
             by the ORIGINAL process, not this resumed one)"
        );
        assert_eq!(
            tool_completed_events[0]["tool_seq"], 1,
            "call_2 (index 1 of the original 3-call turn) must log as tool_seq 1, not 0"
        );
        assert_eq!(
            tool_completed_events[1]["tool_seq"], 2,
            "call_3 (index 2 of the original 3-call turn) must log as tool_seq 2"
        );
    }

    /// (#2114 finding N7) A sleeper that, on its FIRST call — i.e. while
    /// the resume catch-up pass is honoring an active pace pause BEFORE
    /// dispatching anything — asserts that NO tool.completed event has
    /// landed in trajectory.jsonl yet. If the catch-up dispatched its
    /// calls before checking pace, this sleeper's first invocation would
    /// already be racing against (or arriving strictly after) a live
    /// tool dispatch. The second call flips pace off so the dispatch can
    /// finish.
    struct AssertNoToolDispatchedWhileParkedSleeper {
        calls: std::cell::RefCell<u32>,
        out_dir: std::path::PathBuf,
    }
    impl TurnSleeper for AssertNoToolDispatchedWhileParkedSleeper {
        fn sleep(&self, _ms: u64) {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                let traj_path = self.out_dir.join(".darkmux-runtime").join("trajectory.jsonl");
                if let Ok(body) = std::fs::read_to_string(&traj_path) {
                    assert!(
                        !body.contains("\"type\":\"tool.completed\""),
                        "(#2114 finding N7) a tool was dispatched before the pace pause was \
                         honored: {body}"
                    );
                }
                std::fs::write(pace::pace_file_path(&self.out_dir), r#"{"pause": false}"#).unwrap();
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn resume_catch_up_honors_pace_before_dispatching_the_first_pending_call() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        let _turn2_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-pace-first").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let call1 = make_call("call_1", 1);
        let call2 = make_call("call_2", 2);
        let assistant_message = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call1.clone(), call2.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),
                Message::user("read x.txt"),
                assistant_message,
                Message::tool_result("call_1", "read", "<call 1 result>"),
            ],
            turns: 1,
            total_completion_tokens: 20,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        // Pause is already active — the catch-up's FIRST call (call_2)
        // must not dispatch until this pace is honored/lifted.
        std::fs::write(
            pace::pace_file_path(tmp.path()),
            r#"{"pause": true, "reason": "thermal"}"#,
        )
        .unwrap();

        let sleeper = AssertNoToolDispatchedWhileParkedSleeper {
            calls: std::cell::RefCell::new(0),
            out_dir: tmp.path().to_path_buf(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &sleeper,
        )
        .expect("resumed dispatch returns Ok even though it paused before catch-up");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(*sleeper.calls.borrow(), 1, "sanity: the sleeper's assertion actually ran");
    }

    #[test]
    #[serial_test::serial]
    fn resume_catch_up_trims_oversized_old_tool_results_before_the_next_request() {
        // (#2114 finding N1) The resume catch-up pass must run the SAME
        // soft-trim pass the main loop's tool_calls arm runs after ITS
        // tool-dispatch loop — otherwise a resume that appends a batch of
        // results sails into the first post-resume request without ever
        // having had a chance to shrink an old oversized tool result.
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let server = crate::test_support::GuardedMockServer::start();
        // The ONLY registered mock requires the elision marker to be
        // present in the outgoing request body — if the trim never ran
        // (or ran AFTER this request instead of before it), the body
        // still carries the full untrimmed blob, this mock's `matches`
        // predicate fails to match, and the client call errors instead of
        // silently passing.
        let expects_trimmed_body_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.contains(crate::tool_result_prune::TOOL_RESULT_TRIM_MARKER_SENTINEL)
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 100, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-trim").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];
        // Compaction disabled so this test isolates the TRIM path — a
        // separate concern from N1's "or compacted" half, already
        // exercised by the pre-existing `loop_triggers_compaction_when_
        // threshold_crossed` test against the SAME shared trim+compact
        // call site this resume path now reuses.
        let cfg = compaction::CompactionConfig::never_compact();

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let assistant_with_call = |call: &ToolCall| Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![call.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        let big_call = make_call("call_big", 1);
        let filler_call = make_call("call_filler", 2);
        let c1 = make_call("call_1", 3);
        let c2 = make_call("call_2", 4);
        let c3 = make_call("call_3", 5);
        let assistant_turn3 = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![c1.clone(), c2.clone(), c3.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        // (#1391) TOOL_RESULT_TRIM_THRESHOLD_BYTES is 4000 — comfortably
        // exceeded so the trim actually elides a middle section.
        let oversized_body = "X".repeat(6_000);

        // Layout is deliberate: `TOOL_RESULT_TRIM_PRESERVE_RECENT` (6)
        // protects the LAST 6 messages from trimming, so `big_call`'s
        // result (index 3) needs at least one more message pair ahead of
        // it than the minimum, pushing it out of that protected window
        // once the catch-up pass appends its own two results.
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: vec![
                Message::system("test"),                                    // 0
                Message::user("read x.txt"),                                // 1
                assistant_with_call(&big_call),                             // 2
                Message::tool_result("call_big", "read", oversized_body.as_str()),  // 3 <- trim target
                assistant_with_call(&filler_call),                          // 4
                Message::tool_result("call_filler", "read", "small"),       // 5
                assistant_turn3,                                            // 6
                Message::tool_result("call_1", "read", "small"),            // 7
            ],
            turns: 3,
            total_completion_tokens: 60,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![c2, c3]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        expects_trimmed_body_mock.assert_hits(1);
    }

    #[test]
    #[serial_test::serial]
    fn a_three_turn_dispatch_never_rests_when_turn_delay_is_zero() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS"); // unset → 0 default

        let server = crate::test_support::GuardedMockServer::start();
        register_three_turn_tool_then_stop_script(&server);

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-delay-0").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let sleeper = RecordingSleeper::default();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok");

        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 3, "sanity: still three turns");
        assert!(sleeper.calls.borrow().is_empty(), "delay=0 must never sleep");
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 0);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 0);
        assert_eq!(
            outcome.turn_delay_effective_ms, 0,
            "(#2094 finding 8) known and zero, even though this dispatch never rested"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_one_turn_dispatch_never_rests() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::set_var("DARKMUX_TURN_DELAY_MS", "500");

        let server = crate::test_support::GuardedMockServer::start();
        let _m = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 50, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-delay-1turn").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();
        let sleeper = RecordingSleeper::default();

        run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("single-turn dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1);
        assert!(
            sleeper.calls.borrow().is_empty(),
            "a single turn has no prior turn to rest AFTER — never before the first request"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 0);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 0);
    }

    /// (#2094 finding 3a) The rest guard's `!resuming_after_checkpoint`
    /// term, exercised through a REAL checkpoint continuation — not just
    /// the simple multi-turn scripts above, none of which ever set
    /// `resuming_after_checkpoint` true.
    ///
    /// Script: turn 1 finishes via a tool call (`tool_calls`). Turn 2
    /// opens with a `length` response (a genuine checkpoint continuation —
    /// non-empty content, so it takes the checkpoint-judge branch and sets
    /// `resuming_after_checkpoint = true` for the NEXT iteration), then
    /// concludes via `stop`.
    ///
    /// Correct guard: rests exactly ONCE — between turn 1 and turn 2's
    /// first call. The continuation call (turn 2's `length` → `stop`
    /// hand-off) must NOT be treated as a fresh turn boundary and must
    /// NOT rest before it. Deleting `!resuming_after_checkpoint` from the
    /// guard makes it rest a SECOND time immediately before that
    /// continuation call too, since `turns > 0` is already true by then.
    #[test]
    #[serial_test::serial]
    fn a_checkpoint_continuation_does_not_rest_a_second_time() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;
        use httpmock::prelude::*;

        const CONTINUATION_MARKER: &str = "TURN2-CHECKPOINT-CONTINUATION-MARKER";

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::set_var("DARKMUX_TURN_DELAY_MS", "500");

        let server = crate::test_support::GuardedMockServer::start();
        // Call 1: turn 1 completes via a tool call — 0 "role":"tool"
        // substrings in the request body (nothing has executed yet).
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        // (#2164) Turn 1 carries a small closed think block alongside its
        // tool call so it demonstrates reasoning (`dispatch_has_reasoned`
        // flips true from `per_turn_reasoning`) — otherwise turn 2's first
        // call below would carry the ANSWER bound, not the 40-token
        // reasoning interval this test's checkpoint scenario depends on.
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                Some("<think>brief</think>"),
                Some(tool_calls.clone()),
                "tool_calls",
                100,
                20,
            ));
        });
        // Call 2: turn 2's FIRST call — 1 "role":"tool" substring (turn 1's
        // tool result), and the continuation marker is NOT in the request
        // body yet (this call is what introduces it). Responds `length`
        // with non-empty content so the checkpoint-judge branch fires and
        // sets `resuming_after_checkpoint = true`.
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1 && !b.contains(CONTINUATION_MARKER)
            });
            // completion_tokens=40 matches reasoning_checkpoint_interval=40
            // below (t+1 >= per_call_cap) so this reads as a genuine
            // cap-hit checkpoint, not a context-overflow hard error.
            then.status(200).json_body(chat_response_json(Some(CONTINUATION_MARKER), None, "length", 120, 40));
        });
        // Call 3: the checkpoint continuation — the request body now
        // carries the marker (from call 2's own response, folded into the
        // prefill). Concludes turn 2 via `stop`.
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.contains(CONTINUATION_MARKER)
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 140, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-delay-ckpt").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let sleeper = RecordingSleeper::default();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, Some(40), Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("checkpoint-continuation scripted dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 2, "sanity: two logical turns (the continuation is NOT a third)");
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [500],
            "exactly ONE rest — between turn 1 and turn 2 — never a second one before \
             the checkpoint continuation call"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_ms(), 500);
        assert_eq!(crate::trajectory::recorded(tmp.path()).rest_count(), 1);
    }

    // ---------------------------------------------------------------
    // (#2171) The GENERATION check-in — bounds every call that does NOT
    // carry the reasoning bound, not just reasoning ones. Fixes the
    // Devstral inactivity-timeout kill: pre-#2171 a non-thinking model's
    // whole answer/tool-call turn carried the raw answer bound
    // (10000 by default) with no check-in at all.
    // ---------------------------------------------------------------

    /// (#2171 test a) A non-thinking dispatch's first call — no reasoning
    /// bound available (`dispatch_has_reasoned` starts false) — must carry
    /// `max_tokens` at the SMALLER generation interval (4000), not the
    /// larger answer bound (10000). The mock is KEYED on the request body
    /// literally containing `"max_tokens":4000`; if the runtime sent
    /// anything else the request would not match and `run_with_sleeper`
    /// would return an `Err` (httpmock 404s an unmatched request), so
    /// `.expect(..)` succeeding IS the assertion.
    #[test]
    #[serial_test::serial]
    fn generation_bound_caps_the_request_below_the_answer_bound() {
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"max_tokens":4000}"#);
            then.status(200).json_body(chat_response_json(
                Some("a plain non-reasoning answer, well under the generation cap"),
                None,
                "stop",
                100,
                3000,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("gen-cap-request").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("say hi")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(3), None, Some(10_000), None, Some(4000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect(
            "the request must carry max_tokens=4000 (the generation interval) — an Err \
             here means the mock never matched, i.e. some OTHER value was sent (#2171)",
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// (#2171 test b) A non-thinking response that hits the GENERATION bound
    /// with a well-formed tool call salvages exactly like #479 (reasoning
    /// bound salvage already tests this shape) — but must NOT queue the
    /// "reduce your reasoning" nudge, because the model was never
    /// reasoning. Mirrors #2166's own turn-1 fix, applied to the new bound.
    #[test]
    #[serial_test::serial]
    fn generation_bound_salvage_sends_no_reasoning_nudge() {
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("mid-prose reasoning-free narration that ran right up to the cap"),
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
                // cap-1: the LIVE-observed LMStudio shape (stops before the
                // token that would exceed) — see the salvage arm's own
                // comment on `at_cap`'s tolerance match.
                3999,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("gen-cap-salvage").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(3), None, Some(10_000), None, Some(4000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("generation-bound salvage must drive the loop forward, not Err (#2171)");
        assert!(crate::trajectory::recorded(tmp.path()).turns() >= 1);

        for m in &outcome.messages {
            if let Some(c) = &m.content {
                assert!(
                    !c.contains("Reduce reasoning length"),
                    "a generation-bound salvage must never queue the reasoning-reduction \
                     nudge — the model was never reasoning (#2171). Message: {c}"
                );
            }
        }
        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let salvaged: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("dispatch.per_turn_cap.salvaged"))
            .collect();
        assert!(!salvaged.is_empty(), "salvage record must exist — got trajectory:\n{raw}");
        let bound = &salvaged[0]["bound"];
        // (merge-gate review item 0) `max_tokens_per_call` was 10000 on this
        // request; the record must name the SMALLER generation interval
        // (4000) as the bound that actually fired, not the raw answer bound
        // — proving `active_bound`'s three-way priority (reasoning >
        // generation > answer) picked the right one.
        assert_eq!(
            bound["kind"], serde_json::json!("generation_checkpoint_interval"),
            "the salvage record must name generation_checkpoint_interval as the bound, got {bound:?}"
        );
        assert_eq!(
            bound["value"], serde_json::json!(4000),
            "the bound's value must be the generation interval (4000), not max_tokens_per_call              (10000) — got {bound:?}"
        );
    }

    // ─── #2836 stage 1: the observer on the streaming path ───────────
    //
    // NOTE worth carrying: before these, EVERY loop-level test in this file
    // ran the non-streaming path, while streaming is the production default.
    // The check-in's whole redesign lives on the streamed path, so it had no
    // loop-level coverage at all until here.

    /// Build an SSE body: one `data:` chunk per piece, then a terminal chunk
    /// carrying `finish_reason` and `usage`, then `[DONE]`.
    fn sse(pieces: &[&str], finish: &str, completion_tokens: u32) -> String {
        let mut out = String::new();
        for p in pieces {
            let delta = serde_json::json!({"content": p});
            out.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"id":"c","choices":[{"index":0,"delta":delta}]})
            ));
        }
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id":"c",
                "choices":[{"index":0,"delta":{},"finish_reason":finish}],
                "usage":{"prompt_tokens":100,"completion_tokens":completion_tokens,
                         "total_tokens":100+completion_tokens}
            })
        ));
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// **The wire change, asserted directly.** A streamed call carries the
    /// real ceiling as `max_tokens`, never the check-in interval.
    ///
    /// This is the whole fix for #2836 stated as a contract: the interval was
    /// destructive *because* it was enforced server-side, where it truncated
    /// whatever was in flight. `tool_calls` and `content` are separate
    /// response channels but one generation stream, so a cut landed wherever
    /// the model happened to be — measured over four runs, 9 of 14 firings
    /// (64%) landed mid-`arguments` and destroyed an `edit`.
    ///
    /// The mock matches on `max_tokens: 9000` and would 404 on the old
    /// `1000`, so reverting the wire change reds this test.
    #[test]
    #[serial_test::serial]
    fn a_streamed_call_carries_the_ceiling_on_the_wire_not_the_check_in_interval() {
        let server = crate::test_support::GuardedMockServer::start();
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"max_tokens":9000}"#);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse(&["done"], "stop", 5));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("wire-ceiling").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, /* streaming */ true, &cfg,
            Some(3), None,
            /* max_tokens_per_call  */ Some(9_000),
            /* reasoning interval   */ Some(1_000),
            /* generation interval  */ Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect(
            "a streamed request must carry max_tokens=9000 (the ceiling); an Err here \
             means the check-in interval is still being enforced server-side (#2836)",
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// **A clean stream is never interrupted, however far past the interval
    /// it runs.** 8,000 characters of non-repeating text against a 1,000-token
    /// check-in: pre-Stage-1 the server would have truncated this twice, each
    /// cut costing a round trip and a re-sent prefill. Now it is observed and
    /// left alone.
    #[test]
    #[serial_test::serial]
    fn a_long_clean_stream_runs_to_completion_and_is_observed_for_free() {
        // Many small chunks, the shape a real stream actually has.
        let words: Vec<String> = (0..1600).map(|i| format!("w{i} ")).collect();
        let total: usize = words.iter().map(|w| w.len()).sum();
        assert!(total > 8_000, "fixture must cross several boundaries");
        let pieces: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
        let body = sse(&pieces, "stop", 2_000);
        let server = crate::test_support::GuardedMockServer::start();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("clean-stream").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(3), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a clean stream must complete");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "one turn — no continuations, because nothing was cut");

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        assert!(
            !traj_text.contains("dispatch.checkpoint"),
            "a healthy turn must produce no check-in cut at all; got:\n{traj_text}"
        );
        let end: serde_json::Value = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "model.streaming.end")
            .expect("a streaming.end record");
        assert!(
            end["observations"].as_u64().unwrap_or(0) >= 2,
            "the runtime must actually have LOOKED, repeatedly, and said nothing; got {end}"
        );
        // The calibration that makes the chars-per-token constant checkable
        // rather than assumed.
        assert!(
            end["chars_per_token"].as_f64().unwrap_or(0.0) > 0.0,
            "the measured ratio must be stamped; got {end}"
        );
    }

    /// Build an SSE body whose text arrives on the separate REASONING field,
    /// which is how the qwen3.x / Splash family delivers thinking.
    fn sse_reasoning(pieces: &[&str], finish: &str, completion_tokens: u32) -> String {
        let mut out = String::new();
        for p in pieces {
            out.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"id":"c","choices":[{"index":0,
                    "delta":{"reasoning_content": p}}]})
            ));
        }
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id":"c",
                "choices":[{"index":0,"delta":{},"finish_reason":finish}],
                "usage":{"prompt_tokens":100,"completion_tokens":completion_tokens,
                         "total_tokens":100+completion_tokens}
            })
        ));
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// (#2839) A model that will not stop repeating is HANDED OFF after the
    /// one remedy fails — it does not spin until a budget runs out.
    ///
    /// The sequence this pins, which is two calls and not seven:
    ///
    ///   call 1  degenerate, thought open   -> close_thought(), hand back
    ///   call 2  degenerate, thought closed -> escalate, everything banked
    ///
    /// The handler for the second line already existed (`degenerate &&
    /// !writing_thought`). It was UNREACHABLE for this model shape, because
    /// closing the thought flips the judged region to the answer, a
    /// separate-field reasoner never writes an answer, and the judge then
    /// read `""` and returned "not degenerate" forever. Measured live before
    /// the fix: six consecutive `judged_chars: 0, verdict: continue` records
    /// on one turn, ~270s of a 273s run, ending on an exhausted generation
    /// budget rather than on the repetition that actually caused it.
    ///
    /// So this test is the proof that fixing the vacuous pass reconnected an
    /// existing remedy, rather than the proof of new machinery.
    #[test]
    #[serial_test::serial]
    fn a_model_that_keeps_repeating_after_the_conclude_is_handed_off_not_spun() {
        let looped: String = "the same thing over and over ".repeat(400);
        let pieces: Vec<&str> = looped.split_inclusive(' ').collect();
        let body = sse_reasoning(&pieces, "stop", 2_000);
        let server = crate::test_support::GuardedMockServer::start();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("handoff").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(20), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a non-converging turn is handed off, never fatal");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "the handoff must name the REPETITION, not a budget that ran out \
             downstream of it",
        );

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        let checkpoints: Vec<serde_json::Value> = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.checkpoint")
            .collect();
        assert_eq!(
            checkpoints.len(),
            2,
            "exactly two: the conclude, then the handoff. More than that is the \
             spin this fixes; the live failure produced seven. Got:\n{}",
            checkpoints.iter().map(|c| c.to_string()).collect::<Vec<_>>().join("\n")
        );
        assert_eq!(checkpoints[0]["verdict"], "conclude");
        assert_eq!(checkpoints[1]["verdict"], "conclude");
        for c in &checkpoints {
            assert!(
                c["judged_chars"].as_u64().unwrap_or(0) > 0,
                "and neither verdict may be a pass over nothing: {c}"
            );
        }
    }

    /// (#2836, found by a live run rather than by the suite) The
    /// calibration figure must be absent on a tool-calling call.
    ///
    /// The gate's character cadence counts text and deliberately skips
    /// tool-call argument fragments; `completion_tokens` counts both. Divide
    /// one by the other on a tool-calling call and the result is not a
    /// chars-per-token ratio, it is an artifact of how much of the call was
    /// arguments. Live medians: 3.93 on text-only calls (the constant is 4,
    /// so it is right) against 1.58 on tool-calling ones. Stamping the
    /// second kind would invite exactly the wrong conclusion — that the
    /// cadence fires half as often as it should.
    #[test]
    #[serial_test::serial]
    fn the_cadence_calibration_is_omitted_on_a_tool_calling_call() {
        let server = crate::test_support::GuardedMockServer::start();
        let call = serde_json::json!([{
            "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\"}" },
        }]);
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":call}}]}),
            serde_json::json!({
                "id":"c",
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":40,"total_tokens":50}
            }),
        );
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse(&["done"], "stop", 5));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("calib-omit").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("read x")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(3), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("the dispatch must complete");

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        let ends: Vec<serde_json::Value> = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "model.streaming.end")
            .collect();
        let tool_call_turn = ends
            .iter()
            .find(|v| v["tool_calls_count"].as_u64().unwrap_or(0) > 0)
            .expect("a tool-calling streaming.end record");
        assert!(
            tool_call_turn["chars_per_token"].is_null(),
            "a tool-calling call must carry NO calibration figure; got {tool_call_turn}"
        );
        let text_turn = ends
            .iter()
            .find(|v| v["tool_calls_count"].as_u64().unwrap_or(0) == 0)
            .expect("a text-only streaming.end record");
        assert!(
            text_turn["chars_per_token"].as_f64().unwrap_or(0.0) > 0.0,
            "a text-only call must still carry one; got {text_turn}"
        );
    }

    /// **And it still intervenes when the thing it exists to catch happens.**
    /// The same fixture shape, repeating. The runtime ends the call itself —
    /// the endpoint was never told to stop — and the turn is handed back
    /// rather than the dispatch dying.
    #[test]
    #[serial_test::serial]
    fn a_degenerate_stream_is_ended_by_the_runtime_not_the_endpoint() {
        let looped: String = "the same thing over and over ".repeat(400);
        assert!(looped.len() > 8_000);
        let server = crate::test_support::GuardedMockServer::start();
        let pieces: Vec<&str> = looped.split_inclusive(' ').collect();
        let body = sse(&pieces, "stop", 2_000);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            // finish_reason "stop" deliberately: the ENDPOINT is perfectly
            // happy. If a cut happens it was the runtime's decision, which is
            // the whole point of moving the check-in client-side.
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("degen-stream").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(3), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a degenerate turn must never be fatal to the dispatch");
        // #1221's EXISTING remedy for a repeating ANSWER region, unchanged
        // by Stage 1: there is no thought left to close, so the turn cannot
        // be handed back to continue, and the dispatch escalates with
        // everything banked so far attached. Asserted rather than smoothed
        // over — what Stage 1 changes is WHO ended the call, not what
        // happens afterward.
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
        );

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        assert!(
            traj_text.contains("dispatch.checkpoint"),
            "the gate must have intervened on repeating output; got:\n{traj_text}"
        );
        assert!(
            !traj_text.contains("dispatch.tool_call.discarded"),
            "and it must not have destroyed anything doing it"
        );
        // (#2887 F3) The gate stamps its own policy + outcome now, rather
        // than leaving a downstream host to reconstruct them from its own
        // (possibly stale, possibly absent) environment. Under the default
        // (conclude) policy this test runs with, the degenerate observation
        // and the abort it produced both say `acted:true` — this IS the
        // call that ended the stream. Parsed per-record (not a raw
        // substring match) so the assertion pins the FIELD ON THE RIGHT
        // RECORD, not merely somewhere in the file — `"acted":true` is
        // also unconditionally present on every `dispatch.gate.abort`
        // record, so a substring check alone cannot tell a correctly-
        // stamped observation from a wrongly-stamped one sharing a file
        // with a correctly-stamped abort.
        let observation = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "dispatch.gate.observation" && v["degenerate"] == true)
            .expect("a degenerate observation record must exist");
        assert_eq!(observation["policy"], "conclude", "got {observation}");
        assert_eq!(
            observation["acted"], true,
            "the degenerate observation that led to the abort must say it \
             acted — under conclude it is the SAME moment as the abort \
             below; got {observation}"
        );
        let abort = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "dispatch.gate.abort")
            .expect("the stream gate itself must have aborted the call");
        assert_eq!(abort["policy"], "conclude", "got {abort}");
        assert_eq!(abort["acted"], true, "got {abort}");
    }

    /// (B1) Promise: every model call's tokens reach the run's totals and
    /// its caps, including a turn the RUNTIME ended. The cut call never gets
    /// the endpoint's `usage`, so the runtime counts what streamed past. The
    /// trajectory keeps that apart from a reported figure (`usage` stays
    /// null, `completion_estimate` carries the number), and the cumulative
    /// cap sees it: without it a run cut every turn escapes the cap.
    #[test]
    #[serial_test::serial]
    fn a_runtime_cut_turn_counts_toward_the_cumulative_cap_and_is_marked_an_estimate() {
        let looped: String = "the same thing over and over ".repeat(400);
        let server = crate::test_support::GuardedMockServer::start();
        let mut pieces: Vec<&str> = vec!["<think>"];
        pieces.extend(looped.split_inclusive(' '));
        let body = sse(&pieces, "stop", 2_000);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "text/event-stream").body(body.clone());
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("cut-accounting").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(5), Some(100), Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a cut turn must never be fatal to the dispatch");
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CumulativeTokensExceeded),
            "the cut turn's tokens must reach the cumulative cap"
        );
        let text = std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        let completed = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "model.completed")
            .expect("a model.completed record for the cut turn");
        assert!(completed["usage"].is_null(), "the cut call has no reported usage; got {completed}");
        assert!(
            completed["completion_estimate"].as_u64().unwrap_or(0) > 100,
            "the runtime's estimate is recorded apart from usage; got {completed}"
        );
    }

    /// (B1) The per-call-budget line never says a bare `<unknown>`: it names
    /// the count, the runtime's estimate as an estimate, or that the server
    /// reported none.
    #[test]
    fn the_per_call_budget_line_names_what_it_knows_about_a_turns_tokens() {
        assert_eq!(completion_tokens_label(Some(7), None), "7 completion tokens");
        assert_eq!(completion_tokens_label(Some(7), Some(9)), "7 completion tokens");
        assert!(completion_tokens_label(None, Some(9)).contains("~9"));
        assert!(completion_tokens_label(None, Some(9)).contains("estimate"));
        assert!(completion_tokens_label(None, None).contains("not reported by the server"));
        assert!(!completion_tokens_label(None, None).contains("<unknown>"));
    }

    /// (#2846) `record` (was `observe`) measures and records without acting.
    ///
    /// The same degenerate stream the test above feeds to the default
    /// `conclude` policy, where it escalates the dispatch. Under `record`
    /// the check-in must still fire on the same cadence, the tail ratio
    /// must still be computed, the record must say the gate WOULD have
    /// concluded, and the dispatch must NOT be cut.
    ///
    /// This is the single-variable form the bake-off arm D lacked. Widening
    /// the per-call cap to disable the gate also shrinks the usable prompt
    /// budget (cap + prompt must fit the context window), so a failure could
    /// not be attributed to the missing gate. Policy changes only whether the
    /// verdict is obeyed: cadence, per-call cap and prompt budget are
    /// untouched.
    #[test]
    #[serial_test::serial]
    fn record_policy_records_the_would_be_conclusion_without_acting() {
        let looped: String = "the same thing over and over ".repeat(400);
        let server = crate::test_support::GuardedMockServer::start();
        let pieces: Vec<&str> = looped.split_inclusive(' ').collect();
        let body = sse(&pieces, "stop", 2_000);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        std::env::set_var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY", "record");
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("degen-record").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(3), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        );
        std::env::remove_var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY");
        let outcome = outcome.expect("record must not make a degenerate turn fatal");

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();

        // The STREAM gate is what this fixture reaches: the endpoint returns
        // `finish_reason: "stop"`, so under `record` the call is never cut
        // short and therefore never produces a length-finish for the
        // checkpoint gate to judge. That absence is the POINT — under
        // `conclude` the same fixture is aborted mid-stream (asserted by
        // `a_degenerate_stream_is_ended_by_the_runtime_not_the_endpoint`).
        assert!(
            traj_text.contains("dispatch.gate.observation"),
            "observe must still MEASURE; a policy that records nothing is \
             indistinguishable from `off`; got:\n{traj_text}"
        );
        assert!(
            traj_text.contains("\"degenerate\":true"),
            "observe must record that the output WAS repeating, or the \
             counterfactual it exists to provide is not in the artifact; \
             got:\n{traj_text}"
        );
        assert_ne!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "record must not escalate the dispatch the way conclude does"
        );
        // There are TWO gates. The stream gate (`runtime/src/stream_gate.rs`)
        // judges mid-stream and ABORTS the call client-side; the checkpoint
        // gate judges at the per-call cap. A policy that suppresses only the
        // second one still lets the first cut generation short, which is a
        // second variable and destroys this feature's entire reason to exist.
        assert!(
            !traj_text.contains("dispatch.gate.abort"),
            "observe must not let the STREAM gate abort either; the claim is \
             that only the verdict's EFFECT changes, and an aborted stream is \
             an effect; got:\n{traj_text}"
        );
        // (#2887 F3) Same policy/acted stamping this issue adds to the
        // conclude path above — under record the degenerate observation
        // must say `policy:"record"` and `acted:false`: the judge found it
        // repeating, but nothing ended the call because of it. Parsed
        // per-record, same discipline as the conclude test's own version of
        // this assertion.
        let observation = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "dispatch.gate.observation" && v["degenerate"] == true)
            .expect("a degenerate observation record must exist");
        assert_eq!(observation["policy"], "record", "got {observation}");
        assert_eq!(
            observation["acted"], false,
            "a degenerate observation under observe must say it did NOT \
             act — that is the entire point of the policy; got {observation}"
        );
    }

    /// (#2947) `warn` behaves exactly like `record` inside the container
    /// (it measures and never cuts), and stamps `policy:"warn"` on the
    /// finding, which is what the HOST keys its warning surfaces on.
    #[test]
    #[serial_test::serial]
    fn warn_policy_never_cuts_and_stamps_warn_for_the_host() {
        let looped: String = "the same thing over and over ".repeat(400);
        let server = crate::test_support::GuardedMockServer::start();
        let pieces: Vec<&str> = looped.split_inclusive(' ').collect();
        let body = sse(&pieces, "stop", 2_000);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body.clone());
        });
        std::env::set_var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY", "warn");
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("degen-warn").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "m",
            vec![Message::system("s"), Message::user("go")],
            &[Tool::Read], &mut traj, true, &cfg,
            Some(3), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        );
        std::env::remove_var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY");
        let outcome = outcome.expect("record must not make a degenerate turn fatal");

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();

        // The STREAM gate is what this fixture reaches: the endpoint returns
        // `finish_reason: "stop"`, so under `record` the call is never cut
        // short and therefore never produces a length-finish for the
        // checkpoint gate to judge. That absence is the POINT — under
        // `conclude` the same fixture is aborted mid-stream (asserted by
        // `a_degenerate_stream_is_ended_by_the_runtime_not_the_endpoint`).
        assert!(
            traj_text.contains("dispatch.gate.observation"),
            "observe must still MEASURE; a policy that records nothing is \
             indistinguishable from `off`; got:\n{traj_text}"
        );
        assert!(
            traj_text.contains("\"degenerate\":true"),
            "observe must record that the output WAS repeating, or the \
             counterfactual it exists to provide is not in the artifact; \
             got:\n{traj_text}"
        );
        assert_ne!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "record must not escalate the dispatch the way conclude does"
        );
        // There are TWO gates. The stream gate (`runtime/src/stream_gate.rs`)
        // judges mid-stream and ABORTS the call client-side; the checkpoint
        // gate judges at the per-call cap. A policy that suppresses only the
        // second one still lets the first cut generation short, which is a
        // second variable and destroys this feature's entire reason to exist.
        assert!(
            !traj_text.contains("dispatch.gate.abort"),
            "observe must not let the STREAM gate abort either; the claim is \
             that only the verdict's EFFECT changes, and an aborted stream is \
             an effect; got:\n{traj_text}"
        );
        // (#2887 F3) Same policy/acted stamping this issue adds to the
        // conclude path above — under warn the degenerate observation
        // must say `policy:"record"` and `acted:false`: the judge found it
        // repeating, but nothing ended the call because of it. Parsed
        // per-record, same discipline as the conclude test's own version of
        // this assertion.
        let observation = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "dispatch.gate.observation" && v["degenerate"] == true)
            .expect("a degenerate observation record must exist");
        assert_eq!(observation["policy"], "warn", "got {observation}");
        assert_eq!(
            observation["acted"], false,
            "a degenerate observation under observe must say it did NOT \
             act — that is the entire point of the policy; got {observation}"
        );
    }

    /// (#2836) The cut that lands inside a tool call's arguments destroys
    /// it, and until now destroyed it SILENTLY.
    ///
    /// The shape, taken from the proof run (`long-agentic-splash-qwen36-
    /// 1789870277-1`, turn 15): the model reasons past the check-in twice,
    /// starts emitting a tool call on the third slice, and the check-in
    /// lands mid-`arguments`. The JSON does not parse, so #479's salvage
    /// correctly declines to dispatch it — sending malformed arguments back
    /// is what 400s the next request. The turn then proceeds as though
    /// nothing happened: `messages.pop()` drops the whole assistant message
    /// and the prefill replaces it.
    ///
    /// What was missing is any RECORD of that. The trajectory carried a
    /// `dispatch.checkpoint` saying "handing back the answer so far" and
    /// nothing at all about the tool call that went with it — so a run that
    /// lost eighteen tool calls read, from its own artifacts, like a run
    /// that simply did not use tools. That is a no-blind-runs violation
    /// independent of the fix: whatever Stage 1 does about the cut, the
    /// discard has to be visible.
    ///
    /// Deliberately NOT asserting that the call survives. It does not, at
    /// Stage 0, and a test that pretended otherwise would have to be
    /// rewritten rather than extended when Stage 1 lands.
    #[test]
    #[serial_test::serial]
    fn a_tool_call_the_checkpoint_cut_in_half_is_recorded_as_discarded() {
        let server = crate::test_support::GuardedMockServer::start();
        // Arguments truncated mid-string — exactly what a cut inside the
        // JSON leaves behind, and unparseable by construction.
        let half_written = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "edit", "arguments": "{\"path\":\"/workspace/te" },
        }]);
        // Call 1: an OPEN think block plus the half-written call, stopped
        // at the check-in's cap-1 (LMStudio reports cap-1 live).
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                !b.contains("<think>rewrite the test file")
            });
            then.status(200).json_body(chat_response_json(
                Some("<think>rewrite the test file"),
                Some(half_written.clone()),
                "length",
                100,
                // cap-1 of the GENERATION check-in (4000), which is what
                // bounds a fresh turn's first call before this dispatch has
                // shown a closed reasoning region. The proof run was cut by
                // the reasoning check-in at 1000 instead; the discard is the
                // same either way, because it is the cut landing mid-JSON
                // that destroys the call, not which interval placed it.
                3999,
            ));
        });
        // Call 2 (the continuation, carrying the prefill): conclude.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.contains("<think>rewrite the test file")
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 120, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("discard-record").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("edit the test file")];
        let tools = [Tool::Read, Tool::Edit];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(3), None, None, None, None,
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("the dispatch itself must survive the discard");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);

        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        let discarded: Vec<serde_json::Value> = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.tool_call.discarded")
            .collect();
        assert_eq!(
            discarded.len(),
            1,
            "the destroyed call must leave exactly one record; trajectory was:\n{traj_text}"
        );
        let rec = &discarded[0];
        assert_eq!(rec["name"], "edit", "the record must name the tool that was lost");
        assert_eq!(
            rec["arguments_chars"], 22,
            "the record must say how much of the call had been written when the cut landed"
        );
        assert_eq!(
            rec["cut"], "server_length",
            "and who cut it — the seam Stage 1 changes to a runtime abort"
        );
        // (#2963) The call never runs, and the turn's own record says so, so
        // the viewer never names it as the call running now.
        let completed: Vec<serde_json::Value> = traj_text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "model.completed" && v["tool_calls"].is_array())
            .collect();
        assert_eq!(completed[0]["tool_calls"][0]["name"], "edit");
        assert_eq!(completed[0]["tool_calls"][0]["runs"], false, "a discarded call is marked not to run: {}", completed[0]);
        assert!(!traj_text.contains("\"type\":\"tool.completed\""), "and nothing ran");
    }

    /// (#2963) Each `model.completed` tool call says whether it RUNS, decided
    /// by the same plan the dispatch then follows (`plan_tool_calls`): an
    /// ungranted call (first) and a call to no tool at all (last) are marked
    /// `runs: false`, the granted one is not marked, and the only
    /// `tool.completed` that follows is the granted call's. The host builds
    /// `tool_names` / `tool_paths` from the running calls alone, so the
    /// viewer never names a call the runtime refused.
    #[test]
    #[serial_test::serial]
    fn model_completed_marks_the_calls_that_will_not_run() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = serde_json::json!([
            { "id": "c1", "type": "function", "function": { "name": "write", "arguments": "{\"path\":\"/workspace/src/x.rs\",\"content\":\"x\"}" } },
            { "id": "c2", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/workspace/src/y.rs\",\"offset\":1,\"limit\":1}" } },
            { "id": "c3", "type": "function", "function": { "name": "frobnicate", "arguments": "{}" } },
        ]);
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(None, Some(calls.clone()), "tool_calls", 100, 20));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("runs-marks").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read-only task")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(5), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("refused calls must not error the dispatch");
        assert!(matches!(outcome.terminal_reason, TerminalReason::Stop));

        let raw = std::fs::read_to_string(tmp.path().join(".darkmux-runtime/trajectory.jsonl")).unwrap();
        let events: Vec<serde_json::Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let completed = events.iter().find(|v| v["type"] == "model.completed" && v["tool_calls"].is_array()).expect("turn 1's record");
        let marks: Vec<(String, Option<bool>)> = completed["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["name"].as_str().unwrap().to_string(), c.get("runs").map(|r| r.as_bool().unwrap())))
            .collect();
        assert_eq!(
            marks,
            vec![("write".into(), Some(false)), ("read".into(), None), ("frobnicate".into(), Some(false))],
            "only the granted call is left unmarked: {completed}"
        );
        let ran: Vec<&str> = events.iter().filter(|v| v["type"] == "tool.completed").map(|v| v["tool_name"].as_str().unwrap()).collect();
        assert_eq!(ran, vec!["read"], "the calls that run are exactly the unmarked ones");
    }

    /// (#2963) The #479 salvage keeps the well-formed calls of a turn the
    /// cap cut, and drops the one cut mid-arguments (#2836). The record
    /// marks the dropped call `runs: false` and the kept one not at all, and
    /// only the kept call completes: the record agrees with the loop.
    ///
    /// No viewer reads these marks today: a salvaged turn's record ends
    /// `length`, and the host writes no `dispatch.turn` for a `length`
    /// record, so the turn gets no `tool_names` / `tool_paths` at all (a
    /// separate gap, filed on its own).
    #[test]
    #[serial_test::serial]
    fn model_completed_marks_a_call_the_cut_left_malformed() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = serde_json::json!([
            { "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/workspace/src/y.rs\",\"offset\":1,\"limit\":1}" } },
            { "id": "c2", "type": "function", "function": { "name": "edit", "arguments": "{\"path\":\"/workspace/te" } },
        ]);
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            // cap-1 of the generation check-in: OUR cut, so #479 salvages.
            then.status(200).json_body(chat_response_json(None, Some(calls.clone()), "length", 100, 3999));
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("runs-salvage").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("edit it")];
        let tools = [Tool::Read, Tool::Edit];
        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(3), None, None, None, None,
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("the salvage must drive the loop");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);

        let raw = std::fs::read_to_string(tmp.path().join(".darkmux-runtime/trajectory.jsonl")).unwrap();
        let events: Vec<serde_json::Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert!(events.iter().any(|v| v["type"] == "dispatch.per_turn_cap.salvaged"), "the salvage fired: {raw}");
        let completed = events.iter().find(|v| v["type"] == "model.completed" && v["tool_calls"].is_array()).expect("the cut call's record");
        assert!(completed["tool_calls"][0].get("runs").is_none(), "the well-formed call runs: {completed}");
        assert_eq!(completed["tool_calls"][1]["runs"], false, "the call the cut left malformed does not: {completed}");
        let ran: Vec<&str> = events.iter().filter(|v| v["type"] == "tool.completed").map(|v| v["tool_name"].as_str().unwrap()).collect();
        assert_eq!(ran, vec!["read"]);
    }

    /// (#2171 test c) #2166's own invariant must survive this change: once a
    /// dispatch has PROVEN it reasons (turn 1 carries a closed `<think>`
    /// block), turn 2's first call still carries the 1000-token REASONING
    /// interval — not the 4000-token generation interval — even though the
    /// generation knob is left at its production default (unset) the whole
    /// time. Priority is: reasoning bound, then generation bound, then the
    /// raw answer bound.
    #[test]
    #[serial_test::serial]
    fn reasoning_bound_still_wins_over_the_generation_default() {
        let server = crate::test_support::GuardedMockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        // Turn 1: closed think block + tool call — proves this dispatch reasons.
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                Some("<think>brief</think>"),
                Some(tool_calls.clone()),
                "tool_calls",
                100,
                20,
            ));
        });
        // Turn 2's first call: must carry the REASONING interval (1000), not
        // the generation default (4000) — keyed via json_body_partial.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"max_tokens":1000}"#)
                .matches(|req| {
                    let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                    b.matches("\"role\":\"tool\"").count() == 1
                });
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 120, 5));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reasoning-wins").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(3), None, None, None, None,
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect(
            "turn 2's first call must carry max_tokens=1000 (the reasoning interval) — an \
             Err here means it carried the generation default instead (#2171 regression)",
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 2);
    }

    /// (#3074) A turn that keeps hitting a checkpoint until what it has
    /// generated reaches the context window escalates with a named reason
    /// rather than continuing forever. `max_turns` does not count
    /// continuations and the inactivity deadline resets on every chunk, so
    /// this is the bound a streaming model that never stops runs into. The
    /// slices are distinct, so the degeneracy gate stays out of the way and
    /// only the window-derived bound can end the turn.
    #[test]
    #[serial_test::serial]
    fn a_turn_whose_continuations_fill_the_context_window_escalates() {
        let block: String = (0..8100).map(|i| format!("w{i} ")).collect();
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(Some(&block), None, "length", 100, 999));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-continuations").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig { context_window: Some(2500), ..compaction::CompactionConfig::never_compact() };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(100_000), None, Some(1000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("window exhaustion is a clean EscalationTriggered outcome, not an Err (#3074)");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::TurnContinuationsExhausted),
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "every hit continues the SAME turn");
    }

    /// (#3074) Inverted: a turn whose continuations are still UNDER the
    /// context window keeps going. One checkpoint at 999 tokens against a
    /// 2500-token window, then the model stops: the turn ends Stop, not with
    /// the window escalation the twin above pins.
    #[test]
    #[serial_test::serial]
    fn a_turn_whose_continuations_stay_under_the_context_window_continues() {
        const MARK: &str = "PARTIAL-UNDER-WINDOW";
        let block: String = std::iter::once(format!("{MARK} ")).chain((0..8100).map(|i| format!("w{i} "))).collect();
        fn carries_mark(req: &httpmock::prelude::HttpMockRequest) -> bool {
            req.body.as_ref().is_some_and(|v| String::from_utf8_lossy(v).contains(MARK))
        }
        let server = crate::test_support::GuardedMockServer::start();
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| !carries_mark(req));
            then.status(200).json_body(chat_response_json(Some(&block), None, "length", 100, 999));
        });
        server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(carries_mark);
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 100, 5));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-continuations-under").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think a while")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig { context_window: Some(2500), ..compaction::CompactionConfig::never_compact() };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(100_000), None, Some(1000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a turn under the window completes");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::Stop,
            "999 of a 2500-token window is not full: the continuation must be sent"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "the continuation is the SAME turn");
    }

    /// (#3074) The window bound's own edge: a turn has filled the window once
    /// it has generated AS MANY tokens as the window holds, not more; and with
    /// no window configured there is no bound to reach.
    #[test]
    fn turn_fills_window_at_exactly_the_window_and_never_without_one() {
        assert!(!turn_fills_window(Some(2500), 2499));
        assert!(turn_fills_window(Some(2500), 2500));
        assert!(turn_fills_window(Some(2500), 2501));
        assert!(!turn_fills_window(None, u32::MAX));
    }

    /// (#3074) The same window bound for a turn that keeps REASONING: the
    /// slices arrive in the separate `reasoning_content` field with empty
    /// `content`, which is the shape a reasoning model that never closes its
    /// thought produces. The answer-content twin above does not reach this
    /// path (its budget is bounded by #2171 already), so this is the test
    /// that shows the window bound fires for a reasoning continuation.
    #[test]
    #[serial_test::serial]
    fn a_reasoning_turn_whose_continuations_fill_the_context_window_escalates() {
        let block: String = (0..8100).map(|i| format!("w{i} ")).collect();
        let server = crate::test_support::GuardedMockServer::start();
        let mut body = chat_response_json(Some(""), None, "length", 100, 999);
        body["choices"][0]["message"]["reasoning_content"] = serde_json::json!(block);
        // Only the first calls are answered: past that the server refuses, so
        // a missing bound fails the test with an error instead of continuing
        // without end.
        static CUT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        CUT_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
        server.mock(move |when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|_| CUT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 12);
            then.status(200).json_body(body.clone());
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("turn-continuations-reasoning").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig { context_window: Some(2500), ..compaction::CompactionConfig::never_compact() };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(100_000), None, Some(1000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("window exhaustion is a clean EscalationTriggered outcome, not an Err (#3074)");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::TurnContinuationsExhausted),
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "every hit continues the SAME turn");
    }

    /// (#2171 test d, floor added on merge-gate review) A turn that keeps
    /// hitting the GENERATION check-in past its continuation budget must
    /// escalate with a NAMED reason rather than continuing forever —
    /// deliberately NOT the same open-ended shape the reasoning check-in
    /// gets. `max_tokens_per_call=2000`, `generation_checkpoint_interval=
    /// 1000` → the naive ratio is 2000/1000=2, but the FLOOR
    /// (`max(4, ratio)`) governs: the budget is 4 continuations, so the
    /// 5th generation-bound cut is what exhausts it, and a `dispatch.
    /// checkpoint` record must exist for each of the 5 — proving the floor
    /// overrides the ratio rather than merely happening not to matter here.
    ///
    /// (#2633) Two things changed here, both consequences of the budget now
    /// being acted on AFTER the degeneracy gate rather than ahead of it:
    ///
    /// 1. The record count is 5, not 4. The call that exhausts the budget
    ///    now gets judged and recorded like every other checkpoint, so its
    ///    tail ratio is visible to the operator instead of being dropped.
    ///    The budget ALLOWANCE is unchanged — the 5th generation-bound cut
    ///    is still what stops the turn.
    /// 2. The mock's content block is 8100 distinct tokens rather than the
    ///    original 9-token phrase, because the gate now judges checkpoint 5
    ///    in this fixture and the fixture has to mean what it says: this is
    ///    the BUDGET path, so the slice must be robustly NOT degenerate.
    ///    The original phrase was 0.2647 at checkpoint 5 against a 0.25
    ///    threshold — clean by 0.015, i.e. a reworded mock string could have
    ///    silently turned this into a degeneracy test. Any exactly-periodic
    ///    block scores ~`1/k` at checkpoint `k` and so goes degenerate at
    ///    k=5 by construction; the way OUT of that is the property #2258's
    ///    fixtures already rely on — make one period WIDER than the judged
    ///    tail (`TAIL_SAMPLE_INTERVALS * 1000 = 8000` here), so the sampled
    ///    tail is a sub-period run of unique tokens and every window is
    ///    distinct (ratio 1.0) no matter how many times the block repeats.
    ///    `completion_tokens` stays 999 (cap-1, the cap-cliff tolerance the
    ///    original fixture already used) — the gate judges CONTENT while the
    ///    budget counts CALLS, so the two are independent here by design.
    #[test]
    #[serial_test::serial]
    fn generation_checkpoint_budget_exhausts_at_the_floor_not_the_naive_ratio() {
        let block: String = (0..8100).map(|i| format!("w{i} ")).collect();
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(&block),
                None,
                "length",
                100,
                999,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("gen-budget-exhaust").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("write forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(2000), None, Some(1000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("budget exhaustion is a clean EscalationTriggered outcome, not an Err (#2171)");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::GenerationCheckpointBudgetExhausted),
            "exceeding the generation-continuation budget must name the reason, not loop \
             forever or fall through to MaxTurns"
        );
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 1,
            "every hit is a continuation of the SAME logical turn — turns must not move"
        );
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoint_count = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .count();
        assert_eq!(
            checkpoint_count, 5,
            "the floor (max(4, 2000/1000)=4), not the naive ratio (2), must govern — \
             4 successful continuations, then the 5th draws the last of the budget and \
             stops the turn. All 5 are judged and recorded (#2633): the exhausting call \
             is a checkpoint like any other, and dropping its record hid the very ratio \
             an operator needs to tell a runaway from a loop"
        );
        // (#2633) The exhausting call must be stopped by the BUDGET, not by
        // the gate — this fixture exists to pin the budget path, and its
        // fifth checkpoint reading `conclude` would mean the fixture had
        // quietly become a degeneracy test (see the doc comment above).
        let fifth = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .find(|e| e["checkpoint"] == serde_json::json!(5))
            .expect("a checkpoint 5 record must exist");
        assert_eq!(
            fifth["verdict"],
            serde_json::json!("continue"),
            "checkpoint 5's slice must read CLEAN (one 8100-token period is wider than \
             the 8000-token judged tail, so every window is distinct) — a `conclude` \
             here means the fixture is no longer testing the budget, got {fifth:?}"
        );
    }

    // ---------------------------------------------------------------
    // (#1221) The prefill state machine, tested directly.
    //
    // These exist because the loop-level tests could NOT falsify three of
    // `TurnAccum`'s guards: two deliberate mutations (delete `begin`'s
    // abandon; check the inline-think delimiters before `think_closed`) left
    // all 433 loop tests green. That is not evidence the guards are
    // unnecessary — the whole point of the redesign is that the leaking
    // states are no longer REACHABLE through the loop, so the loop cannot
    // reach them to prove anything. A state machine whose invariants are
    // only observable three layers up is a state machine nobody can check.
    // ---------------------------------------------------------------

    /// A prefill and its state are created, folded, and abandoned as a UNIT.
    /// Clearing the index while leaving the message is the leak that produced
    /// both of this feature's shipped defects: nothing downstream can
    /// reconstruct an answer from an orphaned prefill, so `main.rs` hands raw
    /// `<think>` markup over as the deliverable.
    /// (#2836) The degeneracy judge must never be handed an empty string
    /// while the model is producing output.
    ///
    /// `carried()`'s own doc records the MIRROR of this bug already being
    /// fixed once: "judging the thought unconditionally left a non-reasoning
    /// turn measuring an empty string, so degeneracy could never fire and a
    /// repeating answer spun forever." This is the same bug on the other
    /// region.
    ///
    /// The shape, measured live: a model that reasons exclusively through
    /// `reasoning_content` gets one honest degenerate verdict, which calls
    /// `close_thought()`. That flips the judged region from thought to
    /// ANSWER — and `absorb` keeps routing reasoning to the thought, so the
    /// answer stays empty. From that moment the judge reads "" and returns
    /// `continue` forever while the model reasons on, unwatched. Six
    /// consecutive `judged_chars: 0, verdict: continue` records, then the
    /// dispatch died on an exhausted budget.
    ///
    /// A pass over zero characters is not evidence of health.
    #[test]
    fn the_judge_is_never_handed_an_empty_region_while_the_other_one_has_text() {
        let mut turn = TurnAccum::default();
        turn.absorb("reasoning that arrives on the separate field", "");
        assert!(!turn.carried().is_empty(), "precondition: the thought is judged");

        // The degenerate verdict's remedy.
        turn.close_thought();

        // The model keeps reasoning; content stays empty, as it did live.
        turn.absorb(" and keeps going on the separate field", "");
        assert!(
            !turn.carried().is_empty(),
            "after concluding, a reasoning-only model must still be judged on \
             SOMETHING — an empty slice makes every later verdict vacuous"
        );
    }

    /// The other direction must keep working: once there IS an answer, that
    /// is what gets judged. Otherwise this fix would re-introduce the very
    /// bug `carried()`'s doc says it was written to fix.
    #[test]
    fn a_closed_thought_with_a_real_answer_is_judged_on_the_answer() {
        let mut turn = TurnAccum::default();
        turn.absorb("some reasoning", "");
        turn.close_thought();
        turn.absorb("", "the actual answer text");
        assert_eq!(
            turn.carried(),
            "the actual answer text",
            "the answer region wins whenever it has content"
        );
    }

    #[test]
    fn a_new_turn_takes_the_previous_turns_prefill_with_it() {
        let mut messages = vec![Message::system("s"), Message::user("u")];
        let mut turn = TurnAccum::default();
        turn.absorb("thinking hard", "");
        turn.hand_back(&mut messages);
        assert_eq!(messages.len(), 3, "the prefill was pushed");
        assert!(turn.has_prefill());

        turn.begin(&mut messages);
        assert!(!turn.has_prefill(), "the index was cleared");
        assert_eq!(
            messages.len(),
            2,
            "...and so was the MESSAGE — an orphan here becomes the deliverable"
        );
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .unwrap_or("")
                .contains("thinking hard")),
            "the previous turn's scratch work must not survive into the next turn"
        );
    }

    /// A checkpoint REPLACES its predecessor rather than appending beside it.
    /// A live 30-checkpoint dispatch showed the cost of appending: thirty
    /// sibling assistant messages, each opening its own `<think>` around a
    /// truncated copy of the same answer, so the model restarted every call.
    #[test]
    fn each_checkpoint_replaces_the_previous_prefill() {
        let mut messages = vec![Message::system("s"), Message::user("u")];
        let mut turn = TurnAccum::default();
        for slice in ["first ", "second ", "third "] {
            turn.absorb(slice, "");
            turn.hand_back(&mut messages);
        }
        let assistants: Vec<_> = messages.iter().filter(|m| m.role == "assistant").collect();
        assert_eq!(assistants.len(), 1, "one growing message, not a chain");
        let body = assistants[0].content.as_deref().unwrap_or("");
        assert!(
            body.contains("first ") && body.contains("second ") && body.contains("third "),
            "the prefill carries the WHOLE thought, not the newest slice — got {body:?}"
        );
        assert_eq!(
            body.matches(crate::budget_request::THINK_OPEN.trim()).count(),
            1,
            "exactly one opener around the accumulation — got {body:?}"
        );
    }

    /// Once the thought is closed, EVERYTHING that follows is the answer —
    /// including text that itself contains `<think>` markup. Testing the
    /// inline delimiters first sent post-close slices back into the thought,
    /// so a concluded turn never accumulated an answer at all.
    #[test]
    fn a_closed_thought_routes_every_later_slice_to_the_answer() {
        let mut turn = TurnAccum::default();
        turn.absorb("", "<think>\nreasoning");
        assert!(turn.writing_thought(), "an unclosed inline think is the thought");
        turn.close_thought();

        turn.absorb("", "<think>\nthe model re-opened one");
        assert!(
            turn.answer.contains("the model re-opened one"),
            "post-close text is ANSWER text however it is marked up — answer={:?}",
            turn.answer
        );
        assert!(
            !turn.thought.contains("the model re-opened one"),
            "a closed thought must not reopen — thought={:?}",
            turn.thought
        );
        assert!(turn.in_answer_region(), "and the answer bound applies from here");
    }

    /// (#1221) A turn whose thought was NEVER CLOSED must still deliver its
    /// work. This is the common case for a thinking model, not an edge case,
    /// and it was found by a live dispatch rather than by any test here.
    ///
    /// Measured, 66 API calls on qwen3.6-35b: the provider tagged reasoning on
    /// call 1 only (`reasoning_format: separate-field`), so darkmux prefilled
    /// an OPEN `<think>`. Under `response_format` the model cannot close it,
    /// and once darkmux supplies the opener the provider stops tagging
    /// continuations — so all 64 later calls arrived as ordinary content and
    /// were classified as more thought. The answer region was empty for the
    /// entire turn. 26,181 completion tokens were generated and 1,116
    /// characters reached the operator: the last slice, starting mid-sentence.
    ///
    /// That is the discard-the-turn bug this feature exists to end, one layer
    /// further in. `fold` and `pending_answer` also disagreed about it, so the
    /// SAME run produced a different deliverable depending on whether it ended
    /// on `stop` or on a cap.
    #[test]
    fn a_turn_that_never_closed_its_thought_still_delivers_its_work() {
        let mut messages = vec![Message::user("u")];
        let mut turn = TurnAccum::default();
        // Call 1: the provider tags reasoning. Calls 2..n: plain content that
        // is really the answer, but is indistinguishable from a continued
        // thought because the block darkmux opened can never be closed.
        turn.absorb("EARLY-REASONING ", "");
        turn.absorb("", "MIDDLE-WORK ");
        turn.absorb("", "MORE-WORK ");
        turn.hand_back(&mut messages);

        let mut final_msg = Message::assistant("FINAL-SLICE");
        turn.fold(&mut messages, &mut final_msg);
        let delivered = final_msg.content.as_deref().unwrap_or("");
        assert!(
            delivered.contains("MIDDLE-WORK") && delivered.contains("MORE-WORK"),
            "the whole turn's work must reach the operator, not just the last \
             slice — got {delivered:?}"
        );
        assert!(
            delivered.contains("FINAL-SLICE"),
            "...including the concluding call — got {delivered:?}"
        );
        assert!(
            !delivered.contains("<think>"),
            "and it is TEXT, not markup — got {delivered:?}"
        );
    }

    /// The other side of the same rule: once the thought IS closed, we can tell
    /// scratch from answer, so the scratch stays out.
    #[test]
    fn a_closed_thought_is_scratch_and_never_reaches_the_deliverable() {
        let mut messages = vec![Message::user("u")];
        let mut turn = TurnAccum::default();
        turn.absorb("PRIVATE-SCRATCH ", "");
        turn.close_thought();
        turn.absorb("", "the real answer ");
        turn.hand_back(&mut messages);

        let mut final_msg = Message::assistant("and its end");
        turn.fold(&mut messages, &mut final_msg);
        let delivered = final_msg.content.as_deref().unwrap_or("");
        assert_eq!(delivered, "the real answer and its end");
        assert!(!delivered.contains("PRIVATE-SCRATCH"));
    }

    /// Reasoning that arrives after the close is kept, but stays out of the
    /// deliverable. Dropping it is the discard-the-work bug in miniature;
    /// putting it in the answer is the scratch-work-in-the-deliverable bug.
    /// It belongs inside the block that is already closed.
    #[test]
    fn post_close_reasoning_is_carried_back_but_never_delivered() {
        let mut messages = vec![Message::user("u")];
        let mut turn = TurnAccum::default();
        turn.absorb("first thoughts ", "");
        turn.close_thought();
        turn.absorb("MORE-SCRATCH ", "the answer");

        assert!(
            turn.thought.contains("MORE-SCRATCH"),
            "post-close reasoning must still be carried back — thought={:?}",
            turn.thought
        );
        assert!(
            !turn.answer.contains("MORE-SCRATCH"),
            "...but it is NOT the deliverable — answer={:?}",
            turn.answer
        );
        turn.hand_back(&mut messages);
        let body = messages.last().unwrap().content.as_deref().unwrap_or("");
        assert!(
            body.contains("MORE-SCRATCH"),
            "the prefill carries it so the model does not re-derive it — got {body:?}"
        );
        let close = crate::budget_request::THINK_CLOSE.trim();
        assert!(
            body.find("MORE-SCRATCH").unwrap() < body.find(close).unwrap(),
            "and it sits INSIDE the closed block — got {body:?}"
        );
    }

    /// The inline-think test is ANCHORED at the start, not counted anywhere in
    /// the string. An unanchored `opens > closes` misclassifies any answer
    /// that QUOTES the opening delimiter as reasoning — which is exactly what
    /// a reviewer of this very file writes. Same bug class as the
    /// `rfind("</think>")` that truncated a quoting answer.
    #[test]
    fn an_answer_that_quotes_the_delimiter_is_not_mistaken_for_reasoning() {
        let mut turn = TurnAccum::default();
        turn.absorb(
            "",
            "The runtime prefixes `<think>` to the accumulation before handing it back.",
        );
        assert!(
            !turn.is_reasoning,
            "quoting the delimiter mid-sentence is not thinking — thought={:?}",
            turn.thought
        );
        assert!(
            turn.answer.contains("prefixes"),
            "the quoting text is the ANSWER — answer={:?}",
            turn.answer
        );
    }

    /// Only the FIRST slice decides whether the accumulation carries its own
    /// opener, because the flag governs whether `<think>` is prefixed to the
    /// WHOLE thought. Setting it on any inline slice let a later one delete
    /// the opener from an accumulation that began as `reasoning_content`.
    #[test]
    fn a_later_inline_slice_cannot_strip_the_openers_from_an_earlier_one() {
        let mut messages = vec![Message::user("u")];
        let mut turn = TurnAccum::default();
        turn.absorb("started in the reasoning field ", "");
        turn.absorb("", "<think>\nand continued inline");
        turn.hand_back(&mut messages);
        let body = messages.last().unwrap().content.as_deref().unwrap_or("");
        assert!(
            body.starts_with(crate::budget_request::THINK_OPEN),
            "an accumulation that began WITHOUT its own opener still needs one — got {body:?}"
        );
    }

    /// The deliverable must be TEXT, never markup — but the strip is anchored,
    /// so an answer that merely quotes the delimiter keeps its text.
    #[test]
    fn the_deliverable_is_stripped_only_when_it_leads_with_markup() {
        assert_eq!(
            as_deliverable_text("<think>\nscratch work\n</think>\n"),
            "scratch work"
        );
        let quoting = "The opener is `<think>` and the closer is `</think>`.";
        assert_eq!(
            as_deliverable_text(quoting),
            quoting,
            "a quoting answer is handed over verbatim"
        );
    }

    /// An EMPTY completion says nothing about the work already banked. The
    /// accumulation survives it; only a proven-DEGENERATE one is abandoned.
    #[test]
    fn folding_prefers_the_answer_region_and_takes_the_prefill_with_it() {
        let mut messages = vec![Message::user("u")];
        let mut turn = TurnAccum::default();
        turn.absorb("scratch reasoning ", "");
        turn.close_thought();
        turn.absorb("", "the answer so far ");
        turn.hand_back(&mut messages);
        assert_eq!(messages.len(), 2);

        let mut final_msg = Message::assistant("and its conclusion");
        turn.fold(&mut messages, &mut final_msg);
        assert_eq!(
            final_msg.content.as_deref(),
            Some("the answer so far and its conclusion"),
            "the deliverable is the ANSWER region plus this call, never the scratch work"
        );
        assert_eq!(messages.len(), 1, "the prefill went with the fold");
        assert!(!turn.has_prefill());
        assert!(
            turn.pending_answer().is_none(),
            "nothing left to override once folded"
        );
    }
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
            lexically_repaired: None,
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

    /// (#2792) Captures request bodies for the pre-send bound test. A static
    /// because httpmock's matcher is a `fn` pointer and cannot close over
    /// local state.
    static PRE_SEND_WIRE: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
        std::sync::OnceLock::new();

    fn capture_pre_send_wire(req: &httpmock::prelude::HttpMockRequest) -> bool {
        if let Some(cell) = PRE_SEND_WIRE.get() {
            let body = req.body.as_ref().map(|b| String::from_utf8_lossy(b).to_string());
            if let Some(b) = body {
                cell.lock().unwrap().push(b);
            }
        }
        true
    }

    /// (#2792 reopened) darkmux must not SEND a request it has already
    /// computed is over the window its profile declares.
    ///
    /// The first fix made the compaction TRIGGER see the thread about to be
    /// sent, which was necessary and not sufficient: re-running the dogfood on
    /// that fix measured 4 of 27 turns still going out at ~38.4-38.9k against a
    /// declared 32,000. Compaction runs BETWEEN turns; a tool result landing
    /// after the last compaction and before the send grows the thread WITHIN
    /// one, and the soft trim protects exactly that recent window. On a
    /// correctly-loaded model each of those four requests is an HTTP 400 and
    /// the dispatch dies.
    ///
    /// This pins the bound at the only point it can be right: immediately
    /// before the request is built, when nothing further will reduce it.
    #[test]
    #[serial_test::serial]
    fn an_over_window_prompt_is_trimmed_before_it_is_sent() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            // Compaction OFF, so this can only pass via the pre-send bound.
            compactor_model: None,
            threshold_tokens: u32::MAX,
            threshold_ratio: None,
            context_window: Some(8_000),
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        // Capture the REAL request body. `MockMatcherFunction` is a plain
        // `fn` pointer, not a closure, so the capture has to go through a
        // module-level static — an earlier revision of this test tried a
        // closure, could not, and left the dead `Arc` behind as a
        // clippy-silencer while asserting only on the trajectory. That is
        // precisely the gap this now closes: a trajectory-only assertion
        // cannot tell "trimmed the wire" from "recorded the event and sent
        // the original".
        PRE_SEND_WIRE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        PRE_SEND_WIRE.get().unwrap().lock().unwrap().clear();
        let _primary = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(capture_pre_send_wire);
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 100, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("presend").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());

        // One tool result in the PROTECTED recent window, far over the
        // 8,000-token (32,000-byte) budget. Neither compaction nor the soft
        // trim can touch it.
        let huge = "x".repeat(200_000);
        let initial = vec![
            Message::system("test system"),
            Message::user("seed"),
            Message::tool_result("call_1", "read", &huge),
        ];
        let tools = [Tool::Read];

        run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(1), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the dispatch must proceed, not fail");

        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .expect("trajectory must exist");
        let bound: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.pre_send_bound")
            .collect();

        assert!(
            !bound.is_empty(),
            "an over-window prompt must be recorded at the pre-send bound"
        );
        let ev = &bound[0];
        assert!(
            ev["tokens_before"].as_u64().unwrap() > 8_000,
            "the fixture must actually be over the declared window: {ev}"
        );
        assert!(
            ev["results_trimmed"].as_u64().unwrap() >= 1,
            "the oversized tool result must be trimmed even though it sits in \
             the protected recent window — that protection is what leaves this \
             case unhandled: {ev}"
        );
        assert!(
            ev["tokens_after"].as_u64().unwrap() < ev["tokens_before"].as_u64().unwrap(),
            "the trim must actually reduce the prompt: {ev}"
        );
        assert_eq!(
            ev["fits"].as_bool(), Some(true),
            "after trimming, the request must be inside the declared window: {ev}"
        );

        // THE WIRE, not just the record. Two implementations pass a
        // trajectory-only assertion and fail here: trimming a CLONE and
        // sending the original, and running the bound AFTER the request is
        // built. Both were demonstrated green against the previous revision.
        let bodies = PRE_SEND_WIRE.get().unwrap().lock().unwrap().clone();
        assert!(!bodies.is_empty(), "no request was captured");
        let body = &bodies[0];
        assert!(
            body.len() < 200_000,
            "the oversized result must not have reached the wire: {} bytes",
            body.len()
        );
        assert!(
            body.contains(crate::tool_result_prune::TOOL_RESULT_TRIM_MARKER_SENTINEL),
            "the body on the wire must carry the elision marker, proving the \
             TRIMMED thread was sent rather than the original"
        );
    }

    /// (#2792 round-4) The bound must measure against the endpoint's OWN
    /// count, not a flat chars/4 guess.
    ///
    /// Measured on a real dogfood run with the #2804 bound already in: the
    /// turn that overflows estimated 30,132 tokens against a 32,000 budget —
    /// under, so nothing trimmed — and the request the endpoint then counted
    /// was 38,434. The estimate under-shot by 27% at exactly the turn that
    /// matters, so the bound stayed silent on the only send it exists to
    /// catch and fired a turn later, once the thread was already over.
    ///
    /// chars/4 is not wrong about prose — measured 3.94 chars/token on this
    /// project's own TypeScript fixture. It is wrong about a thread whose
    /// mass is tool results and tool-call arguments, which tokenize nearer
    /// 2.9. The fix is to stop guessing the part darkmux already knows
    /// exactly: `usage.prompt_tokens` is ground truth for everything that was
    /// in the LAST request, so only the characters added SINCE that request
    /// need a ruler at all.
    ///
    /// THE FIXTURE MUST ISOLATE THE ANCHOR (round-5 merge gate). Its first
    /// version added ~90,000 characters per turn, which crosses a 32,000
    /// window on the 2.75 ruler ALONE — so it pinned "the ruler is not
    /// chars/4" and not "the estimate carries the endpoint's own count".
    /// Proven vacuous by mutation: deleting the anchor from the estimator, and
    /// even deleting the production line that INSTALLS the anchor, left it
    /// green.
    ///
    /// So the numbers are chosen to make only the anchored estimate cross. The
    /// endpoint reports 28,000 tokens for a nearly empty thread, then one turn
    /// adds ~20,000 characters. Every anchorless ruler stays far under the
    /// 32,000 window — chars/2.75 is ~7,400, chars/4 is ~5,100 — while
    /// 28,000 carried forward plus that delta is ~35,000 and must be bounded.
    #[test]
    #[serial_test::serial]
    fn the_bound_measures_growth_against_the_endpoints_own_count() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            // Compaction OFF, so this can only pass via the pre-send bound.
            compactor_model: None,
            threshold_tokens: u32::MAX,
            threshold_ratio: None,
            context_window: Some(32_000),
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        // Every turn: one echo tool call carrying ~45,000 characters of
        // argument, echoed straight back as a ~45,000-character result. Both
        // halves are counted by `measure_request_context`, so one turn grows
        // the thread by ~90,000 characters.
        let filler = "x".repeat(10_000);
        let tool_calls = serde_json::json!([{
            "id": "call_echo",
            "type": "function",
            "function": {
                "name": "echo",
                "arguments": serde_json::json!({ "text": filler }).to_string(),
            },
        }]);
        let _primary = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(tool_calls),
                "tool_calls",
                // The ground truth the estimate has to respect, and the
                // reason the fixture is sized the way it is. See the doc
                // comment: 28,000 reported against ~20,000 characters added,
                // so ONLY the anchored estimate crosses the window.
                28_000,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("anchored").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());

        let initial = vec![Message::system("test system"), Message::user("seed")];
        let tools = [Tool::Echo];

        run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(2), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the dispatch must proceed, not fail");

        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .expect("trajectory must exist");
        let bound: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "dispatch.pre_send_bound")
            .collect();

        assert!(
            !bound.is_empty(),
            "the second request adds ~20,000 characters on top of a thread \
             the endpoint already counted at 28,000 tokens, so it is over the \
             declared 32,000 window and must be bounded. NO ruler that ignores \
             that 28,000 gets there: chars/2.75 measures this request at \
             ~7,400 and chars/4 at ~5,100, and both would send it"
        );
        let ev = &bound[0];
        assert!(
            ev["tokens_before"].as_u64().unwrap() > 32_000,
            "the estimate must carry the endpoint's own 28,000 forward rather \
             than re-deriving the whole thread from characters: {ev}"
        );
        assert!(
            ev["results_trimmed"].as_u64().unwrap() >= 1,
            "the oversized echo result must be trimmed: {ev}"
        );
        assert!(
            ev["tokens_after"].as_u64().unwrap() < ev["tokens_before"].as_u64().unwrap(),
            "the trim must actually reduce the estimate: {ev}"
        );
    }

    /// (#2792 merge-gate, loop grain) A thread whose weight is a huge TAIL
    /// message and whose compactable MIDDLE is tiny must not kill the
    /// dispatch.
    ///
    /// Measuring occupancy every turn makes this shape trip the compaction
    /// trigger far more often; the compactor then cannot reach the #1389
    /// min-reduction bar, because the middle it is allowed to touch is
    /// `messages[2 .. n-4]` while the weight sits in the preserved tail.
    /// Before the refusal was made non-fatal, the resulting `Err` propagated
    /// through `?` and ended the whole dispatch — every banked turn lost,
    /// `result: "error"`, no envelope. That is #1221's lesson, and the
    /// occupancy change is what made it reachable.
    ///
    /// This is deliberately a LOOP-grain test. The pure-function tests above
    /// pass even when the call site is reverted, so they cannot pin the
    /// wiring; this one drives `run()` end to end.
    #[test]
    #[serial_test::serial]
    fn a_tiny_middle_under_a_huge_tail_skips_the_compaction_instead_of_killing_the_dispatch() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 5000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
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
                1000, // an honest report for a request that was small when sent
                50,
            ));
        });
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some(
                    "Summary: the assistant issued a read tool call against the workspace \
                     file and inspected the returned contents. No decisions were finalized \
                     and no files were modified. The next concrete action is to continue \
                     reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("tiny-middle").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());

        // n = 7: head 0..2, preserved tail 3..7, so the compactable middle is
        // exactly index 2 — one tiny message. The weight is the huge message
        // at the TAIL, which neither the soft-trim (last 6 never trimmed) nor
        // compaction (PRESERVE_TAIL = 4) may touch.
        let huge = "x".repeat(120_000); // ~30k tokens by chars/4
        let initial = vec![
            Message::system("test system"),
            Message::user("seed"),
            Message::user("tiny middle"),
            Message::assistant("ok"),
            Message::user("go"),
            Message::assistant("sure"),
            Message::user(&huge),
        ];
        let tools = [Tool::Read];

        let outcome = run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(4), None, None, None, std::collections::BTreeMap::new(), None,
        );

        outcome.expect(
            "a compaction the thread shape makes impossible must be SKIPPED, not fatal — \
             propagating it ends the dispatch and discards every banked turn (#1221)",
        );
        assert!(
            compactor_mock.hits() >= 1,
            "the scenario must actually have attempted a compaction, else it pins nothing"
        );
        assert!(
            crate::trajectory::recorded(tmp.path()).turns() >= 1,
            "the dispatch must have continued doing work after the refused compaction"
        );
        // (#2797 merge-gate) A refused attempt must NOT burn the operator's
        // `bail_after_compactions` budget. Unpinned in the first revision:
        // restoring the pre-fix "count attempts" increment left all 703 tests
        // green, and that increment is the one deciding whether an operator's
        // escalation bound is spent on work that never happened.
        // (#2797 merge-gate) A refused attempt must NOT burn the operator's
        // `bail_after_compactions` budget. Unpinned in the first revision:
        // restoring the pre-fix "count attempts" increment left all 703 tests
        // green, and that increment decides whether an escalation bound is
        // spent on work that never happened. Asserted against the trajectory
        // rather than a fixed number, because this scenario legitimately
        // installs a later compaction once the middle has re-grown — the claim
        // is that the counter tracks INSTALLS, not attempts.

        // The skip is recorded — an invisible refusal would leave a run that
        // declines to compact indistinguishable from one that never needed to.
        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .expect("trajectory must exist");
        let skipped: Vec<_> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "compaction.skipped")
            .collect();
        assert!(
            !skipped.is_empty(),
            "a refused compaction must be recorded as `compaction.skipped`"
        );
        assert!(
            skipped[0]["reason"].as_str().unwrap_or("").contains("less than"),
            "the recorded reason must name the guard that refused it: {:?}",
            skipped[0]["reason"]
        );

        let installed = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "compaction")
            .count();
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).compactions() as usize, installed,
            "the counter must equal INSTALLED compactions ({installed}), not \
             installs + the {} refused attempt(s) that changed nothing",
            skipped.len()
        );
        assert!(
            !skipped.is_empty() && crate::trajectory::recorded(tmp.path()).compactions() as usize != installed + skipped.len(),
            "the scenario must contain at least one refusal that is excluded from \
             the count, else this pins nothing"
        );
    }

    /// (#3074) LOOP grain: a structured compaction whose reply was cut off
    /// and lexically repaired still installs (#401), and the installed
    /// `compaction` trajectory event says so, so an operator can see the
    /// summary is lossy without reading stderr.
    #[test]
    #[serial_test::serial]
    fn a_lexically_repaired_structured_compaction_is_flagged_on_the_trajectory_event() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 5000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::StructuredSlot,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
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
                6000,
                50,
            ));
        });
        let truncated = r#"{"objective": "finish", "current_truth": {}, "compaction_metadata": {"schema_version": "0.1", "generation": 1, "source_message_count": 3}, "completed_decisions": "decision one; decis"#;
        let _compactor = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200)
                .json_body(chat_response_json(Some(truncated), None, "length", 500, 30));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("repaired-compaction").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let big = "y".repeat(40_000);
        let initial = vec![
            Message::system("test system"),
            Message::user("seed"),
            Message::user(&big),
            Message::assistant("ok"),
            Message::user("go"),
            Message::assistant("sure"),
            Message::user("one"),
            Message::assistant("two"),
        ];
        let tools = [Tool::Read];
        run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(3), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the scenario completes");
        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .unwrap();
        let installed: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| v["type"] == "compaction")
            .collect();
        assert!(!installed.is_empty(), "the scenario must install a compaction: {raw}");
        assert_eq!(
            installed[0]["lexically_repaired"], true,
            "the installed compaction event must carry the repair flag: {}", installed[0]
        );
    }

    /// (#2902 step 1b) LOOP grain: every compactor call the loop makes lands
    /// in the trajectory as exactly one `compaction.call` event, installed or
    /// refused, and a turn names the model the server says answered it. The
    /// scenario is the one above (refusals, then a later install), so both
    /// outcomes are exercised. `model.completed` stays one per PRIMARY call:
    /// a consumer counting turns must never see a compactor call.
    #[test]
    #[serial_test::serial]
    fn every_compactor_call_lands_as_one_compaction_call_event_and_turns_name_their_model() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 5000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        let primary = server.mock(|when, then| {
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
                1000,
                50,
            ));
        });
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some(
                    "Summary: the assistant issued a read tool call against the workspace \
                     file and inspected the returned contents. No decisions were finalized \
                     and no files were modified. The next concrete action is to continue \
                     reading and then act on what the file contains.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("compaction-call").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let huge = "x".repeat(120_000);
        let initial = vec![
            Message::system("test system"),
            Message::user("seed"),
            Message::user("tiny middle"),
            Message::assistant("ok"),
            Message::user("go"),
            Message::assistant("sure"),
            Message::user(&huge),
        ];
        let tools = [Tool::Read];
        run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(4), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the scenario completes");
        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .unwrap();
        let events: Vec<serde_json::Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let calls: Vec<&serde_json::Value> =
            events.iter().filter(|v| v["type"] == "compaction.call").collect();
        // (#2915) Every compaction attempt is announced BEFORE its calls:
        // each `compaction.call` is preceded, since the previous call of a
        // different generation, by a `compaction.start` of its own
        // generation naming the compactor.
        let mut open_generation: Option<u64> = None;
        let mut starts = 0;
        for e in &events {
            if e["type"] == "compaction.start" {
                starts += 1;
                assert_eq!(e["requested_model"], "test-compactor", "{e}");
                open_generation = e["generation"].as_u64();
            } else if e["type"] == "compaction.call" {
                assert_eq!(
                    open_generation,
                    e["generation"].as_u64(),
                    "a compactor call must follow its own generation's `compaction.start`: {e}"
                );
            }
        }
        assert!(starts >= 2, "one start per compaction attempt");
        assert!(compactor_mock.hits() >= 2, "the scenario must compact more than once");
        assert_eq!(
            calls.len(),
            compactor_mock.hits(),
            "one `compaction.call` per compactor call the server answered"
        );
        for c in &calls {
            assert_eq!(c["requested_model"], "test-compactor", "{c}");
            assert_eq!(c["reported_model"], "ignored-by-test", "{c}");
            assert_eq!(c["usage"]["total_tokens"], 530, "{c}");
        }
        let turns: Vec<&serde_json::Value> =
            events.iter().filter(|v| v["type"] == "model.completed").collect();
        assert_eq!(turns.len(), primary.hits(), "model.completed is one per PRIMARY call");
        assert!(turns.iter().all(|t| t["reported_model"] == "ignored-by-test"), "{turns:?}");
    }

    // ─── effective_prompt_occupancy (#2792) ─────────────────────────

    /// The defect, reproduced at its own grain: the endpoint reported a small
    /// count for the request that already went out, the loop then appended a
    /// large tool result, and the compaction decision saw only the stale
    /// number. Measured on a real run as 6,290 reported followed by a 38,446
    /// request against a declared 32,000 window — which a correctly-loaded
    /// model answers with HTTP 400.
    #[test]
    fn a_big_tool_result_appended_after_the_last_report_raises_the_occupancy() {
        let reported: u32 = 6_290;
        // ~32k tokens of tool output arriving AFTER `reported` was measured.
        let big_tool_result = "x".repeat(128_000);
        let messages = vec![
            Message::system("sys"),
            Message::user("do the thing"),
            Message::user(&big_tool_result),
        ];

        // The anchor as the loop builds it: the characters of the request
        // that reported `reported`, which is everything before the tool
        // result landed.
        let anchor = Some(PromptAnchor {
            chars: "sys".len() + "do the thing".len(),
            tokens: reported,
        });
        let occupancy = effective_prompt_occupancy(&messages, reported, anchor);

        assert!(
            occupancy > reported,
            "occupancy must reflect the thread about to be SENT, not the last \
             request's report: got {occupancy}, reported {reported}"
        );
        assert!(
            occupancy >= 32_000,
            "a ~32k-token tool result must push occupancy past a 32,000 window \
             so compaction can fire BEFORE the oversized request goes out: {occupancy}"
        );
    }

    /// The endpoint's own count stays authoritative when it is the larger
    /// number — it is ground truth for everything already sent, and chars/4
    /// under-counts some tokenizations. This is what makes the change
    /// strictly compact-earlier and never compact-later.
    #[test]
    fn the_endpoint_count_still_wins_when_it_is_larger_than_the_estimate() {
        let reported: u32 = 50_000;
        let messages = vec![Message::system("sys"), Message::user("tiny")];
        assert_eq!(
            effective_prompt_occupancy(&messages, reported, None),
            reported,
            "a larger reported count must not be lowered by a small local estimate"
        );
    }

    /// No messages and no report is zero, not a panic — the loop consults this
    /// on turn one before anything has been reported.
    #[test]
    fn occupancy_of_an_empty_thread_with_no_report_is_zero() {
        assert_eq!(effective_prompt_occupancy(&[], 0, None), 0);
    }

    /// (#2792 round-4) The estimator reproduces the measured dogfood turn.
    ///
    /// Real numbers from the instrumented run: the endpoint counted 6,290
    /// tokens for a request carrying 14,166 message characters, the next turn
    /// added 94,312 characters, and the endpoint counted the result at
    /// 38,434. chars/4 said 30,132 — under a 32,000 window, so the bound
    /// stayed silent. The anchored estimate has to land above the window.
    #[test]
    fn the_estimator_reproduces_the_measured_overflow_turn() {
        let anchor = Some(PromptAnchor { chars: 14_166, tokens: 6_290 });
        let est = estimate_prompt_tokens(14_166 + 94_312, 12_050, anchor);

        let flat_chars_over_four = ((14_166 + 94_312 + 12_050) / 4) as u32;
        assert_eq!(
            flat_chars_over_four, 30_132,
            "the fixture must be the measured turn, not a rounded retelling"
        );
        assert!(
            est > 32_000,
            "the turn the endpoint counted at 38,434 must measure over a \
             32,000 window: got {est}"
        );
        // Within 10% of ground truth, and on the SAFE side of it.
        assert!(
            (38_434..=42_277).contains(&est),
            "the estimate must track the endpoint's 38,434 closely and err \
             high, never low: got {est}"
        );
    }

    /// (#2792 round-5) The compaction TRIGGER keeps the ruler it has always
    /// used when there is no ground truth to justify changing it.
    ///
    /// Round 4 routed this decision through the bound's estimator, whose 2.75
    /// ruler is deliberately conservative. On every anchorless turn — turn
    /// one, any usage-less turn, every post-compaction and post-trim turn —
    /// that silently moved the trigger 45% earlier, with no measurement
    /// behind it, on the mechanism the revert commit found was what actually
    /// keeps the thread bounded. The bound may err high; this decision may
    /// not drift.
    #[test]
    fn the_anchorless_compaction_trigger_keeps_its_own_ruler() {
        let chunk = "y".repeat(40_000);
        let messages = vec![Message::system("sys"), Message::user(&chunk)];
        let chars = "sys".len() + 40_000;

        let occupancy = effective_prompt_occupancy(&messages, 0, None);
        let bounds_ruler = (chars as f64 / UNCOUNTED_CHARS_PER_TOKEN) as u32;

        assert!(
            occupancy < bounds_ruler,
            "the trigger must not adopt the pre-send bound's conservative \
             ruler on a turn with no endpoint count to anchor against: got \
             {occupancy}, the bound's ruler would say {bounds_ruler}"
        );
        assert_eq!(
            occupancy,
            (chars / 4) as u32,
            "and the ruler it keeps is the chars/4 one it has always used"
        );
    }

    /// (#2792 round-5) Each arm of the bound's "why it could not fit" reason
    /// must match the condition that selects it.
    ///
    /// The last arm read "the weight is not in tool results" and is reached
    /// exactly when trimmable tool results EXIST — so it stated the opposite
    /// of its own condition. Measured by the review: a turn printed it while
    /// holding results that the very next turn trimmed 44,000 bytes out of,
    /// sending the operator to raise n_ctx when clearing results would have
    /// worked.
    #[test]
    fn every_arm_of_the_bounds_reason_matches_its_own_condition() {
        // Trimmed something and still over.
        let s = why_the_bound_could_not_fit(2, 5, 0);
        assert!(s.contains("after this trim"), "{s}");

        // Nothing big enough to trim: the weight really is spread thin.
        let s = why_the_bound_could_not_fit(0, 0, 0);
        assert!(s.contains("spread across many small ones"), "{s}");

        // Trimmable results exist and every one is already elided.
        let s = why_the_bound_could_not_fit(0, 4, 4);
        assert!(s.contains("already been elided"), "{s}");

        // Trimmable results exist, not all elided, and none were trimmed.
        // THE ARM THAT LIED. It must not claim the weight is elsewhere.
        let s = why_the_bound_could_not_fit(0, 4, 1);
        assert!(
            !s.contains("not in tool results"),
            "this arm is selected BECAUSE trimmable tool results exist; it \
             cannot tell the operator the weight is not in them: {s}"
        );
        assert!(s.contains("could not reduce"), "{s}");
    }

    /// (#2792 round-4) The budget is the estimator's inverse. A target derived
    /// on a different ruler would leave the trimmed thread still measuring
    /// over — the bound trimming and then certifying its own failure.
    #[test]
    fn the_chars_budget_inverts_the_estimate() {
        for anchor in [
            None,
            Some(PromptAnchor { chars: 14_166, tokens: 6_290 }),
            Some(PromptAnchor { chars: 1_000, tokens: 100 }),
            // An endpoint reporting far FEWER tokens than the characters it
            // was sent. Here the anchored budget alone exceeds the chars/4
            // budget, so this is the case where taking the min actually
            // binds — without it the bound trims to a target the floor still
            // measures over, and certifies a failure as a fix.
            Some(PromptAnchor { chars: 30_000, tokens: 100 }),
            // ANCHORS AT OR PAST THE WINDOW. Every anchor above has
            // `tokens < window`, so none of them reaches the branch where the
            // two functions used to disagree — the budget took its flat
            // branch while the estimate took its anchored one, handing back a
            // target larger than the thread. These three red without the fix.
            // Reachable whenever the loaded n_ctx exceeds the declared one,
            // which is the configuration the bound exists for.
            Some(PromptAnchor { chars: 60_000, tokens: 40_000 }),
            Some(PromptAnchor { chars: 70_000, tokens: 33_000 }),
            Some(PromptAnchor { chars: 50_000, tokens: 32_000 }),
            // AN ANCHOR WHOSE CHARS EXCEED THE chars/4 BUDGET. Every anchor
            // above sits below `4 * window - tools` (75,950 here), so the
            // `flat_budget.min(...)` cap never binds and dropping it survived
            // a mutation sweep. With 200,000 characters the cap is the only
            // thing standing between the budget and a target the estimate
            // measures at 77,109 against a 32,000 window — the bound trimming
            // and then certifying its own failure.
            Some(PromptAnchor { chars: 200_000, tokens: 40_000 }),
        ] {
            let budget = message_chars_budget(32_000, 12_050, anchor);
            let est = estimate_prompt_tokens(budget, 12_050, anchor);
            assert!(
                est <= 32_000,
                "a thread trimmed exactly to the budget must measure inside \
                 the window: budget {budget} chars -> {est} tokens ({anchor:?})"
            );
        }
    }

    /// (#2792 round-4) An anchor whose own count already meets the window
    /// leaves no headroom to spend, and trimming removes characters the
    /// endpoint already counted — so the pairing no longer holds and the flat
    /// ruler takes over. Pinned because the arithmetic underflows otherwise.
    #[test]
    fn an_anchor_at_or_past_the_window_falls_back_to_the_flat_ruler() {
        let anchor = Some(PromptAnchor { chars: 100_000, tokens: 40_000 });
        let budget = message_chars_budget(32_000, 12_050, anchor);
        assert!(
            budget > 0 && budget < 100_000,
            "the budget must be a real target below the anchor, not zero and \
             not the anchor itself: {budget}"
        );
        // And a thread that SHRANK below its anchor is estimated flat too,
        // rather than by a negative delta. Asserted as BEHAVIOR, not as the
        // expression under test restated: an earlier revision asserted
        // equality against `(chars / UNCOUNTED_CHARS_PER_TOKEN)`, which reds
        // for any change to the constant and pins nothing about the branch.
        let est = estimate_prompt_tokens(50_000, 12_050, anchor);
        assert!(
            est < 40_000,
            "half the anchor's characters must not still be charged the \
             anchor's full 40,000-token count: {est}"
        );
        assert!(
            est > (62_050 / 4),
            "and it must not fall below the chars/4 floor either: {est}"
        );
    }

    /// (#2792 cost check) The occupancy walk now runs every turn, so its cost
    /// is on the hot path. Measured here rather than asserted: a transcript far
    /// larger than a real dispatch's must still cost orders of magnitude less
    /// than the model call it precedes.
    #[test]
    fn occupancy_cost_is_negligible_against_a_model_turn() {
        // ~2 MB of transcript — well beyond a real dispatch at any window.
        let chunk = "y".repeat(20_000);
        let messages: Vec<Message> =
            (0..100).map(|_| Message::user(&chunk)).collect();

        let start = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(effective_prompt_occupancy(&messages, 0, None));
        }
        let per_call = start.elapsed() / 100;

        eprintln!("effective_prompt_occupancy over ~2MB: {per_call:?} per call");
        assert!(
            per_call < std::time::Duration::from_millis(10),
            "the per-turn occupancy walk must stay far below a model turn's \
             seconds-scale cost: {per_call:?}"
        );
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
        let server = crate::test_support::GuardedMockServer::start();
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


    /// (#1221) An EMPTY completion at the boundary says nothing about the work
    /// already banked.
    ///
    /// The intra-turn stall recovery drops the useless call and nudges — that
    /// is pre-existing #414 behavior and it stays. What it must NOT do is take
    /// the accumulation with it: discarding five productive checkpoints because
    /// the sixth call came back blank is precisely the discard-the-turn bug
    /// this whole feature exists to end, reappearing one layer down.
    ///
    /// Two MUTUALLY EXCLUSIVE mocks keyed on the request body — `mock()` takes
    /// an FnOnce that runs ONCE at registration, so a call counter inside it
    /// never advances and the mock answers identically forever.
    #[test]
    #[serial_test::serial]
    fn an_empty_call_does_not_discard_the_work_already_banked() {
        const BANKED: &str = "BANKED-WORK-FROM-AN-EARLIER-CHECKPOINT";
        let server = crate::test_support::GuardedMockServer::start();
        // First call: real content, cut at the boundary.
        let _first = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                !b.contains(BANKED)
            });
            then.status(200)
                .json_body(chat_response_json(Some(BANKED), None, "length", 100, 200));
        });
        // Every later call (the request now carries the prefill): blank.
        let _blank = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.contains(BANKED)
            });
            then.status(200)
                .json_body(chat_response_json(Some(""), None, "length", 100, 200));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ckblank").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("t"), Message::user("answer")];
        let tools: [Tool; 0] = [];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(10), None, Some(200), Some(200), std::collections::BTreeMap::new(), None,
        )
        .expect("a blank call after real work returns Ok");

        assert!(
            matches!(
                outcome.terminal_reason,
                TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted)
            ),
            "repeated blank calls must still exhaust the recovery budget and escalate, got {:?}",
            outcome.terminal_reason
        );
        // The banked work is reachable EXACTLY as main.rs reaches it.
        let deliverable = outcome
            .final_answer
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| {
                outcome
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "assistant")
                    .and_then(|m| m.content.clone())
            })
            .unwrap_or_default();
        assert!(
            deliverable.contains(BANKED),
            "the work banked before the blank call must still be the deliverable — got {deliverable:?}"
        );
        // And it is still ONE logical turn: a blank call is not a boundary.
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 1,
            "an empty call mid-accumulation does not start a new turn (turns={})",
            crate::trajectory::recorded(tmp.path()).turns()
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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

        // The gate fired, so the thought was closed and the model answered
        // from it. What must be true now is about the DELIVERABLE, not about
        // delimiters left lying in history: the terminal fold removes the
        // prefill and hands back the answer region, so a concluded turn
        // correctly leaves no `<think>` behind at all.
        // Exactly what main.rs does: prefer the answer the loop identified.
        let final_assistant = outcome
            .final_answer
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| {
                outcome
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "assistant")
                    .and_then(|m| m.content.clone())
            })
            .unwrap_or_default();

        // Asserts the deliverable is not raw MARKUP. Not `!contains` — this
        // mock replays its `<think>` opener on every call, which a real
        // continuation never does (the model resumes inside the block darkmux
        // handed back), so interior copies are a fixture artifact rather than a
        // property of the code.
        assert!(
            !final_assistant.trim_start().starts_with("<think>"),
            "the deliverable must be text, not an unopened think block — got {:?}",
            &final_assistant[..final_assistant.len().min(120)]
        );
        // A conclude is not a turn boundary: many API calls, still one turn.
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 1,
            "a conclude closes the thought, not the turn; turns={} means the loop \
             treated it as a boundary and restarted the thought",
            crate::trajectory::recorded(tmp.path()).turns()
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns() < 100,
            "cumulative bail must fire before MAX_TURNS; got turns={}",
            crate::trajectory::recorded(tmp.path()).turns()
        );
        // The cumulative-tokens sum must have crossed the cap.
        assert!(
            crate::trajectory::recorded(tmp.path()).tokens.completion >= 250_000,
            "cumulative bail fires when sum >= 250000; got {}",
            crate::trajectory::recorded(tmp.path()).tokens.completion
        );
    }

    /// (#423) Negative case: when each turn reports modest token
    /// usage and the loop terminates normally on stop, the
    /// cumulative-budget check must NOT trip. Asserts the normal
    /// stop path still fires for healthy dispatches.
    #[test]
    #[serial_test::serial]
    fn loop_does_not_escalate_when_under_cumulative_budget() {
        let server = crate::test_support::GuardedMockServer::start();
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
        assert!(crate::trajectory::recorded(tmp.path()).tokens.completion < 250_000);
    }

    /// This test pairs with the existing
    /// `loop_runs_against_mock_and_terminates_on_stop` (Stop case)
    /// to lock both terminal reasons. MaxTurns specifically asserts
    /// the loop returns Ok(outcome) — the JSON envelope path in
    /// main.rs reads outcome.terminal_reason and emits result=max_turns.
    #[test]
    #[serial_test::serial]
    fn loop_returns_maxturns_terminal_reason_when_cap_hit() {
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns() >= 100,
            "expected >= MAX_TURNS turns; got {}",
            crate::trajectory::recorded(tmp.path()).turns()
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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

        let server = crate::test_support::GuardedMockServer::start();
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
    ///   - more than one recorded turn (the bail was promoted, not exited)
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
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns() > 1,
            "promotion must continue the loop past turn 1; got turns={} (pre-#406 silent bail at turn 1)",
            crate::trajectory::recorded(tmp.path()).turns()
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

    /// (#1959) The exact shape a live crawl produced: the per-call cap landed
    /// mid-serialization of the FIFTH `read`, so four calls carried arguments
    /// and one carried none. Every one of them was dispatched. The empty call
    /// failed, stayed in the transcript, and LMStudio answered the next
    /// streaming request with HTTP 500 — the dispatch ran 67s and returned no
    /// envelope at all.
    fn salvage_msg(args: &[&str]) -> Message {
        Message {
            role: "assistant".to_string(),
            content: Some("half a thought".to_string()),
            reasoning_content: None,
            tool_calls: Some(
                args.iter()
                    .enumerate()
                    .map(|(i, a)| crate::lmstudio::ToolCall {
                        id: format!("call_{i}"),
                        kind: "function".to_string(),
                        function: crate::lmstudio::FunctionCall {
                            name: "read".to_string(),
                            arguments: (*a).to_string(),
                        },
                        extra_content: None,
                    })
                    .collect(),
            ),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn a_tool_call_the_cap_cut_in_half_is_dropped_not_dispatched() {
        let mut msg = salvage_msg(&[
            r#"{"path":"/workspace/bookend.rs"}"#,
            r#"{"path":"/workspace/daemon_probe.rs"}"#,
            r#"{"path":"/workspace/integrity.rs"}"#,
            r#"{"path":"/workspace/presence.rs"}"#,
            "", // the cap landed here
        ]);
        assert_eq!(count_well_formed_tool_calls(&msg), 4, "precondition");

        retain_well_formed_tool_calls(&mut msg);

        let kept = msg.tool_calls.as_ref().expect("four calls survive");
        assert_eq!(kept.len(), 4, "the log said 4; the message must agree");
        assert!(
            kept.iter()
                .all(|tc| serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok()),
            "an unparseable `arguments` reaching the transcript is what 500s the next request"
        );
    }

    #[test]
    fn dropping_every_call_leaves_no_tool_calls_rather_than_an_empty_list() {
        // `resolve_finish_reason` asks whether any tool calls remain, and
        // `Some([])` answers "yes" — a turn with nothing to dispatch would be
        // routed as though it had work to do.
        let mut msg = salvage_msg(&["", r#"{"path":"#]);
        retain_well_formed_tool_calls(&mut msg);
        assert!(
            msg.tool_calls.is_none(),
            "an empty vector is not the same as no tool calls"
        );
    }

    #[test]
    fn a_turn_whose_calls_all_parse_is_left_exactly_as_it_was() {
        let mut msg = salvage_msg(&[
            r#"{"path":"/workspace/a.rs"}"#,
            r#"{"path":"/workspace/b.rs"}"#,
        ]);
        let before: Vec<String> = msg
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .map(|tc| tc.function.arguments.clone())
            .collect();
        retain_well_formed_tool_calls(&mut msg);
        let after: Vec<String> = msg
            .tool_calls
            .as_ref()
            .expect("both calls survive")
            .iter()
            .map(|tc| tc.function.arguments.clone())
            .collect();
        assert_eq!(after, before, "the common case must be untouched");
    }

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
        let server = crate::test_support::GuardedMockServer::start();
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
        run(
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
            crate::trajectory::recorded(tmp.path()).turns() >= 1,
            "salvage must let the loop continue past turn 1 (got turns={})",
            crate::trajectory::recorded(tmp.path()).turns()
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

    // ─── (#2169) malformed structured tool-call names ──────────────────

    fn malformed_call(id: &str, bogus_name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {
                "name": bogus_name,
                "arguments": "{}",
            },
        })
    }

    fn valid_read_call(id: &str, path: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {
                "name": "read",
                "arguments": format!("{{\"path\":\"{path}\",\"offset\":1,\"limit\":10}}"),
            },
        })
    }

    /// Issue #2169's own test. A turn carries 3 valid `read` calls and 5
    /// structured calls whose `name` is model content sliced out of a
    /// `[TOOL_CALLS]` marker (the observed Devstral 2 + LM Studio shape).
    /// Only the 3 valid calls dispatch; the 5 invalid ones are coalesced
    /// into exactly one feedback message and one trajectory detector
    /// event naming the model, never becoming 5 separate tool.completed
    /// "doesn't exist" failures.
    #[test]
    #[serial_test::serial]
    fn malformed_tool_call_names_are_never_dispatched_and_coalesced() {
        let server = crate::test_support::GuardedMockServer::start();
        let mut calls = vec![
            valid_read_call("call_ok_1", "/workspace/a.txt"),
            valid_read_call("call_ok_2", "/workspace/b.txt"),
            valid_read_call("call_ok_3", "/workspace/c.txt"),
        ];
        for i in 0..5 {
            calls.push(malformed_call(
                &format!("call_bad_{i}"),
                &format!("}} catch (error) {{ log(error) }} --- [TOOL_CALLS]bogus_{i}"),
            ));
        }
        // First turn: the mixed valid/invalid batch above. Second turn
        // (request body now carries `role:tool` messages): a clean stop,
        // so the run terminates deterministically instead of relying on
        // MAX_TURNS.
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::Value::Array(calls.clone())),
                "tool_calls",
                100,
                50,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("malformed-tool-names").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("malformed-name calls must not error the dispatch (#2169)");

        assert!(
            matches!(outcome.terminal_reason, TerminalReason::Stop),
            "the turn must progress to a clean stop, not stall or escalate: {:?}",
            outcome.terminal_reason
        );

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let events: Vec<serde_json::Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        // Exactly 3 executed — never 8, never 5 "doesn't exist" failures.
        let tool_completed =
            events.iter().filter(|v| v["type"] == "tool.completed").count();
        assert_eq!(
            tool_completed, 3,
            "only the 3 valid calls should ever be dispatched; invalid-name calls must \
             never reach tools::dispatch"
        );

        // Exactly one coalesced detector event for the whole turn's batch,
        // naming the model and carrying the count.
        let malformed_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["type"] == "dispatch.tool.malformed_names")
            .collect();
        assert_eq!(
            malformed_events.len(),
            1,
            "5 invalid-name calls in ONE turn must coalesce into exactly ONE detector event, \
             got: {malformed_events:?}"
        );
        assert_eq!(malformed_events[0]["count"], 5);
        assert_eq!(malformed_events[0]["model"], "devstral-test");
        // (merge-gate MUST FIX 1) These 5 names match no real darkmux tool —
        // must classify as "not_a_tool", never "real_tool_not_granted".
        assert_eq!(malformed_events[0]["reason"], "not_a_tool");
        let prefix = malformed_events[0]["sample_name_prefix"].as_str().unwrap();
        assert!(prefix.chars().count() <= 40, "sample_name_prefix must be capped at 40 chars: {prefix:?}");
        assert!(!prefix.contains('\n'), "sample_name_prefix must have newlines stripped: {prefix:?}");

        // One feedback-injection event on turn 2's request (drained at the
        // top of the NEXT loop iteration, same as every other detector
        // signal in this file).
        let feedback_injected = events.iter().any(|v| {
            v["type"] == "dispatch.feedback.injected"
                && v["signal_kinds"].to_string().contains("malformed_tool_names")
        });
        assert!(
            feedback_injected,
            "exactly one coalesced feedback message must be queued and drained next turn"
        );

        // (merge-gate CONSIDER 5) Every one of the 8 tool_call_ids from the
        // assistant's tool_calls array must get EXACTLY one `tool`-role
        // message — LM Studio's OpenAI-compatible endpoint requires one per
        // id, and the coalesced-feedback shape must not silently drop any.
        // Without this, deleting the invalid-id message-push loop still
        // left every assertion above green (they only count events, never
        // check message-list completeness).
        let mut got_ids: Vec<&str> = outcome
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        got_ids.sort_unstable();
        let mut want_ids: Vec<&str> = vec![
            "call_ok_1", "call_ok_2", "call_ok_3", "call_bad_0", "call_bad_1", "call_bad_2",
            "call_bad_3", "call_bad_4",
        ];
        want_ids.sort_unstable();
        assert_eq!(
            got_ids, want_ids,
            "every tool_call_id (executed AND malformed) must have exactly one tool-role \
             message — a missing one breaks LM Studio's protocol on the NEXT request"
        );
    }

    /// (#419 interaction) Invalid-name calls must never reach the
    /// consecutive-failure cascade detector. Three calls sharing the
    /// SAME bogus name+args (a signature that WOULD trip
    /// `DEFAULT_WARN_THRESHOLD` (3) if dispatched) ride in one turn
    /// alongside a valid call. Red-proved by temporarily removing the
    /// partition call site — see PR #2169's description for the mutation
    /// evidence.
    #[test]
    #[serial_test::serial]
    fn malformed_tool_call_names_do_not_advance_the_consecutive_failure_counter() {
        let server = crate::test_support::GuardedMockServer::start();
        let bogus_name = "} catch (error) { --- [TOOL_CALLS]same bogus name every time";
        let mut calls = vec![valid_read_call("call_ok_1", "/workspace/a.txt")];
        for i in 0..3 {
            // Identical name+args each time — the exact signature the
            // cascade detector keys on.
            calls.push(malformed_call(&format!("call_bad_{i}"), bogus_name));
        }
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::Value::Array(calls.clone())),
                "tool_calls",
                100,
                50,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("malformed-no-cascade").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let cascade_fired = raw.lines().any(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap_or_default();
            v.get("type").and_then(|t| t.as_str()) == Some("dispatch.tool.repeated_failure")
        });
        assert!(
            !cascade_fired,
            "3 invalid-name calls sharing a signature must NOT trip the #419 cascade \
             detector — they must never reach FailureRateDetector::record at all"
        );
    }

    /// A turn whose tool_calls are ENTIRELY invalid names must still
    /// progress to the next turn rather than stall — no valid calls to
    /// dispatch, but the loop must still push tool-result messages for
    /// every id, queue the coalesced feedback, and send the next request.
    #[test]
    #[serial_test::serial]
    fn a_turn_of_only_invalid_tool_calls_still_progresses() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = vec![
            malformed_call("call_bad_1", "} catch (error) { --- [TOOL_CALLS]one"),
            malformed_call("call_bad_2", "} catch (error) { --- [TOOL_CALLS]two"),
        ];
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::Value::Array(calls.clone())),
                "tool_calls",
                100,
                50,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("malformed-all-invalid").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("an all-invalid turn must not error");

        assert!(
            matches!(outcome.terminal_reason, TerminalReason::Stop),
            "an all-invalid-name turn must still progress to turn 2's clean stop, not stall: {:?}",
            outcome.terminal_reason
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 2, "the loop must advance past the all-invalid turn");
    }

    // ─── (#2169 merge-gate MUST FIX 1 + 2) real-tool-not-granted bucket ──

    fn ungranted_bash_call(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"echo should-not-run\",\"timeout_seconds\":5}",
            },
        })
    }

    /// (merge-gate MUST FIX 2) Pins the REAL, independent, pre-existing
    /// hole the #2169 partition closes as a side effect: a read-only
    /// role (`tools = [Tool::Read]`, no `Tool::Bash` granted) receiving a
    /// structured `bash` call from the model. `tools::dispatch` matches
    /// PURELY on `Tool::from_name` — pre-#2169 (no partition at all) this
    /// call would have been dispatched and EXECUTED regardless of the
    /// role's granted tool set, because `allowed_tool_names` was, before
    /// this issue, consulted only by the plain-text promoter, never by
    /// the structured path. This test is the regression pin: the call
    /// must never reach `tools::dispatch` (no `tool.completed` event at
    /// all — not even a failed one), and the detector must name the
    /// CORRECT reason (`real_tool_not_granted`, not `not_a_tool` — `bash`
    /// is a perfectly real tool, just not one this dispatch has).
    ///
    /// Red-proved: merging the ungranted bucket back into "granted" (the
    /// shape of the mutation this test exists to catch) makes a
    /// `tool.completed` event for `bash` reappear — see this PR's
    /// description for the mutation transcript.
    #[test]
    #[serial_test::serial]
    fn ungranted_real_tool_call_is_never_dispatched_and_names_the_correct_reason() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = vec![ungranted_bash_call("call_bash_1")];
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::Value::Array(calls.clone())),
                "tool_calls",
                100,
                20,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("ungranted-bash").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read-only task")];
        // The role's ENTIRE granted tool set — deliberately NOT Tool::Bash.
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("an ungranted-tool call must not error the dispatch");
        assert!(matches!(outcome.terminal_reason, TerminalReason::Stop));

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let events: Vec<serde_json::Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        // The regression pin: NO tool.completed event at all — bash was
        // never dispatched, never even attempted and failed.
        let executed = events.iter().filter(|v| v["type"] == "tool.completed").count();
        assert_eq!(
            executed, 0,
            "a real-but-ungranted tool call must NEVER reach tools::dispatch — got {executed} \
             tool.completed event(s), meaning bash actually ran"
        );

        let malformed: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["type"] == "dispatch.tool.malformed_names")
            .collect();
        assert_eq!(malformed.len(), 1);
        assert_eq!(
            malformed[0]["reason"], "real_tool_not_granted",
            "bash is a REAL tool — mislabeling it not_a_tool corrupts the Devstral-pattern metric"
        );
        assert_eq!(malformed[0]["count"], 1);
        assert_eq!(malformed[0]["sample_name_prefix"], "bash");

        // The feedback message must name `bash` specifically (the
        // ungranted-real-tool wording), never the "looks like quoted
        // code / [TOOL_CALLS] marker" wording — that wording is false
        // for a correctly-named real tool and would mislead the model
        // about what it actually got wrong.
        let injected_names_bash = outcome.messages.iter().any(|m| {
            m.role == "system"
                && m.content.as_deref().map(|c| c.contains("bash") && c.contains("not granted")).unwrap_or(false)
        });
        assert!(
            injected_names_bash,
            "the feedback message must name the offending real tool (`bash`) and say it \
             is not granted, not the generic quoted-code/[TOOL_CALLS] wording"
        );
        let injected_wrong_wording = outcome.messages.iter().any(|m| {
            m.role == "system"
                && m.content.as_deref().map(|c| c.contains("[TOOL_CALLS]")).unwrap_or(false)
        });
        assert!(
            !injected_wrong_wording,
            "an ungranted-real-tool call must NOT get the not-a-tool wording"
        );
    }

    /// A turn carrying all THREE kinds at once — one dispatchable call, one
    /// real-but-ungranted call, one not-a-tool call — must produce TWO
    /// SEPARATE detector events (one per reason-bucket), not one muddled
    /// one, and only the one dispatchable call ever reaches tools::dispatch.
    #[test]
    #[serial_test::serial]
    fn mixed_reason_buckets_produce_separate_detector_events() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = vec![
            valid_read_call("call_ok", "/workspace/a.txt"),
            ungranted_bash_call("call_bash"),
            malformed_call("call_garbage", "} catch (error) { --- [TOOL_CALLS]bogus"),
        ];
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::Value::Array(calls.clone())),
                "tool_calls",
                100,
                30,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("mixed-reasons").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let events: Vec<serde_json::Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        assert_eq!(events.iter().filter(|v| v["type"] == "tool.completed").count(), 1);

        let malformed: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["type"] == "dispatch.tool.malformed_names")
            .collect();
        assert_eq!(
            malformed.len(),
            2,
            "one ungranted call + one not-a-tool call in the SAME turn must produce TWO \
             separate events, not one merged bucket: {malformed:?}"
        );
        let reasons: std::collections::BTreeSet<&str> =
            malformed.iter().filter_map(|v| v["reason"].as_str()).collect();
        assert_eq!(
            reasons,
            std::collections::BTreeSet::from(["real_tool_not_granted", "not_a_tool"]),
            "must see exactly one of each reason"
        );
        for m in &malformed {
            assert_eq!(m["count"], 1, "each bucket in this turn has exactly one offender");
        }
    }

    // ─── (#2169 merge-gate MUST FIX 3) composes with #2171/#2176's ────
    // ─── generation-checkpoint-bound salvage ───────────────────────────

    /// (merge-gate MUST FIX 3) The PR description originally claimed #2172
    /// — a SPECIFIC pull request — never merged, therefore there was no
    /// interaction to check. That was checking the wrong thing: #2172 was
    /// superseded/closed, but the FEATURE it targeted (issue #2171, the
    /// generation check-in) merged separately as #2176 and IS on `main`.
    /// `run()` (the test-only convenience wrapper every other #2169 test
    /// in this file uses) hard-codes `generation_checkpoint_interval =
    /// Some(u32::MAX)`, which makes that bound structurally unreachable —
    /// every prior #2169 test is blind to this interaction by
    /// construction. This test goes through `run_with_sleeper` directly
    /// (the same way `generation_bound_salvage_sends_no_reasoning_nudge`
    /// does) with a real, small generation interval, so a turn that hits
    /// BOTH the generation-checkpoint bound (salvage fires) AND carries an
    /// invalid-name call (the #2169 partition fires) on the SAME turn is
    /// actually exercised — proving the partition runs on every source of
    /// a turn's `calls`, not just an organic `finish_reason=tool_calls`
    /// response and #479's per-turn-cap salvage.
    #[test]
    #[serial_test::serial]
    fn generation_bound_salvage_and_malformed_names_compose_on_the_same_turn() {
        let server = crate::test_support::GuardedMockServer::start();
        let calls = serde_json::json!([
            {
                "id": "call_ok",
                "type": "function",
                "function": {
                    "name": "read",
                    "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                },
            },
            {
                "id": "call_bad",
                "type": "function",
                "function": {
                    "name": "} catch (error) { --- [TOOL_CALLS]bogus",
                    "arguments": "{}",
                },
            },
        ]);
        let _turn1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                Some("mid-prose narration that ran right up to the generation cap"),
                Some(calls.clone()),
                "length",
                100,
                // Same cap-1 tolerance shape as
                // `generation_bound_salvage_sends_no_reasoning_nudge` — the
                // LIVE-observed LMStudio behavior (stops one token before
                // the configured cap).
                3999,
            ));
        });
        let _turn2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() > 0
            });
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("gen-salvage-malformed").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "devstral-test", initial, &tools, &mut traj, false, &cfg,
            Some(5), None, Some(10_000), None, Some(4000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a generation-bound-salvaged turn carrying a malformed call must not error");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let events: Vec<serde_json::Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        let salvaged = events.iter().find(|v| v["type"] == "dispatch.per_turn_cap.salvaged");
        assert!(salvaged.is_some(), "the generation-bound salvage must fire on this turn: {events:?}");
        assert_eq!(
            salvaged.unwrap()["bound"]["kind"],
            serde_json::json!("generation_checkpoint_interval"),
            "must be the GENERATION bound that fired, not the raw answer bound"
        );

        let malformed: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["type"] == "dispatch.tool.malformed_names")
            .collect();
        assert_eq!(
            malformed.len(),
            1,
            "the #2169 partition must ALSO run on a generation-bound-salvaged turn: {events:?}"
        );
        assert_eq!(malformed[0]["reason"], "not_a_tool");
        assert_eq!(malformed[0]["count"], 1);

        // Exactly the one well-formed `read` call dispatched — the
        // malformed one never reached tools::dispatch even though the
        // turn was ALSO a salvage.
        assert_eq!(
            events.iter().filter(|v| v["type"] == "tool.completed").count(),
            1
        );
    }

    // ─── (#2169 merge-gate MUST FIX 4) escalation ladder ────────────────

    /// (merge-gate MUST FIX 4) Nothing bounded N consecutive
    /// all-invalid-tool-call turns under default config — max_turns and
    /// max_cumulative_tokens both default `None`, the #419 cascade
    /// detector is warn-only and never sees these calls anyway, and a
    /// `model.partial` heartbeat keeps the HOST watchdog's deadline alive
    /// regardless of whether any of it is productive. This test scripts 3
    /// turns that are ENTIRELY invalid tool calls (no valid dispatch in
    /// any of them) and confirms the loop escalates via
    /// `MalformedToolCallsExhausted` rather than spinning forever.
    #[test]
    #[serial_test::serial]
    fn three_consecutive_all_malformed_turns_escalate() {
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_bad",
                    "type": "function",
                    "function": {
                        "name": "} catch (error) { --- [TOOL_CALLS]bogus",
                        "arguments": "{}",
                    },
                }])),
                "tool_calls",
                100,
                20,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("malformed-escalation").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // High max_turns so the loop would spin well past 3 if the
        // escalation didn't fire — proves this is the escalation
        // terminating it, not MaxTurns.
        let outcome = run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(50),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::MalformedToolCallsExhausted),
            "3 consecutive all-invalid turns must escalate, not spin to MaxTurns: {:?}",
            outcome.terminal_reason
        );
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), MAX_CONSECUTIVE_MALFORMED_TURNS,
            "must escalate at exactly the Kth consecutive all-invalid turn, not before or after"
        );
    }

    /// Sibling of the escalation test above: a turn that dispatches at
    /// least one REAL call resets the counter, so 2 all-invalid turns +
    /// 1 mixed turn + 2 more all-invalid turns must NOT escalate (the
    /// mixed turn's progress resets the streak).
    #[test]
    #[serial_test::serial]
    fn a_mixed_turn_resets_the_consecutive_malformed_counter() {
        let server = crate::test_support::GuardedMockServer::start();
        let bad_call = |id: &str, marker: &str| {
            serde_json::json!([{
                "id": id,
                "type": "function",
                "function": {
                    "name": format!("}} catch (error) {{ --- [TOOL_CALLS]{marker}"),
                    "arguments": "{}",
                },
            }])
        };
        let mixed_call = serde_json::json!([
            valid_read_call("call_ok", "/workspace/a.txt"),
            serde_json::json!({
                "id": "call_bad_mixed",
                "type": "function",
                "function": {
                    "name": "} catch (error) { --- [TOOL_CALLS]bogus_mixed",
                    "arguments": "{}",
                },
            }),
        ]);
        // Matched by the EXACT count of `"role":"tool"` messages already
        // in the request body — the same discriminator
        // `salvage_record_names_the_reasoning_checkpoint_interval_bound`
        // and this file's other multi-turn tests use, since each turn's
        // request carries one more tool-result message than the last.
        //
        // turn1 (0 role:tool): bad-only        → counter 0→1, +1 tool msg (total 1)
        // turn2 (1 role:tool): bad-only        → counter 1→2, +1 tool msg (total 2)
        // turn3 (2 role:tool): MIXED            → counter 2→0 (RESET), +2 tool msgs (total 4)
        // turn4 (4 role:tool): bad-only        → counter 0→1, +1 tool msg (total 5)
        // turn5 (5 role:tool): bad-only        → counter 1→2, +1 tool msg (total 6)
        // Never reaches MAX_CONSECUTIVE_MALFORMED_TURNS (3) — must run to
        // MaxTurns(5), not escalate.
        // httpmock's `.matches()` takes a plain `fn(&HttpMockRequest) ->
        // bool` (a non-capturing function pointer, not a `Fn` closure), so
        // the count literal has to be inline per closure rather than
        // parametrized — same reason this file's other `.matches(|req|
        // { ... .count() == 0 })` sites all inline their own literal.
        let _t1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                None, Some(bad_call("call_bad_1", "a")), "tool_calls", 100, 20,
            ));
        });
        let _t2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1
            });
            then.status(200).json_body(chat_response_json(
                None, Some(bad_call("call_bad_2", "b")), "tool_calls", 100, 20,
            ));
        });
        let _t3 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 2
            });
            then.status(200)
                .json_body(chat_response_json(None, Some(mixed_call.clone()), "tool_calls", 100, 20));
        });
        let _t4 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 4
            });
            then.status(200).json_body(chat_response_json(
                None, Some(bad_call("call_bad_4", "c")), "tool_calls", 100, 20,
            ));
        });
        let _t5 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 5
            });
            then.status(200).json_body(chat_response_json(
                None, Some(bad_call("call_bad_5", "d")), "tool_calls", 100, 20,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("malformed-reset").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("do things")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run(
            &client,
            &client,
            "devstral-test",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "a mixed turn's progress must reset the consecutive-malformed counter, so 5 \
             turns with only one BAD streak of 2 (never reaching 3) must run to MaxTurns \
             rather than escalating: {:?}",
            outcome.terminal_reason
        );
    }

    #[test]
    fn sanitize_sample_name_prefix_caps_length_and_strips_newlines_and_control_bytes() {
        let raw = "line one\nline two\r\nwith a tab\tand a bell\u{7}and \u{e9}accent";
        let out = sanitize_sample_name_prefix(raw);
        assert!(out.chars().count() <= 40, "must cap at 40 chars: {out:?}");
        assert!(!out.contains('\n'), "newlines must be stripped: {out:?}");
        assert!(!out.contains('\r'), "carriage returns must be stripped: {out:?}");
        assert!(out.is_ascii(), "must be pure printable ASCII: {out:?}");
    }

    #[test]
    fn sanitize_sample_name_prefix_passes_ordinary_text_through() {
        assert_eq!(sanitize_sample_name_prefix("hello_world-123"), "hello_world-123");
    }

    /// (#2165, redesigned post-#2164) Post-#2164, a FRESH turn's first
    /// request carries the reasoning bound only once `dispatch_has_reasoned`
    /// is true — never on the dispatch's very first call (see
    /// `carries_reasoning_bound`'s own doc at the cap-selection site). So
    /// this test primes the dispatch with a turn that demonstrates
    /// reasoning (a closed think block, dispatched cleanly via
    /// `finish_reason=tool_calls`, matching the SAME priming pattern
    /// `checkpoint_regression_tests.rs`'s
    /// `a_reasoning_checkpoint_dispatches_tools_without_nudging_the_model_to_think_less`
    /// uses) before the turn under test — turn 2's first request, which now
    /// DOES carry the reasoning bound because `dispatch_has_reasoned` is
    /// true and the turn has absorbed nothing yet. The salvage record's
    /// `bound` must name that, not a bare number a remote reader has to
    /// reverse-engineer from memory of the design (the miss #2165 exists to
    /// close).
    #[test]
    #[serial_test::serial]
    fn salvage_record_names_the_reasoning_checkpoint_interval_bound() {
        let server = crate::test_support::GuardedMockServer::start();
        // Priming turn (0 "role":"tool" in the accumulating request body):
        // demonstrates reasoning via a closed think block, dispatches
        // cleanly (finish_reason=tool_calls, not length) so it does NOT
        // itself salvage or checkpoint — it exists ONLY to flip
        // `dispatch_has_reasoned` true before the scenario under test.
        let _priming = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                Some("<think>brief</think>"),
                Some(serde_json::json!([{
                    "id": "c0",
                    "type": "function",
                    "function": { "name": "echo", "arguments": "{\"text\":\"priming\"}" }
                }])),
                "tool_calls",
                100,
                20,
            ));
        });
        // Turn 2's first request (>=1 "role":"tool" now in history, from
        // the priming turn's own dispatched result): carries the reasoning
        // bound (dispatch_has_reasoned=true, fresh turn) and hits it.
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 1
            });
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
        let tmp = tempfile::Builder::new().prefix("bound-provenance-reasoning").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Echo, Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(4), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("salvage must drive the loop (#479)");
        assert!(crate::trajectory::recorded(tmp.path()).turns() >= 1);

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let salvaged: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("dispatch.per_turn_cap.salvaged"))
            .collect();
        assert!(!salvaged.is_empty(), "salvage record must exist — got trajectory:\n{raw}");
        let bound = &salvaged[0]["bound"];
        assert_eq!(
            bound["kind"], serde_json::json!("reasoning_checkpoint_interval"),
            "turn 2's first request, after a priming turn proved reasoning, is governed by \
             the reasoning check-in interval, got {bound:?}"
        );
        assert_eq!(bound["source"], serde_json::json!("built-in"), "no CLI source flag was set for this test");
    }

    /// (#2165, post-#2164) Post-#2164, the DEFAULT case is the answer bound
    /// on a fresh turn's first request — no priming, no region flip needed.
    /// This is now the simpler counterpart to the reasoning test above (the
    /// two tests' shapes have effectively swapped relative to pre-#2164):
    /// the dispatch's very first call, with `dispatch_has_reasoned` still
    /// false, carries `max_tokens_per_call`, not the reasoning interval —
    /// mirroring `loop_salvages_tool_call_on_per_turn_cap_hit` above.
    #[test]
    #[serial_test::serial]
    fn salvage_record_names_the_max_tokens_per_call_bound_once_in_answer_region() {
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial truncated content"),
                Some(serde_json::json!([{
                    "id": "call_2",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":50}",
                    },
                }])),
                "length",
                140,
                MAX_TOKENS_PER_CALL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("bound-provenance-answer").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(4), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("salvage must drive the loop (#479)");
        assert!(crate::trajectory::recorded(tmp.path()).turns() >= 1);

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let salvaged: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("dispatch.per_turn_cap.salvaged"))
            .collect();
        assert!(!salvaged.is_empty(), "salvage record must exist — got trajectory:\n{raw}");
        let bound = &salvaged[0]["bound"];
        assert_eq!(
            bound["kind"], serde_json::json!("max_tokens_per_call"),
            "the dispatch's very first call, before dispatch_has_reasoned is ever true, \
             carries the answer bound, got {bound:?}"
        );
        assert_eq!(bound["source"], serde_json::json!("built-in"), "no CLI source flag was set for this test");
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
        let server = crate::test_support::GuardedMockServer::start();
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
        run(
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
        assert!(crate::trajectory::recorded(tmp.path()).turns() >= 1);
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
    /// discarding every prior productive turn — the #1222 shakedown's
    /// failure mode). It routes through the stall recovery: drop + nudge +
    /// bounded budget, ending in a clean EscalationTriggered outcome when
    /// the mock repeats the shape past the budget.
    #[test]
    #[serial_test::serial]
    fn cap_cliff_partial_content_recovers_instead_of_killing_the_dispatch() {
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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

    /// (#2836) #479's salvage is gated on the cut having been OURS, and
    /// that half of the condition had never been tested. Deleting it left
    /// 742/742 green — found by mutation while re-expressing the predicate
    /// over `CutSource`.
    ///
    /// The gap matters because `finish_reason: "length"` is ambiguous on
    /// the wire. It is what the server says when our own `max_tokens`
    /// stopped generation AND when the prompt crossed the model's loaded
    /// context window. Only the token count separates them. Without the
    /// gate, an overflow that happens to carry a syntactically complete
    /// tool call gets DISPATCHED — the runtime acts on a turn the model
    /// never finished thinking, and the operator sees a tool run instead of
    /// the diagnosis telling them their context window is too small.
    ///
    /// Well-formed arguments are the point of the fixture: they are what
    /// makes the JSON check pass, so the cap comparison is the only thing
    /// left standing between an overflow and a dispatch.
    #[test]
    #[serial_test::serial]
    fn a_below_cap_length_is_an_overflow_even_when_its_tool_call_parses() {
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("partial content"),
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\"}"
                    },
                }])),
                "length",
                100,
                4000,
            ));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("overflow-with-call").tempdir().unwrap();
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
        .expect_err(
            "a length finish 6,000 tokens below our own cap is the context window, \
             not us — a parseable tool call inside it does not change whose cut it was",
        );
        assert!(
            err.to_string().contains("context overflow"),
            "the error must name context overflow, got: {err:#}"
        );
        let traj_text =
            std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"))
                .unwrap();
        assert!(
            !traj_text.contains("tool.started"),
            "the salvage must not have dispatched anything; trajectory was:\n{traj_text}"
        );
    }

    /// Salvage must still dispatch the tool call even when feedback
    /// injection is disabled. The nudge is a no-op but the salvage
    /// path itself stays active — separating queueing from routing.
    #[test]
    #[serial_test::serial]
    fn loop_salvages_tool_call_even_when_feedback_injection_disabled() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "0");
        let server = crate::test_support::GuardedMockServer::start();
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
        run(
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
            crate::trajectory::recorded(tmp.path()).turns() >= 1,
            "salvage must still drive the loop when feedback is disabled (got turns={})",
            crate::trajectory::recorded(tmp.path()).turns()
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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
        run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
            .expect("recovered call from length-truncated response should drive the loop");

        assert!(
            crate::trajectory::recorded(tmp.path()).turns() > 1,
            "promotion must continue the loop even when finish_reason=length; got turns={}",
            crate::trajectory::recorded(tmp.path()).turns()
        );
    }

    /// (#406) Reasoning-channel variant of the bail scenario: the XML
    /// tool call lands in `reasoning_content` rather than `content`
    /// (the Qwen 3.x thinking-mode case from V4 N=5 Run 2). The
    /// promoter must fall back from content to reasoning_content.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_in_reasoning_when_finish_reason_is_stop() {
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns() > 1,
            "reasoning-channel promotion must keep the loop alive past turn 1; got turns={}",
            crate::trajectory::recorded(tmp.path()).turns()
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::MaxTurns);
    }

    // The `assert_every_mock_was_hit` per-test opt-in helper that used to
    // live here (#2541) is retired (#2599): its one remaining caller,
    // `assistant_messages_in_history_never_carry_reasoning_content` below,
    // already registers its mocks on a `GuardedMockServer`, whose own
    // Drop-time check subsumes exactly what this helper asserted — every
    // registered `.mock(...)` was hit at least once — with no separate
    // call and no list of labels to keep in sync. The sharper half of its
    // doc comment (a hits-based check cannot distinguish a legitimately
    // false predicate from one that was silently shadowed and never even
    // evaluated) moved to `GuardedMockServer::mock_expect_zero`'s own doc
    // in `test_support.rs`, where the exemption it warns about is actually
    // granted.

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
        let server = crate::test_support::GuardedMockServer::start();
        // (#1444 review) Registration ORDER is load-bearing, and this test
        // had it backwards. httpmock returns the FIRST-REGISTERED mock whose
        // predicate matches, not the most specific one — and `turn1`'s
        // predicate (`body_contains("\"role\":\"user\"")`) stays true
        // forever, because the original user message never leaves the
        // growing conversation. Registered first, `turn1` therefore answered
        // EVERY request: the loop hammered turn one's tool call 100 times,
        // tripped the cycle detector, exited on max-turns, and still passed
        // — under an `.expect("clean two-turn dispatch")` that was false,
        // for ~2.8s of suite time. The guard itself is load-bearing
        // (deleting the post-promoter reasoning clear does red this test),
        // so the fix is to route it correctly, not to delete it.
        //
        // Registering the MORE SPECIFIC mock first gives it the lower id:
        // on request 1 `turn2`'s predicate is false (no tool result exists
        // yet) so it falls through to `turn1`; from request 2 on, `turn2`
        // matches and wins. Same ordering as
        // `loop_accumulates_reasoning_and_cached_tokens_across_turns_tri_state`.
        // Two recorded turns pin the routing so it cannot
        // silently regress again — and `GuardedMockServer`'s own Drop-time
        // check (#2599) now independently requires both `turn1` and
        // `turn2` below to have been hit at least once.
        //
        // (#2541 full audit, instrumented via `Mock::hits()` across every
        // runtime test registering 2+ mocks, run whole-suite: this was
        // the ONLY test where a registered mock a run was expected to
        // reach went unserved.
        //
        // (#2599 round 3 review) Don't trust a specific test/registration
        // count for this population — it doesn't reproduce. Three
        // independent counting passes gave three different totals (an
        // initial manual audit landed on 44 tests / 105 registrations;
        // a later runtime-instrumented count gave 42 / 100; a separate
        // source-level parse gave 43 / 106), and the gaps trace to the
        // population DEFINITION doing the work rather than to counting
        // error: whether a helper's registrations attribute to every
        // caller (`register_three_turn_tool_then_stop_script` registers 3
        // mocks with no registration site of its own and is called by 3
        // tests), whether two same-named tests in different modules
        // collapse under a name-based count, and whether a test that
        // registers one mock per iteration of a loop counts at all
        // (shadowing needs 2+ DISTINCT mocks on the same server). Rather
        // than pick a fourth number, this comment describes what was
        // audited instead of asserting a population size: every runtime
        // test that registers 2+ mocks was checked via `Mock::hits()`
        // across a whole-suite run, and (see the exemption count below)
        // every found-unhit mock is now either fixed (this test) or
        // deliberately exempted via `mock_expect_zero`.
        //
        // Two counts DO reproduce exactly and are safe to rely on. First:
        // 94 static `GuardedMockServer::start()` call sites outside
        // `test_support.rs` (confirmed by
        // `grep -c 'let .*= .*GuardedMockServer::start();'` across
        // `checkpoint_regression_tests.rs`, `compaction.rs`, and this
        // file). Second: a RUNTIME-instrumented count of every
        // `GuardedMockServer::start()` call across the whole crate comes
        // out exactly one higher than those 94 static sites, because
        // `max_stall_recoveries_override_changes_the_escalation_point`
        // constructs its server inside a 2-iteration `for` loop — one
        // static site, two servers at run time. That test registers only
        // ONE mock per iteration, so — per the DEFINITION problem above —
        // it is not part of any 2+-mock shadowing-audit population
        // regardless of which count of that population you trust; it is
        // exactly the test responsible for the crate-wide static-vs-
        // runtime construction-count gap.
        //
        // The 0-hit mocks found (9 total, across 8 tests) are all
        // deliberate, and every one is now registered through
        // `GuardedMockServer::mock_expect_zero` with a written reason
        // (#2599) rather than left to a hits-only check: an explicit
        // `Mock::assert_hits(0)` proving a resumed/mid-turn dispatch never
        // re-requests a call it already has, a "never actually serve —
        // this mock only observes" detector whose predicate always
        // returns false but does its real work as a side-channel counter
        // (see `GuardedMockServer::mock_expect_zero`'s own doc in
        // `test_support.rs` for why a hits-only check cannot safely
        // exempt this shape on its own), or one arm of a pair of
        // genuinely mutually-exclusive predicates whose other branch this
        // particular scripted run doesn't take. This 9-total breakdown
        // DOES reproduce: one uses a
        // `body_contains("\"model\":\"test-primary\"")` /
        // `\"test-compactor\"` discriminator, three use tool-role count
        // matchers, three use disjoint content-sentinel predicates, one
        // uses a token-limit predicate, and one is
        // `checkpoint_regression_tests::
        // a_salvage_after_a_checkpoint_never_leaves_two_assistant_messages_
        // adjacent`'s observe-only `_detector` — 1 + 3 + 3 + 1 + 1 = 9.
        // None of the 9 were found shadowed. Noted, not fixed: at least
        // one of these mutually-exclusive-arm exemptions
        // (`checkpoint_regression_tests::
        // an_answer_after_the_models_own_think_close_is_delivered`'s
        // `_m3`, and arguably `an_empty_tool_calls_turn_does_not_delete_
        // the_accumulation`'s `_m3` alongside it) belongs to a run that
        // escalates after turn two, so a scripted third turn in that test
        // is never exercised — coverage its name implies but does not
        // reach.)
        //
        // Second call (after the tool result): model finishes with stop.
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

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reasoning-invariant").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
            .expect("clean two-turn dispatch");

        // (#1444 review) Pins the mock ROUTING the ordering above fixes.
        // Without it the loop can silently fall back to answering every
        // request from `turn1`, run to the 100-turn cap, and still satisfy
        // every assertion below — which is exactly what it did.
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 2,
            "turn 1 must be answered by the tool_calls mock and turn 2 by the stop mock; \
             a higher count means turn1's mock is shadowing turn2's again"
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);

        // Both `turn1` and `turn2` are real (`.mock`, not
        // `.mock_expect_zero`) registrations on a `GuardedMockServer` —
        // its own Drop-time check already requires every one of them to
        // have been hit at least once (#2599), so a separate
        // `assert_every_mock_was_hit(&[...])` call here would be pure
        // duplication of what teardown already proves; that per-test
        // opt-in helper was retired once this, its one remaining caller,
        // no longer needed it.

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
        let server = crate::test_support::GuardedMockServer::start();
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
        run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(10_000), std::collections::BTreeMap::new(), None)
            .expect("clean single-turn dispatch");

        captured.assert();
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1);
    }

    /// (#2164) `max_tokens_per_call` (the ANSWER bound) still bounds the
    /// answer region — this fix does not widen or remove that cap, it only
    /// stops a fresh turn's first call from carrying the SMALLER reasoning
    /// check-in interval instead. `reasoning_checkpoint_interval` is set to
    /// a deliberately DIFFERENT, much smaller value (100) than
    /// `max_tokens_per_call` (3000) so this test can only pass if the
    /// request actually carried the answer bound — a regression back to
    /// "every fresh turn's first call carries the reasoning bound" would
    /// send `max_tokens: 100` and this mock (keyed on 3000) would never
    /// match, failing the dispatch outright.
    #[test]
    #[serial_test::serial]
    fn max_tokens_per_call_bounds_the_answer_region_on_a_fresh_turns_first_call() {
        let server = crate::test_support::GuardedMockServer::start();
        let captured = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"max_tokens\":3000");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("answer-bound-first-call").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("hi")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(3000), Some(100), std::collections::BTreeMap::new(), None,
        )
        .expect("a fresh turn's first call, sent under the answer bound, dispatches cleanly");

        captured.assert();
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1);
    }

    /// Smoke: mock returns finish_reason=stop on first call. Loop
    /// terminates cleanly, no compaction. Proves the mock + LmStudioClient
    /// + loop_runner integration plumbing works.
    #[test]
    #[serial_test::serial]
    fn loop_runs_against_mock_and_terminates_on_stop() {
        let server = crate::test_support::GuardedMockServer::start();
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
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1);
        assert_eq!(crate::trajectory::recorded(tmp.path()).compactions(), 0);
        assert_eq!(crate::trajectory::recorded(tmp.path()).tokens.prompt, 1234);
        // (#1444) `chat_response_json` never emits `completion_tokens_details`/
        // `prompt_tokens_details` — the LMStudio-local shape. Absent, not zero.
        assert_eq!(crate::trajectory::recorded(tmp.path()).tokens.reasoning, None);
        assert_eq!(crate::trajectory::recorded(tmp.path()).tokens.cached, None);
        // #325: pin the Stop terminal_reason on this clean-exit path.
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// (#1444) Two turns: turn 1 (tool_calls) reports BOTH reasoning_tokens
    /// and cached_tokens; turn 2 (stop) reports reasoning_tokens only — its
    /// `usage` carries no `prompt_tokens_details` at all. The final totals
    /// must SUM what was reported (500+300=800 reasoning) while
    /// `total_cached_tokens` reflects ONLY turn 1's report (20), never
    /// reset to `None` or zeroed by turn 2's silence on the field — the
    /// tri-state "a turn that omits it doesn't corrupt what's already been
    /// seen" contract `total_reasoning_tokens`'s own doc names.
    #[test]
    fn loop_accumulates_reasoning_and_cached_tokens_across_turns_tri_state() {
        let server = crate::test_support::GuardedMockServer::start();
        // (#1444 test-infra finding) httpmock's `find_mock` returns the
        // FIRST-registered mock (ascending internal id) whose predicate is
        // satisfied — NOT the most specific one. `turn1`'s predicate
        // (`body_contains("\"role\":\"user\"")`) stays true forever (the
        // original user message never leaves the growing conversation), so
        // it would shadow `turn2` on every later request if registered
        // first. Registering the MORE SPECIFIC mock (`turn2`, matched only
        // once a tool result exists) FIRST — so it gets the lower id and is
        // checked first — makes routing correct: on request 1 `turn2`'s
        // predicate is false (no tool result yet) so it falls through to
        // `turn1`; from request 2 onward `turn2` matches and wins.
        let turn2 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"tool\"");
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-2",
                "object": "chat.completion",
                "created": 1700000001,
                "model": "ignored-by-test",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "done" },
                    "finish_reason": "stop",
                }],
                "usage": {
                    "prompt_tokens": 200,
                    "completion_tokens": 350,
                    "total_tokens": 550,
                    "completion_tokens_details": { "reasoning_tokens": 300 },
                    // No prompt_tokens_details at all this turn — must NOT
                    // reset total_cached_tokens back to None.
                },
            }));
        });
        let turn1 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"user\"");
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "ignored-by-test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\"}" },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 600,
                    "total_tokens": 700,
                    "completion_tokens_details": { "reasoning_tokens": 500 },
                    "prompt_tokens_details": { "cached_tokens": 20 },
                },
            }));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reasoning-tokens-accum").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![
            Message::system("you are a test assistant"),
            Message::user("read x.txt"),
        ];
        let tools = [Tool::Read, Tool::Edit, Tool::Bash];

        let cfg = compaction::CompactionConfig::never_compact();
        run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
            .expect("two-turn loop should terminate cleanly on turn 2's stop");

        turn1.assert();
        turn2.assert();
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 2);
        assert_eq!(crate::trajectory::recorded(tmp.path()).tokens.completion, 950, "600 + 350 — reasoning_tokens is INCLUDED, not additional");
        assert_eq!(crate::trajectory::recorded(tmp.path()).tokens.reasoning, Some(800), "500 + 300 summed across both turns");
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).tokens.cached,
            Some(20),
            "only turn 1 reported cached_tokens; turn 2's silence must not reset or corrupt it"
        );
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
    ///   - one recorded compaction
    ///   - compactor mock was hit exactly once
    ///   - primary mock was hit at least twice (before + after compaction)
    #[test]
    fn loop_triggers_compaction_when_threshold_crossed() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 1000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = crate::test_support::GuardedMockServer::start();

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
        if outcome.is_ok() {
            assert!(
                crate::trajectory::recorded(tmp.path()).compactions() >= 1,
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
            compactor_context_window: None,
            threshold_tokens: 1000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let primary_server = crate::test_support::GuardedMockServer::start();
        let compactor_server = crate::test_support::GuardedMockServer::start();

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
            compactor_context_window: None,
            threshold_tokens: 5000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = crate::test_support::GuardedMockServer::start();
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

        // (3) The estimate is a real measurement of the thread AT THE
        // CROSSING, not a stand-in constant.
        //
        // (#2792) This assertion used to require `estimate >= 5000`, i.e. that
        // the estimate at the staleness crossing was itself above the
        // threshold — because under the old design the stale path was the ONLY
        // way the estimate reached the compaction decision, so the first
        // compaction could not happen until the crossing. That is no longer
        // the mechanism: the estimate is now consulted every turn, so this
        // thread is ALREADY compacted by the time turn 3 crosses, and the
        // measured estimate is correspondingly smaller (observed: 2497).
        //
        // The bound is dropped rather than lowered to a number that happens to
        // pass today. What #854 actually claims is asserted above and still
        // holds, more strongly than before: assertion (1) — a frozen
        // below-threshold count does not suppress compaction — now passes
        // because occupancy is measured unconditionally, not because a
        // staleness heuristic rescued it. This event is now a DIAGNOSTIC that
        // the endpoint is misreporting, which is what its message says.
        let estimate = stale_events[0]["estimate"].as_u64().unwrap();
        let reported_messages = stale_events[0]["message_count"]
            .as_u64()
            .or_else(|| stale_events[0]["messages"].as_u64())
            .expect("the event must report the thread size it measured");
        // (#2792 merge-gate) `estimate > 0` was vacuous — a hardcoded `1`
        // passed it while its own message forbade placeholders. Two bounds
        // that are properties of a REAL measurement, without re-encoding the
        // old `>= 5000` mechanism:
        //
        // 1. Every message carries content, so a genuine chars/4 measure of
        //    N messages exceeds N.
        // 2. This fixture's padded messages are ~630 chars each and the
        //    preserved head + tail alone is 6 of them, so any honest
        //    measurement here is far above 250 tokens (~1,000 chars). A
        //    placeholder small enough to be convenient fails this; a real
        //    measurement cannot.
        assert!(
            estimate > reported_messages,
            "a chars/4 measure of {reported_messages} messages must exceed the \
             message count itself: got {estimate}"
        );
        assert!(
            estimate >= 250,
            "the event must carry a real measurement of this fixture's padded \
             thread, not a placeholder: got {estimate}"
        );
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
            compactor_context_window: None,
            threshold_tokens: 1000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };

        let server = crate::test_support::GuardedMockServer::start();

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
            crate::trajectory::recorded(tmp.path()).compactions(), 1,
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
            compactor_context_window: None,
            threshold_tokens: 1000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = crate::test_support::GuardedMockServer::start();
        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    // `echo`, not `read`: a run that re-reads the same files
                    // after every compaction is bounded on purpose (#3013),
                    // and this test is about the compaction COUNT bound only.
                    "function": {
                        "name": "echo",
                        "arguments": "{\"text\":\"still working\"}",
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
        let tools = [Tool::Echo];

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
            crate::trajectory::recorded(tmp.path()).compactions() >= 1,
            "compaction still fires; bail just doesn't kick in"
        );
    }

    /// The config both #3013 tests use: every compaction is PRODUCTIVE (a
    /// short summary leaves the thread far under the trigger), so the
    /// unproductive-compaction counter never grows.
    fn productive_compaction_cfg() -> compaction::CompactionConfig {
        compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 5_000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        }
    }

    /// Over the 200-char floor a compaction summary must clear to install,
    /// and far under the trigger once installed.
    const SHORT_SUMMARY: &str = "Summary of prior work: the four implementation files were read and understood. Their contents are captured in the plan above. Do not read them again; make the edits the plan calls for and run the tests to finish the task.";

    fn read_call_json(path: &str) -> serde_json::Value {
        serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "read",
                "arguments": format!("{{\"path\":\"{path}\",\"offset\":1,\"limit\":0}}"),
            },
        }])
    }

    fn seeded_thread() -> Vec<Message> {
        let filler = "seed content ".repeat(80);
        (0..10)
            .map(|i| {
                if i == 0 {
                    Message::system("test system")
                } else {
                    Message::user(format!("turn {i}: {filler}"))
                }
            })
            .collect()
    }

    /// (#3013) Promise: a run whose post-compaction turns repeat the
    /// pre-compaction work is bounded and escalated. The live shape: read the
    /// same files, compact (productively, so #2807 never counts it), read the
    /// same files again, forever. `max_turns` is 40; only the re-read
    /// detector can end this run before it.
    #[test]
    #[serial_test::serial]
    fn a_run_that_rereads_the_same_files_after_every_productive_compaction_escalates() {
        let server = crate::test_support::GuardedMockServer::start();
        let four_reads = serde_json::json!(["a", "b", "c", "d"].map(|f| serde_json::json!({
            "id": format!("call_{f}"),
            "type": "function",
            "function": {"name": "read", "arguments": format!("{{\"path\":\"/workspace/{f}.txt\",\"offset\":1,\"limit\":0}}")},
        })));
        let _primary = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(None, Some(four_reads.clone()), "tool_calls", 9_000, 50));
        });
        let compactor = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(Some(SHORT_SUMMARY), None, "stop", 500, 30));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reread-loop").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run(
            &client, &client, "test-primary", seeded_thread(), &[Tool::Read], &mut traj, false,
            &productive_compaction_cfg(), Some(40), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("escalation is a graceful terminal, not an error");
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionRereadLoop),
        );
        let recorded = crate::trajectory::recorded(tmp.path());
        assert!(recorded.turns() <= 8, "bounded near the threshold, not max_turns: turns={}", recorded.turns());
        assert!(compactor.hits() >= 5, "every compaction ran: {}", compactor.hits());
        assert!(recorded.compactions() >= 5, "and INSTALLED: {}", recorded.compactions());
        let raw = std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        assert!(raw.contains("escalation_compaction_reread_loop"), "the reason is on the record");
        assert!(!raw.contains("compaction.unproductive"), "each compaction was productive");
    }

    /// (#3013) The inverse: a run that compacts every turn but reads a NEW
    /// file each time is making progress, and must not be escalated.
    #[test]
    #[serial_test::serial]
    fn a_run_that_reads_new_files_after_each_compaction_is_not_escalated() {
        let server = crate::test_support::GuardedMockServer::start();
        // Turn i's request carries turn i-1's call in the preserved tail, so
        // "mentions file i" picks the next file. Registered highest first:
        // a body naming several files matches the newest one.
        const FILES: usize = 9;
        let mut mocks = Vec::new();
        for i in (0..FILES).rev() {
            let (needle, reply) = if i == 0 {
                (String::new(), read_call_json("/workspace/f0.txt"))
            } else {
                (format!("/workspace/f{}.txt", i - 1), read_call_json(&format!("/workspace/f{i}.txt")))
            };
            let last = i == FILES - 1;
            mocks.push(server.mock(move |when, then| {
                let w = when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-primary\"");
                if !needle.is_empty() {
                    w.body_contains(&needle);
                }
                if last {
                    then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 9_000, 5));
                } else {
                    then.status(200).json_body(chat_response_json(None, Some(reply.clone()), "tool_calls", 9_000, 50));
                }
            }));
        }
        let compactor = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(Some(SHORT_SUMMARY), None, "stop", 500, 30));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reread-inverse").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run(
            &client, &client, "test-primary", seeded_thread(), &[Tool::Read], &mut traj, false,
            &productive_compaction_cfg(), Some(40), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the run completes");
        let installed = crate::trajectory::recorded(tmp.path()).compactions();
        assert!(installed >= 6, "the scenario must INSTALL compactions repeatedly: {installed} (compactor hits {})", compactor.hits());
        assert_ne!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionRereadLoop),
            "new files after each compaction are progress"
        );
    }

    // ---- (B4) loop-level pins for the model-facing nudges, the silent arm and the episode reset ----
    //
    // Each of these was proven by mutation to leave the whole suite green when
    // its production line was deleted: the detectors' trajectory events were
    // asserted, but nothing checked that the nudge reached the NEXT REQUEST,
    // which is the point of the signal.

    /// Serve a mock that answers `done` (a clean stop) to any request whose
    /// body carries `nudge`, and registers the looping `then` reply after it.
    /// Registered first, so it wins whenever the nudge is present: the run
    /// only ends `Stop` if the nudge reached a request.
    fn stop_when_request_carries(server: &crate::test_support::GuardedMockServer, nudge: &'static str, looping: serde_json::Value) {
        let _nudge_mock = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-primary\"").body_contains(nudge);
            then.status(200).json_body(chat_response_json(Some("done"), None, "stop", 100, 5));
        });
        let _looping_mock = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(looping.clone());
        });
    }

    fn assert_nudge_reached_the_request(outcome: &LoopOutcome, kind: &str, tmp: &std::path::Path) {
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::Stop,
            "the run ends only if a request carried the nudge; MaxTurns means it never did"
        );
        let raw = std::fs::read_to_string(tmp.join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        assert!(
            raw.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v["type"] == "dispatch.feedback.injected")
                .any(|v| v["signal_kinds"].as_array().is_some_and(|k| k.iter().any(|x| x == kind))),
            "and the injection is recorded under `{kind}`"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_tool_failure_cascade_nudge_reaches_the_next_request() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let server = crate::test_support::GuardedMockServer::start();
        stop_when_request_carries(
            &server,
            "the tool or its environment failing",
            chat_response_json(None, Some(read_call_json("/workspace/no-such-file.txt")), "tool_calls", 100, 10),
        );
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("nudge-cascade").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run(
            &client, &client, "test-primary", vec![Message::system("s"), Message::user("go")], &[Tool::Read], &mut traj,
            false, &compaction::CompactionConfig::never_compact(), Some(12), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the run completes");
        assert_nudge_reached_the_request(&outcome, "tool_failure_cascade", tmp.path());
    }

    #[test]
    #[serial_test::serial]
    fn a_reasoning_loop_nudge_reaches_the_next_request() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let server = crate::test_support::GuardedMockServer::start();
        let same_thought = "<think>I should read the file again to be sure what it says before I do anything else at all here.</think>";
        stop_when_request_carries(
            &server,
            "revisited the same line of reasoning",
            chat_response_json(
                Some(same_thought),
                Some(serde_json::json!([{
                    "id": "call_r", "type": "function",
                    "function": {"name": "echo", "arguments": "{\"text\":\"again\"}"},
                }])),
                "tool_calls",
                100,
                10,
            ),
        );
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("nudge-reasoning").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run(
            &client, &client, "test-primary", vec![Message::system("s"), Message::user("go")], &[Tool::Echo], &mut traj,
            false, &compaction::CompactionConfig::never_compact(), Some(12), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the run completes");
        assert_nudge_reached_the_request(&outcome, "reasoning_loop", tmp.path());
    }

    #[test]
    #[serial_test::serial]
    fn a_post_compaction_nudge_reaches_the_next_request() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let server = crate::test_support::GuardedMockServer::start();
        let _compactor = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(Some(SHORT_SUMMARY), None, "stop", 500, 30));
        });
        // `echo`, not `read`: a re-read after every compaction is bounded on
        // purpose (#3013) and would end the run first.
        stop_when_request_carries(
            &server,
            "Working memory was just compressed",
            chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_e", "type": "function",
                    "function": {"name": "echo", "arguments": "{\"text\":\"working\"}"},
                }])),
                "tool_calls",
                9_000,
                50,
            ),
        );
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("nudge-compaction").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run(
            &client, &client, "test-primary", seeded_thread(), &[Tool::Echo], &mut traj,
            false, &productive_compaction_cfg(), Some(12), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("the run completes");
        assert_nudge_reached_the_request(&outcome, "post_compaction", tmp.path());
    }

    /// A hand-rolled SSE server: the FIRST connection sends one chunk then
    /// goes silent past the client's read timeout; every later connection
    /// answers with a clean `done`. httpmock cannot express "bytes stop".
    fn silent_then_clean_server() -> String {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (n, conn) in listener.incoming().enumerate() {
                let Ok(mut sock) = conn else { return };
                std::thread::spawn(move || {
                    let mut head = std::io::BufReader::new(sock.try_clone().unwrap());
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if head.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    let _ = head.read_exact(&mut body);
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
                    let frame = |sock: &mut std::net::TcpStream, payload: String| {
                        let _ = sock.write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes());
                        let _ = sock.flush();
                    };
                    if n == 0 {
                        frame(&mut sock, "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>working through the problem step by step\"}}]}\n\n".to_string());
                        std::thread::sleep(std::time::Duration::from_secs(4));
                    } else {
                        frame(&mut sock, "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n".to_string());
                        frame(&mut sock, "data: [DONE]\n\n".to_string());
                        let _ = sock.write_all(b"0\r\n\r\n");
                    }
                });
            }
        });
        format!("http://{addr}/v1")
    }

    /// (B4) An endpoint that goes silent mid-stream ends THAT CALL, not the
    /// dispatch: everything it produced is handed back and the run continues.
    /// Deleting the silent arm makes this an `Err` (and loses every banked
    /// checkpoint of a long turn).
    #[test]
    #[serial_test::serial]
    fn a_silent_stream_hands_back_what_it_produced_instead_of_failing_the_dispatch() {
        let url = silent_then_clean_server();
        let client = LmStudioClient::with_base_url_and_read_timeout(url, std::time::Duration::from_millis(400));
        let tmp = tempfile::Builder::new().prefix("silent-arm").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let outcome = run_with_sleeper(
            &client, &client, "m", vec![Message::system("s"), Message::user("go")], &[Tool::Read], &mut traj, true,
            &compaction::CompactionConfig::never_compact(), Some(6), None, Some(9_000), Some(1_000), Some(1_000),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a silent endpoint must not fail the dispatch");
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop, "the run carried on after the silent call");
        let raw = std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        let first_completed = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "model.completed")
            .expect("the silent call is recorded");
        assert_eq!(first_completed["finish_reason"], "length", "a cut call closes as length");
        assert!(first_completed["usage"].is_null(), "no usage arrived: {first_completed}");
        assert!(first_completed["completion_estimate"].as_u64().unwrap_or(0) > 0, "its tokens are still counted: {first_completed}");
    }

    /// (#2114 finding 1) Resume-compaction parity: a checkpoint whose
    /// `compactions` already sits at `bail_after_compactions - 1` must
    /// escalate the INSTANT the resume catch-up pass's own compaction
    /// check pushes it over the bound — the same as a live (never-killed)
    /// dispatch would at the identical count. Before this fix, the resume
    /// catch-up's compaction path didn't run the `bail_after_compactions`
    /// check at all, so it would silently issue one MORE request past the
    /// operator's bound instead of escalating to the frontier.
    /// (#2793) Compaction that runs every turn and never gets below its own
    /// trigger is a distinct, nameable state, and it must be named.
    ///
    /// Measured on the dogfood run this issue came from: 44 compactions
    /// across 50 turns, 1,047,519 prompt tokens against 10,327 completion —
    /// every turn paying a compactor dispatch, none of them buying a turn
    /// without one. Each individual compaction looked successful, which is
    /// why it was invisible from the compaction records.
    ///
    /// Loop-grain by design: the previous two merge-gate rounds both proved
    /// that unit tests on the pieces leave the wiring unpinned.
    #[test]
    #[serial_test::serial]
    fn compaction_that_never_gets_below_its_own_trigger_is_reported_once() {
        // Trigger well ABOVE anything a compaction of this thread can reach,
        // so every compaction is "successful" and still unproductive.
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 1,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        let pad = "tool output ".repeat(200);
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
                9_000,
                50,
            ));
        });
        let _compactor = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some(&format!("Summary of prior work. {pad}")),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("unproductive").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let filler = "seed content ".repeat(80);
        let initial: Vec<Message> = (0..10)
            .map(|i| {
                if i == 0 {
                    Message::system("test system")
                } else {
                    Message::user(format!("turn {i}: {filler}"))
                }
            })
            .collect();
        let tools = [Tool::Read];

        let outcome = run(
            &client, &client, "test-primary", initial, &tools, &mut traj, false,
            &cfg, Some(12), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("escalation is a graceful terminal, not an error");

        // (#2805) The state is now ESCALATED, not merely reported. #2793
        // detected it and let the run continue; measured twice, that meant 50
        // turns / 1.05M prompt tokens and then 124 / 2.25M, neither
        // converging, both stopped by hand. `max_turns` here is 12 — if the
        // loop ran to that bound instead of escalating at 5, this assertion
        // is what catches it.
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionUnproductive),
            "an unproductive-compaction episode must hand off to the frontier, \
             not keep burning turns"
        );
        assert!(
            crate::trajectory::recorded(tmp.path()).turns() < 12,
            "it must escalate BEFORE max_turns, else it is not bounding anything: \
             turns={}",
            crate::trajectory::recorded(tmp.path()).turns()
        );

        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .expect("trajectory must exist");
        let events: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let unproductive: Vec<_> = events
            .iter()
            .filter(|v| v["type"] == "compaction.unproductive")
            .collect();
        let compactions = events.iter().filter(|v| v["type"] == "compaction").count();

        assert!(
            compactions >= UNPRODUCTIVE_COMPACTION_TURNS as usize,
            "the scenario must actually compact repeatedly, else it pins nothing: \
             {compactions} compactions"
        );
        assert_eq!(
            unproductive.len(),
            1,
            "an episode is reported ONCE, not per turn — a per-turn line would \
             bury the run it is describing. got {} events",
            unproductive.len()
        );
        let ev = unproductive[0];
        assert!(
            ev["tokens_after"].as_u64().unwrap() >= ev["trigger_tokens"].as_u64().unwrap(),
            "the reported state IS 'still at or above the trigger': {ev}"
        );
        assert_eq!(
            ev["consecutive"].as_u64().unwrap(),
            UNPRODUCTIVE_COMPACTION_TURNS as u64,
            "it fires at the threshold, not later"
        );
    }

    /// (#2792 round-3) The RESUME catch-up site must skip a refused
    /// compaction, not kill the dispatch — and must not count it.
    ///
    /// Round 2 converted this site and round 3 proved the conversion was
    /// pinned by NOTHING: restoring the fatal `?` OR the count-attempts
    /// increment here both left the full suite green. This is the worse of
    /// the two sites to leave unguarded, because a resume begins from a
    /// checkpoint whose thread is already large — exactly where a middle too
    /// small to compact meets a thread big enough to trigger — and killing
    /// the dispatch there discards the banked work the checkpoint exists to
    /// preserve.
    #[test]
    #[serial_test::serial]
    fn resume_catch_up_skips_a_refused_compaction_instead_of_killing_the_dispatch() {
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 5_000,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        let _primary = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200)
                .json_body(chat_response_json(Some("done"), None, "stop", 900, 20));
        });
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some(
                    "Summary: the assistant issued a read against the workspace and \
                     inspected the result. Nothing was finalized and no files changed. \
                     The next action is to continue reading and then act on the contents.",
                ),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-skip").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];

        // Weight in the PRESERVED TAIL, tiny compactable middle — the shape
        // whose middle cannot reach the min-reduction bar however good the
        // compactor is.
        let huge = "x".repeat(120_000);
        let assistant_turn = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{\"path\":\"/workspace/x.txt\"}".into(),
                },
                extra_content: None,
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        let checkpoint_messages = vec![
            Message::system("test system"),
            Message::user("seed"),
            Message::user("tiny middle"),
            Message::assistant("ok"),
            Message::user("go"),
            assistant_turn,
            Message::tool_result("call_1", "read", &huge),
        ];

        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".into(),
            messages: checkpoint_messages,
            turns: 2,
            total_completion_tokens: 40,
            compactions: 0,
            pending_hand_back: None,
            // The catch-up block runs only when the checkpoint carries an
            // undispatched call — that is what "catch-up" means.
            pending_tool_calls: Some(vec![ToolCall {
                id: "call_2".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{\"path\":\"/workspace/y.txt\"}".into(),
                },
                extra_content: None,
            }]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        run_with_sleeper(
            &client, &client, "test-primary", vec![], &tools, &mut traj, false, &cfg,
            Some(4), None, None, None, Some(u32::MAX), None,
            std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect(
            "a resume catch-up compaction the thread shape makes impossible must be \
             SKIPPED, not fatal — propagating it discards the checkpoint's banked work",
        );

        assert!(
            compactor_mock.hits() >= 1,
            "the scenario must actually have attempted a catch-up compaction"
        );

        let raw = std::fs::read_to_string(
            tmp.path().join(".darkmux-runtime").join("trajectory.jsonl"),
        )
        .expect("trajectory must exist");
        let events: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let skipped = events.iter().filter(|v| v["type"] == "compaction.skipped").count();
        let installed = events.iter().filter(|v| v["type"] == "compaction").count();

        assert!(skipped >= 1, "the refused catch-up compaction must be recorded");
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).compactions() as usize, installed,
            "the counter must equal INSTALLED compactions ({installed}), not installs \
             plus the {skipped} refused catch-up attempt(s)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resume_catch_up_compaction_honors_bail_after_compactions() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            // Low enough that the resume catch-up's local chars/4 estimate
            // trips it unconditionally once the message-count floor (7) is
            // met — isolates the bail check from needing a precise token
            // count.
            threshold_tokens: 1,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };

        let server = crate::test_support::GuardedMockServer::start();
        let primary_mock = server.mock_expect_zero(
            "must NEVER be hit — escalating means the resume never reaches the main loop's \
             first post-resume request at all; assert_hits(0) below already pins this, this \
             declares the zero legitimate to GuardedMockServer too",
            |when, then| {
                when.method(POST)
                    .path("/v1/chat/completions")
                    .body_contains("\"model\":\"test-primary\"");
                then.status(200).json_body(chat_response_json(Some("should not be reached"), None, "stop", 100, 5));
            },
        );
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
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
        let tmp = tempfile::Builder::new().prefix("resume-bail").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let c1 = make_call("call_1", 1);
        let c2 = make_call("call_2", 2);
        let assistant_turn = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![c1.clone(), c2.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        // (#1389) Padding so the compacted middle clears the min-reduction
        // guard, same rationale as the sibling bail tests above — a
        // single-message middle can't shrink 20% against a summary of
        // comparable length, so this needs SEVERAL padding messages
        // (matching the pattern the sibling bail tests already use), not
        // just one.
        let pad = "context detail that occupies transcript space ".repeat(6);
        let mut checkpoint_messages = vec![Message::system("test system"), Message::user(format!("seed: {pad}"))];
        for i in 0..3 {
            checkpoint_messages.push(Message::user(format!("padding user {i}: {pad}")));
            checkpoint_messages.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        checkpoint_messages.push(assistant_turn);
        checkpoint_messages.push(Message::tool_result("call_1", "read", "<call 1 result>"));

        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: checkpoint_messages,
            turns: 2,
            total_completion_tokens: 40,
            // (#2114 finding 1 test) bail_after_compactions - 1: the
            // catch-up's own compaction is the ONE that crosses the bound.
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![c2]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-primary", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("bail should produce Ok with EscalationTriggered, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionLimitReached),
            "the resume catch-up's own compaction must escalate at the bound, not just the \
             main loop's"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).compactions(), 1, "the bound-crossing compaction is counted");
        assert_eq!(compactor_mock.hits(), 1, "exactly one compactor call, during catch-up");
        primary_mock.assert_hits(0);
    }

    /// (#3074) RESUME grain twin of
    /// `a_lexically_repaired_structured_compaction_is_flagged_on_the_trajectory_event`:
    /// a structured catch-up compaction whose reply was cut off and lexically
    /// repaired installs, and its `compaction` event says so. The bound of 1
    /// ends the run right after the install, so the event is the catch-up's.
    #[test]
    #[serial_test::serial]
    fn a_lexically_repaired_resume_catch_up_compaction_is_flagged_on_the_trajectory_event() {
        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");
        let cfg = compaction::CompactionConfig {
            compactor_context_window: None,
            threshold_tokens: 1,
            compactor_model: Some("test-compactor".to_string()),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::StructuredSlot,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };
        let server = crate::test_support::GuardedMockServer::start();
        let primary_mock = server.mock_expect_zero(
            "never hit: the catch-up compaction crosses the bound of 1 before the first post-resume request",
            |when, then| {
                when.method(POST)
                    .path("/v1/chat/completions")
                    .body_contains("\"model\":\"test-primary\"");
                then.status(200).json_body(chat_response_json(Some("should not be reached"), None, "stop", 100, 5));
            },
        );
        let truncated = r#"{"objective": "finish", "current_truth": {}, "compaction_metadata": {"schema_version": "0.1", "generation": 1, "source_message_count": 3}, "completed_decisions": "decision one; decis"#;
        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200)
                .json_body(chat_response_json(Some(truncated), None, "length", 500, 30));
        });
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("resume-repaired").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let tools = [Tool::Read];

        let make_call = |id: &str, offset: u32| ToolCall {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::lmstudio::FunctionCall {
                name: "read".into(),
                arguments: format!("{{\"path\":\"/workspace/x.txt\",\"offset\":{offset},\"limit\":1}}"),
            },
            extra_content: None,
        };
        let c1 = make_call("call_1", 1);
        let c2 = make_call("call_2", 2);
        let assistant_turn = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![c1, c2.clone()]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        // Same padding as the bail twin above, so the middle clears the
        // min-reduction guard.
        let pad = "context detail that occupies transcript space ".repeat(6);
        let mut checkpoint_messages = vec![Message::system("test system"), Message::user(format!("seed: {pad}"))];
        for i in 0..3 {
            checkpoint_messages.push(Message::user(format!("padding user {i}: {pad}")));
            checkpoint_messages.push(Message::assistant(format!("padding assistant {i}: {pad}")));
        }
        checkpoint_messages.push(assistant_turn);
        checkpoint_messages.push(Message::tool_result("call_1", "read", "<call 1 result>"));
        let resume_checkpoint = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: "test-role".to_string(),
            messages: checkpoint_messages,
            turns: 2,
            total_completion_tokens: 40,
            compactions: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![c2]),
            pending_tool_calls_seq_base: 1,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-primary", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("the bound produces Ok with EscalationTriggered");
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionLimitReached),
            "fixture sanity: the catch-up compaction installed and crossed the bound"
        );
        assert_eq!(compactor_mock.hits(), 1, "exactly one compactor call, during catch-up");
        primary_mock.assert_hits(0);

        let raw = std::fs::read_to_string(tmp.path().join(".darkmux-runtime").join("trajectory.jsonl")).unwrap();
        let installed: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| v["type"] == "compaction")
            .collect();
        assert_eq!(installed.len(), 1, "exactly the catch-up's compaction installed: {raw}");
        assert_eq!(
            installed[0]["lexically_repaired"], true,
            "the catch-up's installed compaction event must carry the repair flag: {}", installed[0]
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
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns() >= 2,
            "expected at least 2 turns (stall + recovery); got {}",
            crate::trajectory::recorded(tmp.path()).turns()
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
        let server = crate::test_support::GuardedMockServer::start();
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
        // (#1221) What "the work survives" means on a DEGENERATE fixture.
        //
        // This mock returns the identical sentence forever, so the turn is
        // degenerate by construction. The loop still does the #1221 thing: it
        // checkpoints, and each checkpoint hands the WHOLE accumulation back in
        // ONE growing message rather than discarding the truncated call — six
        // `continue` verdicts before the gate can see the cycle at all, then
        // `conclude`.
        //
        // What it must NOT do is DELETE that accumulation. An earlier cut did:
        // a degenerate verdict on a turn with no open thought abandoned the
        // prefill, spent a recovery unit, and nudged. Measured against
        // realistic answer shapes at the shipped threshold, that verdict is
        // wrong for whole classes of legitimate output — an enum-valued JSON
        // array scores 0.003, a block of identical match arms 0.003, an ASCII
        // table frame 0.003, a checklist with an invariant line 0.002. A review
        // probe drove that path with an 11 KB first chunk and the operator
        // received "Done."
        //
        // So repetition now STOPS the turn and hands off with everything
        // attached. Same terminal as before, reached without destroying data.
        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let traj = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoints: Vec<serde_json::Value> = traj
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .collect();
        let verdicts: Vec<String> = checkpoints
            .iter()
            .filter_map(|e| e["verdict"].as_str().map(str::to_string))
            .collect();
        let continues = verdicts.iter().filter(|v| *v == "continue").count();
        assert!(
            continues >= 5,
            "the truncated calls must ACCUMULATE, not be discarded one by one — \
             only {continues} `continue` checkpoint(s) in {} verdict(s): {verdicts:?}",
            verdicts.len()
        );
        assert!(
            verdicts.iter().any(|v| v == "conclude"),
            "the gate must eventually SEE the repetition — verdicts were {verdicts:?}"
        );
        // (#2165 CONSIDER item 5, corrected by the #2171 rebase) The
        // original assertion here claimed "a checkpoint only ever fires on
        // the reasoning check-in interval" and pinned every checkpoint
        // record's `bound.kind` to `reasoning_checkpoint_interval`
        // unconditionally. That was already inaccurate on #2165 alone: this
        // very fixture proves it — the "this model has produced no
        // reasoning region" line above confirms `dispatch_has_reasoned`
        // never flips true, so `carries_reasoning_bound` is false for every
        // call in this test, and the checkpoint-continuation machinery is
        // reached via the ANSWER bound (`max_tokens_per_call=1000`), not the
        // reasoning interval — #2165's own checkpoint-record site hardcoded
        // `BoundKind::ReasoningCheckpointInterval` regardless, so the
        // MISLABEL passed silently until this rebase replaced that hardcode
        // with `active_bound(sent_reasoning_bound, sent_generation_bound,
        // per_call_cap)`, which reads back whichever bound the request
        // actually carried. The corrected assertion below is what this
        // fixture was always actually exercising.
        for cp in &checkpoints {
            assert_eq!(
                cp["bound"]["kind"], serde_json::json!("max_tokens_per_call"),
                "every checkpoint record in this reasoning-free fixture must name the answer                  bound (max_tokens_per_call) it was actually governed by, got {cp:?}"
            );
            assert_eq!(
                cp["bound"]["value"], serde_json::json!(1000),
                "the answer bound this test set via max_tokens_per_call, got {cp:?}"
            );
        }
        // Exactly what main.rs does to produce the deliverable.
        let delivered = outcome
            .final_answer
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| {
                outcome
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "assistant")
                    .and_then(|m| m.content.clone())
            })
            .unwrap_or_default();
        let kept = delivered.matches("half my answer").count();
        assert!(
            kept >= 5,
            "escalating must carry the accumulation, not delete it — the operator \
             got {kept} occurrence(s) of the work: {:?}",
            &delivered[..delivered.len().min(120)]
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

    /// (#2258) The degeneracy gate's tail window must be sized by whichever
    /// bound actually GOVERNED the call, not by `reasoning_interval`
    /// unconditionally. This dispatch never reasons, so every length-finish
    /// is GENERATION-bound (`sent_generation_bound`) — `active_bound`
    /// resolves to `generation_checkpoint_interval`, not the reasoning
    /// interval, for every one of these calls.
    ///
    /// `reasoning_checkpoint_interval` (10) is set deliberately SMALLER than
    /// `generation_checkpoint_interval` (50) here so the two produce
    /// measurably different tail windows at `TAIL_SAMPLE_INTERVALS=8`: 80
    /// tokens (wrong — the pre-#2258 bug) vs. 400 tokens (right — the bound
    /// that actually governed).
    ///
    /// The mock returns the SAME 90-distinct-word block every call
    /// (`w0`..`w89`), so the accumulation is exactly periodic with period 90
    /// tokens, and `tail_repetition_ratio`'s distinct-windows count is fixed
    /// at 90 once at least one full period is in view. The period is
    /// DELIBERATELY WIDER than the 50-token `generation_checkpoint_interval`
    /// that governs the correct tail — see the guard note below for why a
    /// period equal to (or narrower than) the governing interval is not
    /// enough. At checkpoint 5 (450 tokens accumulated over 5 calls):
    ///   - sized by the interval that governed (generation, 50 → tail 400,
    ///     narrower than the 450 accumulated, so only the last 400 tokens
    ///     are sampled): 389 windows, 90 distinct — ratio 90/389 ≈0.2314,
    ///     under the 0.25 degenerate threshold.
    ///   - sized by the interval that did NOT govern (reasoning, 10 → tail
    ///     80, already smaller than one period): 69 windows, all distinct
    ///     (a sub-period slice of unique tokens can't repeat) — ratio 1.0,
    ///     comfortably CLEAN.
    /// (Checkpoint 5, not 8: the ratio crosses 0.25 as soon as the
    /// correctly-sized tail exceeds 349 windows' worth — 360 accumulated at
    /// checkpoint 4 still reads ≈0.258, clean — so a correct gate never
    /// reaches checkpoint 8 in this fixture at all.)
    ///
    /// (#2258 review, guard hardening) A 50-token period — equal to the
    /// governing generation interval — is NOT enough to red-prove Mutation
    /// C (a "fix" that reads the raw `answer_max_tokens` cap, 1000 here,
    /// instead of whichever bound actually governed): at checkpoint 5 with
    /// a 50-token period, BOTH the correctly-sized tail (400, computed
    /// above) and the mutant tail (1000 × 8 = 8000) exceed the 250-token
    /// accumulation, so both sample the WHOLE accumulation and produce the
    /// bit-identical ratio 50/239 ≈0.2092 — a mutant that ignores which
    /// bound governed passes silently. Widening the period to 90 keeps the
    /// correctly-sized tail (400) narrower than the 450-token accumulation
    /// (sampling only the tail, ratio ≈0.2314) while the mutant tail (8000)
    /// still swallows the whole accumulation (ratio 90/439 ≈0.2050) — both
    /// verdicts still read "conclude" (both under 0.25), so the guard below
    /// asserts the exact ratio, not just the threshold.
    #[test]
    #[serial_test::serial]
    fn generation_bound_degeneracy_gate_sizes_the_tail_by_the_generation_interval() {
        let block: String = (0..90).map(|i| format!("w{i} ")).collect();
        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .json_body(chat_response_json(Some(&block), None, "length", 100, 90));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("gen-degeneracy-window").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("write forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(50), None, Some(1000), Some(10), Some(50),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a generation-bound degenerate repeat is a clean escalation, not an Err");

        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoints: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .collect();
        let fifth = checkpoints
            .iter()
            .find(|c| c["checkpoint"] == serde_json::json!(5))
            .unwrap_or_else(|| panic!("expected a checkpoint 5 record, got {checkpoints:?}"));

        assert_eq!(
            fifth["bound"]["kind"],
            serde_json::json!("generation_checkpoint_interval"),
            "checkpoint 5 must be governed by the generation check-in in this fixture, \
             got {fifth:?}"
        );
        let ratio = fifth["tail_ratio"].as_f64().unwrap_or_else(|| {
            panic!("checkpoint 5 must have a numeric tail_ratio, got {fifth:?}")
        });
        // (#2258 review, guard hardening) A threshold check (`ratio < 0.25`)
        // only catches UNDER-sizing the tail (a narrower-than-governing tail
        // reads clean, ratio 1.0 — see the fixture doc). It does NOT catch
        // Mutation C (a "fix" that always reads the raw `answer_max_tokens`
        // cap, ignoring which bound governed): with this 90-token period,
        // the mutant tail (8000) still swallows the whole 450-token
        // accumulation and produces ratio 90/439 ≈0.2050, which is ALSO
        // under 0.25 — the threshold alone can't tell the two apart. The
        // exact value can: sized by the GOVERNING generation interval (50 →
        // tail 400, narrower than the 450-token accumulation) the sampled
        // tail is 389 windows with all 90 periodic tokens distinct, giving
        // exactly 90/389 ≈0.23136. Any other value means the tail was sized
        // by something other than the interval that actually governed.
        let expected_ratio = 90.0_f64 / 389.0_f64;
        assert!(
            (ratio - expected_ratio).abs() < 1e-4,
            "(#2258) checkpoint 5 has 450 accumulated tokens of an exactly-periodic \
             90-token block — sized by the GENERATION interval (50, the bound that \
             actually governed this call) the tail is 400 tokens, narrower than the \
             accumulation, giving 389 windows and exactly 90/389 ≈{expected_ratio:.4} \
             distinct. A ratio of {ratio} means the gate did not size the tail by the \
             interval that governed — either the pre-#2258 regression (a \
             narrower-than-governing tail, ratio near 1.0) or a fix that reads the raw \
             per-call cap regardless of which bound governed (ratio ≈0.2050, this \
             fixture's Mutation C)."
        );
        assert_eq!(
            fifth["verdict"],
            serde_json::json!("conclude"),
            "a ratio under the threshold must verdict conclude, got {fifth:?}"
        );
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "the gate must catch the repeat at checkpoint 5 and escalate cleanly — a \
             narrower-than-governing tail instead lets it run past the degeneracy gate \
             all the way to the independent generation-continuation budget backstop, \
             got {:?}",
            outcome.terminal_reason
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "every hit is a continuation of the same logical turn");
    }

    /// (#2633) The degeneracy gate must actually RUN on the generation-bound
    /// length-finish arm at the LITERAL shipped defaults — no overrides, so
    /// `MAX_TOKENS_PER_CALL` (10000) and `GENERATION_CHECKPOINT_INTERVAL`
    /// (4000) govern, a ratio of **2.5**.
    ///
    /// That ratio is the whole point. #2258's two fixtures run at
    /// `max_tokens_per_call=1000` against a 50-token interval — a ratio of
    /// **20** — which puts `max_generation_continuations` at 20 and leaves
    /// the gate all the room it needs. They passed while the shipped
    /// configuration could not reach the gate at all, which is exactly the
    /// mistake this fixture exists not to repeat: it takes its numbers from
    /// the constants themselves rather than from convenient literals, so it
    /// cannot drift away from what operators actually run.
    ///
    /// The collision it pins, in arithmetic:
    ///
    /// - The gate's metric is distinct-windows over total-windows across a
    ///   tail of `TAIL_SAMPLE_INTERVALS` (8) intervals. A verbatim loop
    ///   whose every call exactly fills the interval has `k` identical
    ///   copies accumulated by checkpoint `k`, so the ratio is `interval /
    ///   (k * interval - (TAIL_WINDOW_TOKENS - 1))` — about `1/k`. It first
    ///   dips under the 0.25 threshold at **k = 5**, and that crossing is a
    ///   fixed `k` no matter how large the interval is, because numerator
    ///   and denominator scale together.
    /// - `max_generation_continuations` is `(answer_max_tokens /
    ///   generation_interval).max(4)`.
    ///
    /// So the budget's stop and the gate's first possible verdict can land on
    /// the same call, and whichever runs first wins. Measured on the merged
    /// code when `MAX_TOKENS_PER_CALL` was 10,000 — a 2.5:1 ratio, floored to
    /// a budget of 4: checkpoints 1-4 at 1.0000 / 0.5007 / 0.3336 / 0.2502,
    /// all `continue`, then `EscalationTriggered(GenerationCheckpointBudget
    /// Exhausted)` with only FOUR checkpoint records — the 5th call's slice,
    /// which reads 0.2001 and is plainly degenerate, was never judged.
    ///
    /// **(#2836 stage 2) The collision is now resolved by arithmetic, and
    /// this fixture pins the resolution rather than the collision.** Raising
    /// `MAX_TOKENS_PER_CALL` to 32,000 puts the budget at 8, comfortably past
    /// the gate's `k = 5`. That is not a happy accident to be re-derived
    /// later: a repeating turn must end on the REPETITION, which is what
    /// darkmux observed, rather than on a budget running out, which is an
    /// accounting fact true of any turn that long. The guard below therefore
    /// asserts the INVARIANT — the budget must not preempt the gate — instead
    /// of the particular ratio that happened to be shipping, so a future
    /// retune of either constant is caught the moment it re-creates the
    /// defect.
    ///
    /// The assertions below are the after-state of that same measurement, and
    /// they read identically in both regimes: what changed is WHY checkpoint
    /// 5 exists — #2633 made the gate run before the budget acted; 32,000
    /// means the budget is not even close.
    #[test]
    #[serial_test::serial]
    fn degeneracy_gate_runs_at_the_shipped_generation_ratio_not_just_a_roomy_one() {
        // The fixture's numbers come from the constants, never from
        // literals — a future retune of either one keeps this honest.
        // The gate's first possible degenerate verdict is a FIXED k = 5 (the
        // ratio is ~1/k and numerator and denominator scale together, so the
        // crossing does not move with the interval). The continuation budget
        // must leave room to reach it.
        const GATE_FIRST_VERDICT_AT: u32 = 5;
        let budget = (MAX_TOKENS_PER_CALL / GENERATION_CHECKPOINT_INTERVAL.max(1)).max(4);
        assert!(
            budget >= GATE_FIRST_VERDICT_AT,
            "the generation-continuation budget ({budget}, from \
             {MAX_TOKENS_PER_CALL}/{GENERATION_CHECKPOINT_INTERVAL} floored at 4) would \
             escalate BEFORE the degeneracy gate can first return a verdict (k={GATE_FIRST_VERDICT_AT}). \
             A repeating turn would then end on 'the budget ran out' instead of on the \
             repetition darkmux actually observed — #2633's defect, re-created by a \
             constant retune. Raise MAX_TOKENS_PER_CALL or lower \
             GENERATION_CHECKPOINT_INTERVAL."
        );
        // One call's worth of output, exactly filling the check-in interval,
        // with every token distinct so the accumulation's period is exactly
        // the interval — the verbatim-loop shape the gate is tuned for.
        let block: String = (0..GENERATION_CHECKPOINT_INTERVAL).map(|i| format!("w{i} ")).collect();

        let server = crate::test_support::GuardedMockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(&block),
                None,
                "length",
                100,
                GENERATION_CHECKPOINT_INTERVAL,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("shipped-ratio-gate").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("write forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // Every interval knob is None: the shipped constants govern.
        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(50), None, None, None, None,
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a degenerate generation-bound repeat is a clean escalation, not an Err");

        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoints: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .collect();

        // Checkpoint 4 is the near-miss that makes this the SHIPPED case and
        // not a roomier one: 4000/(4*4000-11) = 0.2502, above the 0.25
        // threshold by 0.0002. If this fired early, the fixture would prove
        // nothing about the call the budget used to take.
        let fourth = checkpoints
            .iter()
            .find(|c| c["checkpoint"] == serde_json::json!(4))
            .unwrap_or_else(|| panic!("expected a checkpoint 4 record, got {checkpoints:?}"));
        assert_eq!(
            fourth["verdict"],
            serde_json::json!("continue"),
            "checkpoint 4 sits just ABOVE the threshold (0.2502 vs 0.25) — a conclude \
             here means the gate fired a call early and the fixture is no longer \
             exercising the collision, got {fourth:?}"
        );

        let fifth = checkpoints
            .iter()
            .find(|c| c["checkpoint"] == serde_json::json!(5))
            .unwrap_or_else(|| {
                panic!(
                    "(#2633) NO checkpoint 5 record — the call that first reads DEGENERATE \
                     was never judged, because the generation-continuation budget escalated \
                     ahead of the gate. This is the defect verbatim. Got {checkpoints:?}"
                )
            });
        assert_eq!(
            fifth["bound"]["kind"],
            serde_json::json!("generation_checkpoint_interval"),
            "this dispatch never reasons, so every call is generation-bound — a different \
             bound here means the fixture drifted off the arm under test, got {fifth:?}"
        );
        let ratio = fifth["tail_ratio"].as_f64().unwrap_or_else(|| {
            panic!("checkpoint 5 must have a numeric tail_ratio, got {fifth:?}")
        });
        // 5 * 4000 accumulated tokens, all inside the 8 * 4000 tail, so the
        // whole accumulation is sampled: 19989 windows over a period-4000
        // sequence gives exactly 4000 distinct.
        let expected = 4000.0_f64 / 19989.0_f64;
        assert!(
            (ratio - expected).abs() < 1e-4,
            "checkpoint 5's tail ratio must be exactly 4000/19989 ≈{expected:.4} — a \
             different value means the tail was sized by something other than the \
             generation interval that governed the call. Got {ratio}"
        );
        assert_eq!(
            fifth["verdict"],
            serde_json::json!("conclude"),
            "a ratio under the 0.25 threshold must verdict conclude, got {fifth:?}"
        );
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "(#2633) a turn that is REPEATING must terminate as a repeat. \
             GenerationCheckpointBudgetExhausted here means the budget check ran ahead of \
             the gate again and the operator is told an accounting fact ('this turn spent \
             its continuations') in place of the diagnosis ('this turn is emitting the \
             same block over and over'). Got {:?}",
            outcome.terminal_reason
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "every hit is a continuation of the SAME logical turn");
    }

    /// (#2633 fix-pass) The SECOND half of the budget block's ordering: it
    /// must act BEFORE the remedy branches, not just after the degeneracy
    /// escalation. The fixture above pins "after the gate"; nothing pinned
    /// "before the remedy", and moving the whole block down past the
    /// `degenerate && writing_thought` chain left the suite green.
    ///
    /// What the mutation costs, concretely: the remedy for a degenerate
    /// THOUGHT is `turn.close_thought()`, and a closed thought changes what
    /// `pending_answer()` hands over — `deliverable("")` emits the thought
    /// region ONLY while the block is still open (see its doc). Run the
    /// remedy first and the escalation below it hands the frontier the
    /// answer region alone, with the entire thought this turn banked
    /// silently dropped. That is #1221's discard-the-turn bug, re-entered
    /// through the ordering rather than through the region machine.
    ///
    /// Reaching all three conditions on ONE call takes some care, because
    /// `sent_generation_bound` and an open thought pull against each other:
    /// an open thought makes `in_answer_region()` false, so the only way a
    /// call carrying one is still generation-bound is
    /// `dispatch_has_reasoned == false` at REQUEST time — and any call that
    /// leaves an open thought sets that flag true for every call after it.
    /// So the one reachable shape is exactly this: four generation-bound
    /// calls that never reason at all, then a fifth whose content OPENS a
    /// `<think>` it does not close and whose slice reads degenerate. The
    /// fifth call's request was built while the flag was still false, so it
    /// is generation-bound and draws the continuation that exhausts the
    /// budget; its response is what makes `writing_thought()` true.
    ///
    /// The numbers: `max_tokens_per_call=200` / `generation_checkpoint_
    /// interval=50` puts `max_generation_continuations` at
    /// `max(200/50, 4) == 4`, so call 5 is the one that exhausts it, and
    /// `tail_sample_tokens(50)` is 400 tokens.
    ///
    /// - Calls 1-4 return a 500-token block of DISTINCT tokens. One period
    ///   (500) is deliberately WIDER than the judged tail (400) — the same
    ///   property #2258's fixtures rely on — so the sampled tail is always a
    ///   sub-period run of unique tokens, ratio 1.0, robustly CLEAN. Without
    ///   that these four would escalate early through the ANSWER-region arm
    ///   and the fixture would never reach the call it exists to test.
    /// - Call 5 returns `<think>` + `"loop forever "` x300. Its tail is 400
    ///   tokens of a period-2 sequence: 389 windows, 2 distinct, ratio
    ///   ≈0.0051 — degenerate with a ~49x margin.
    ///
    /// The two mocks are told apart by counting `ZZBLOCK` (one per absorbed
    /// block) in the outgoing prefill: 0-3 copies is a call in the first
    /// group, 4 is the fifth call.
    #[test]
    #[serial_test::serial]
    fn the_generation_budget_escalates_before_the_thought_is_closed() {
        let plain_block: String = std::iter::once("ZZBLOCK ".to_string())
            .chain((0..499).map(|i| format!("a{i} ")))
            .collect();
        let thought_block = format!("<think>\n{}", "loop forever ".repeat(300));

        let server = crate::test_support::GuardedMockServer::start();
        // Calls 1-4: plain content, never any reasoning, so every request is
        // built with `dispatch_has_reasoned == false` and carries the
        // generation bound.
        let _plain = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req
                    .body
                    .as_ref()
                    .map(|v| String::from_utf8_lossy(v).to_string())
                    .unwrap_or_default();
                b.matches("ZZBLOCK").count() <= 3
            });
            then.status(200)
                .json_body(chat_response_json(Some(&plain_block), None, "length", 100, 50));
        });
        // Call 5: opens a thought it never closes, degenerate inside.
        let _thinking = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req
                    .body
                    .as_ref()
                    .map(|v| String::from_utf8_lossy(v).to_string())
                    .unwrap_or_default();
                b.matches("ZZBLOCK").count() >= 4
            });
            then.status(200).json_body(chat_response_json(
                Some(&thought_block),
                None,
                "length",
                100,
                50,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("budget-before-remedy").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("write forever")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(50), None, Some(200), None, Some(50),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("budget exhaustion on a thought-carrying call is a clean escalation, not an Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(
                EscalationReason::GenerationCheckpointBudgetExhausted
            ),
            "call 5 is the one that exhausts the 4-continuation budget, and its thought is \
             still OPEN — so the budget, not the ANSWER-region degeneracy arm, is what \
             stops this turn. Got {:?}",
            outcome.terminal_reason
        );

        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoints: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .collect();
        let fifth = checkpoints
            .iter()
            .find(|c| c["checkpoint"] == serde_json::json!(5))
            .unwrap_or_else(|| panic!("expected a checkpoint 5 record, got {checkpoints:?}"));
        assert_eq!(
            fifth["verdict"],
            serde_json::json!("conclude"),
            "checkpoint 5's slice is a period-2 verbatim loop — if this reads `continue` the \
             fixture has drifted off the shape under test (a clean slice never reaches the \
             remedy branch this test guards). Got {fifth:?}"
        );
        assert_eq!(
            fifth["bound"]["kind"],
            serde_json::json!("generation_checkpoint_interval"),
            "call 5 must still be GENERATION-bound — a reasoning bound here means \
             `dispatch_has_reasoned` flipped before the request was built and the budget \
             was never drawn. Got {fifth:?}"
        );

        // THE GUARD. Both regions must reach the frontier. `close_thought()`
        // running before this escalation would drop the thought half — the
        // marker below is the only thing that distinguishes the two
        // orderings, because every other observable (terminal reason, record
        // count, verdict, ratio, call count) is identical under both.
        let delivered = outcome
            .final_answer
            .as_deref()
            .expect("the escalation must hand over everything banked, not None");
        assert!(
            delivered.contains("loop forever"),
            "(#2633 fix-pass) the THOUGHT this turn banked is missing from the deliverable. \
             That is what acting on the budget AFTER the `degenerate && writing_thought` \
             remedy costs: the remedy closes the thought, and a closed thought is excluded \
             from `deliverable()`, so the frontier receives the answer region alone. \
             Got {delivered:?}"
        );
        assert!(
            delivered.contains("ZZBLOCK"),
            "the four earlier calls' ANSWER region must survive too — this half held even \
             under the wrong ordering, and it is here so a future change that trades one \
             region for the other is caught in both directions. Got {delivered:?}"
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 1, "every hit is a continuation of the SAME logical turn");
    }

    /// (#2258) The INVERTED direction of the fixture above — a fix that
    /// simply swapped the hardcoded `reasoning_interval` for a hardcoded
    /// `generation_interval` (rather than reading back whichever bound
    /// actually governed) would pass the generation-bound fixture above and
    /// still be wrong: it would UNDER-size the tail for a genuinely
    /// REASONING-bound call. This fixture pins that direction so such a fix
    /// cannot ship.
    ///
    /// A priming turn (closed think block, dispatched via `finish_reason:
    /// tool_calls`) establishes `dispatch_has_reasoned`. Turn 2 then opens
    /// an UNCLOSED `<think>` and keeps re-affirming it every call (the mock
    /// returns the identical `<think> w1..w49` slice each time), so
    /// `carries_reasoning_bound` stays true and every length-finish in turn
    /// 2 is REASONING-bound (`sent_reasoning_bound`) — the opposite
    /// governing bound from the fixture above.
    ///
    /// `generation_checkpoint_interval` (10) is set deliberately SMALLER
    /// than `reasoning_checkpoint_interval` (50) — the mirror image of the
    /// generation-bound fixture's interval choice — so a fix that reads the
    /// wrong bound is caught in BOTH directions: reading `reasoning_interval`
    /// unconditionally is caught above; reading `generation_interval`
    /// unconditionally (or otherwise failing to read back what this call
    /// actually carried) is caught here.
    ///
    /// The block is `"<think> "` + 89 distinct words = 90 tokens/call, same
    /// period as the fixture above, so checkpoint 5 lands at the identical
    /// 450-accumulated-tokens point with the identical expected ratios
    /// (~0.2314 sized by the governing 50-interval, 1.0 sized by the wrong
    /// 10-interval) — the two fixtures are deliberately numerically
    /// symmetric, just with the roles of the two intervals swapped. (#2258
    /// review, guard hardening: the 90-token period, wider than the
    /// 50-token governing interval, is what lets checkpoint 5's exact ratio
    /// discriminate Mutation C — a fix that reads the raw
    /// `answer_max_tokens` cap regardless of which bound governed — the
    /// same reason the fixture above widened its own period from 50; see
    /// its doc for the full math.)
    #[test]
    #[serial_test::serial]
    fn reasoning_bound_degeneracy_gate_sizes_the_tail_by_the_reasoning_interval() {
        let reasoning_block: String =
            std::iter::once("<think> ".to_string()).chain((1..=89).map(|i| format!("w{i} "))).collect();

        let server = crate::test_support::GuardedMockServer::start();
        // Priming turn: a closed think block dispatched cleanly via
        // finish_reason=tool_calls, matched on the ABSENCE of a "role":
        // "tool" message — establishes `dispatch_has_reasoned` before the
        // scenario under test. Same priming pattern as
        // `salvage_record_names_the_reasoning_checkpoint_interval_bound`.
        let _priming = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(
                Some("<think>brief</think>"),
                Some(serde_json::json!([{
                    "id": "c0",
                    "type": "function",
                    "function": { "name": "echo", "arguments": "{\"text\":\"priming\"}" }
                }])),
                "tool_calls",
                100,
                20,
            ));
        });
        // Turn 2: every length-finish is a reasoning-bound checkpoint
        // continuation — matched on the PRESENCE of the priming turn's
        // tool-result message, persistent for every call after.
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 1
            });
            then.status(200).json_body(chat_response_json(
                Some(&reasoning_block),
                None,
                "length",
                100,
                90,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("reasoning-degeneracy-window").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Echo, Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // (#2258) `max_cumulative_tokens=700` is a bounded backstop, not the
        // signal under test — a reasoning-bound thought is deliberately
        // open-ended (no continuation budget of its own), and once the
        // thought closes (checkpoint 5, correctly sized) the turn moves on
        // to generation-bound answer-region continuations that could run
        // for a while too — so without SOME ceiling this fixture could run
        // long if the gate never fires at all. The assertions below read
        // the checkpoint-5 record directly rather than the eventual
        // terminal_reason, since both a correct and a wrong governing
        // interval can reach the SAME cumulative-cap terminal eventually —
        // the divergence is only visible mid-run.
        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(50), Some(700), Some(1000), Some(50), Some(10),
            None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("a reasoning-bound degenerate repeat must not error the dispatch");
        let _ = outcome;

        let traj_file = tmp.path().join(".darkmux-runtime").join("trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_file).expect("trajectory written");
        let checkpoints: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
            .collect();
        let fifth = checkpoints
            .iter()
            .find(|c| c["checkpoint"] == serde_json::json!(5))
            .unwrap_or_else(|| panic!("expected a checkpoint 5 record, got {checkpoints:?}"));

        assert_eq!(
            fifth["bound"]["kind"],
            serde_json::json!("reasoning_checkpoint_interval"),
            "checkpoint 5 must be governed by the reasoning check-in in this fixture, \
             got {fifth:?}"
        );
        let ratio = fifth["tail_ratio"].as_f64().unwrap_or_else(|| {
            panic!("checkpoint 5 must have a numeric tail_ratio, got {fifth:?}")
        });
        // (#2258 review, guard hardening) Same reasoning as the fixture
        // above: a threshold check alone can't distinguish the correctly-
        // sized tail from Mutation C's raw-cap tail (both land under 0.25
        // at this accumulation), so assert the exact value the GOVERNING
        // reasoning interval implies (400-token tail over a 450-token
        // accumulation → 389 windows, 90 distinct → 90/389 ≈0.23136).
        let expected_ratio = 90.0_f64 / 389.0_f64;
        assert!(
            (ratio - expected_ratio).abs() < 1e-4,
            "(#2258, inverted direction) checkpoint 5 has 450 accumulated tokens of an \
             exactly-periodic 90-token block — sized by the REASONING interval (50, the \
             bound that actually governed this call) the tail is 400 tokens, narrower \
             than the accumulation, giving 389 windows and exactly 90/389 \
             ≈{expected_ratio:.4} distinct. A ratio of {ratio} means the gate did not \
             size the tail by the interval that governed — either a fix that reads the \
             GENERATION interval here (ratio near 1.0, moving the #2258 bug rather than \
             fixing it) or a fix that reads the raw per-call cap regardless of which \
             bound governed (ratio ≈0.2050, this fixture's Mutation C)."
        );
        assert_eq!(
            fifth["verdict"],
            serde_json::json!("conclude"),
            "a ratio under the threshold must verdict conclude, got {fifth:?}"
        );
    }

    /// (#414 PR A → #1221) Coverage for the `tool_calls: []` empty-array
    /// shape (distinct from `tool_calls: null`/absent) WITH content at the
    /// cap. Same #1221 re-target as the content-present case: recovers via
    /// drop + nudge instead of killing the dispatch.
    #[test]
    #[serial_test::serial]
    fn length_with_content_and_empty_tool_calls_array_recovers_at_cap() {
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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
        let server = crate::test_support::GuardedMockServer::start();
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
            crate::trajectory::recorded(tmp.path()).turns(), MAX_STALL_RECOVERIES + 1,
            "expected exactly MAX_STALL_RECOVERIES+1 turns (=={}); got {}",
            MAX_STALL_RECOVERIES + 1,
            crate::trajectory::recorded(tmp.path()).turns()
        );
    }

    /// (#1123) `finish_reason=tool_calls` with NO tool_calls (an empty
    /// completion — the shape a degraded devstral-24b run produced) must
    /// recover like the length-arm stall (#414), NOT hard-`Err` on the first
    /// occurrence. Every call returns the empty-tool_calls shape, so the loop
    /// recovers `MAX_STALL_RECOVERIES` times then escalates — same bounded
    /// behavior as the length-stall, proving the pre-#1123 hard-fail is gone.
    ///
    /// (#2190) Asserts `EmptyToolCallsExhausted`, NOT `IntraTurnStallExhausted`
    /// — this shape (an empty `tool_calls` array) is a protocol-shaped miss,
    /// not a runaway-reasoning cut, and #2190 split it into its own kind so
    /// the escalation names the real cause. Mutation-proof: reverting the
    /// arm's `EscalationReason::EmptyToolCallsExhausted` back to
    /// `IntraTurnStallExhausted` makes this assertion fail (confirmed by
    /// hand before writing this comment — see the PR's test evidence).
    #[test]
    #[serial_test::serial]
    fn loop_recovers_from_empty_tool_calls_then_escalates() {
        let server = crate::test_support::GuardedMockServer::start();
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
            TerminalReason::EscalationTriggered(EscalationReason::EmptyToolCallsExhausted),
            "empty finish_reason=tool_calls should route to its OWN escalation kind \
             (EmptyToolCallsExhausted), NOT the runaway-reasoning IntraTurnStallExhausted; \
             got {:?}",
            outcome.terminal_reason
        );
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), MAX_STALL_RECOVERIES + 1);
    }

    /// (#2229) Sibling of `a_mixed_turn_resets_the_consecutive_malformed_
    /// counter` above, for the OTHER budget in this loop. The stall budget
    /// is documented as CONSECUTIVE (`MAX_STALL_RECOVERIES`' own doc: "a
    /// second if the NEXT TURN also stalls. Three CONSECUTIVE stalls is the
    /// pathology signal") but was implemented lifetime-cumulative — inited
    /// to 0 once and only ever `saturating_add`ed, with no reset site
    /// anywhere in the function. A dispatch that recovered from one empty
    /// completion on turn 5, ran a thousand productive turns, then hit an
    /// unrelated empty completion on turn 1005 escalated out, because the
    /// counter was still sitting at 1/2 with no way back down. That bites
    /// hardest on the long unattended crawl units, which is exactly where
    /// an escalation costs the most.
    ///
    /// Interleaved stall → productive → stall → productive → stall. Each
    /// productive turn dispatches a real tool call, so the streak never
    /// reaches 3 and the run must reach MaxTurns(5) rather than escalating.
    ///
    /// Turn discrimination is the pair (`"role":"tool"` count, stall-nudge
    /// count) in the accumulating request body — the tool count alone
    /// collides here (a stall turn appends no tool message), so the nudge
    /// each recovery injects supplies the second coordinate:
    ///
    /// ```text
    /// turn1 (tool 0, nudge 0): STALL      → budget 0→1, +1 nudge
    /// turn2 (tool 0, nudge 1): productive → decay 1→0, +1 tool msg
    /// turn3 (tool 1, nudge 1): STALL      → budget 0→1, +1 nudge
    /// turn4 (tool 1, nudge 2): productive → decay 1→0, +1 tool msg
    /// turn5 (tool 2, nudge 2): STALL      → budget 0→1, loop continues
    /// ```
    ///
    /// Pre-fix the same five turns run 0→1, 1, 1→2, 2, then `2 >= 2` on
    /// turn 5 and escalate with `EmptyToolCallsExhausted`.
    ///
    /// This test does NOT pin the pay-down's PLACEMENT: it stays green under
    /// the interesting wrong placement (delete the site in the `"tool_calls"`
    /// arm, add an unconditional pay-down at the top of every turn). Five
    /// pre-existing tests catch that mutation, and they are the placement
    /// pins to run alongside this one:
    /// `loop_escalates_when_stall_recovery_budget_exhausted`,
    /// `loop_recovers_from_empty_tool_calls_then_escalates`,
    /// `max_stall_recoveries_override_changes_the_escalation_point`,
    /// `genuine_reasoning_bound_cut_still_produces_intra_turn_stall_kind`,
    /// `escalation_triggered_record_carries_model_and_prompt_tokens`.
    #[test]
    #[serial_test::serial]
    fn a_productive_turn_pays_down_the_stall_recovery_budget() {
        let server = crate::test_support::GuardedMockServer::start();
        // A distinctive fragment of `STALL_NUDGE_MESSAGE` — one occurrence
        // per recovery already injected. httpmock's `.matches()` takes a
        // plain `fn(&HttpMockRequest) -> bool` (a non-capturing function
        // pointer, not a `Fn` closure), so both literals are inlined per
        // closure rather than parametrized — same constraint the
        // consecutive-malformed sibling's matchers work around.
        let _t1 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
                    && b.matches("ended without a tool call and without a final answer").count() == 0
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "tool_calls", 100, 50));
        });
        let _t2 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
                    && b.matches("ended without a tool call and without a final answer").count() == 1
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([valid_read_call("call_ok_1", "/workspace/a.txt")])),
                "tool_calls",
                100,
                20,
            ));
        });
        let _t3 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1
                    && b.matches("ended without a tool call and without a final answer").count() == 1
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "tool_calls", 100, 50));
        });
        let _t4 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1
                    && b.matches("ended without a tool call and without a final answer").count() == 2
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([valid_read_call("call_ok_2", "/workspace/b.txt")])),
                "tool_calls",
                100,
                20,
            ));
        });
        let _t5 = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 2
                    && b.matches("ended without a tool call and without a final answer").count() == 2
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "tool_calls", 100, 50));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("stall-budget-reset").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
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
            Some(5),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "a productive turn must PAY DOWN the stall-recovery budget, so 5 turns whose \
             longest stall STREAK is 1 (never reaching the budget of {}) must run to \
             MaxTurns rather than escalating: {:?}",
            MAX_STALL_RECOVERIES,
            outcome.terminal_reason
        );
        // Assert the turn count too, the way this test's siblings do —
        // `MaxTurns` alone would also be produced by the run dying early on
        // a mock miss (an unmatched request 404s), which is not what this
        // test is about.
        assert_eq!(crate::trajectory::recorded(tmp.path()).turns(), 5, "all five scripted turns must have run");
    }

    /// (#2229 round-2 blocker 1) The budget pays DOWN by one per productive
    /// turn rather than resetting to zero, so it bounds a stall RATE.
    ///
    /// Zeroing bounds only BACK-TO-BACK stalls: one dispatched call between
    /// two stalls wipes the counter, so under `= 0` a model stalling TWICE
    /// for every productive turn runs 1, 2, 0, 1, 2, 0 … and never
    /// escalates — it is 67% useless and the loop tolerates it forever.
    /// Under decay the same model nets +1 per cycle and reaches the bound:
    ///
    /// ```text
    /// turn1 (tool 0, nudge 0): STALL → 0→1
    /// turn2 (tool 0, nudge 1): STALL → 1→2
    /// turn3 (tool 0, nudge 2): work  → decay 2→1
    /// turn4 (tool 1, nudge 2): STALL → 1→2
    /// turn5 (tool 1, nudge 3): STALL → 2 >= 2, escalate
    /// ```
    ///
    /// The gap this test does NOT close, stated so it is not mistaken for a
    /// claim: an EXACT 1:1 alternation (stall, work, stall, work, …)
    /// oscillates 1,0,1,0 and never reaches the bound. Measured — a model
    /// alternating a stall with one identical trivial `read` ran 40 turns
    /// without escalating, and nothing else in the loop stops it (the #418
    /// cycle and #419 failure-rate detectors are warn-only, and the host
    /// inactivity watchdog is reset by the trivial call's own successful
    /// `tool.completed`). That shape is bounded only by an operator-set
    /// `max_turns`/`max_cumulative_tokens`. Closing it needs a second
    /// absolute bound and is deliberately deferred; see
    /// `MAX_STALL_RECOVERIES`' doc.
    ///
    /// Turn discrimination is the same (tool-message count, nudge count)
    /// pair the sibling above uses, expressed as a RELATION: with two
    /// stalls per productive turn, `nudges - 2 * tools` is the number of
    /// stalls since the last productive turn.
    #[test]
    #[serial_test::serial]
    fn a_two_to_one_stall_ratio_still_escalates() {
        let server = crate::test_support::GuardedMockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                let tools = b.matches("\"role\":\"tool\"").count();
                let nudges = b.matches("ended without a tool call and without a final answer").count();
                nudges < tools * 2 + 2
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "tool_calls", 100, 50));
        });
        let _work = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                let tools = b.matches("\"role\":\"tool\"").count();
                let nudges = b.matches("ended without a tool call and without a final answer").count();
                nudges >= tools * 2 + 2
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([valid_read_call("call_trivial", "/workspace/a.txt")])),
                "tool_calls",
                100,
                20,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("stall-ratio").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];
        let cfg = compaction::CompactionConfig::never_compact();

        // A turn cap well above the expected escalation point, present ONLY
        // so a regression ends this test instead of running long. A run that
        // REACHES it has failed: production leaves `max_turns` unset.
        let outcome = run(
            &client,
            &client,
            "test-model",
            initial,
            &tools,
            &mut traj,
            false,
            &cfg,
            Some(20),
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::EmptyToolCallsExhausted),
            "two stalls per productive turn nets +1 on the budget each cycle and must \
             reach the bound; reaching the turn cap instead means the pay-down is a reset \
             rather than a decay, and the loop tolerates a 67%-useless model forever: {:?}",
            outcome.terminal_reason
        );
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 5,
            "the second stall of the second cycle is the one that finds the budget already \
             at {MAX_STALL_RECOVERIES}"
        );
    }

    /// (#2229 round-2 blocker 2) A checkpoint continuation is the SAME
    /// logical turn resuming, so it must not erase a recovery that same turn
    /// already spent.
    ///
    /// The reset lives in the `"tool_calls"` arm's `else` (a dispatchable
    /// call survived the #2169 partition), which reads as turn-granular and
    /// is not: after `recover_intra_turn_stall`, the length arm sets
    /// `resuming_after_checkpoint = turn.has_prefill()`, and the top of the
    /// next iteration then does NOT increment `turns` — the next API call is
    /// the same logical turn. If that call returns a real tool call it lands
    /// on the reset and zeroes a budget the length arm spent moments earlier
    /// inside the very same turn.
    ///
    /// The scenario, with calls 1–3 all being turn 1:
    ///
    /// ```text
    /// turn1 call1: length + content at the cap → checkpoint, banks a prefill
    /// turn1 call2: length + EMPTY             → spends a recovery (0→1)
    /// turn1 call3: tool_calls + a real call   → the reset site
    /// turn2      : length + EMPTY             → 1→2
    /// turn3      : length + EMPTY             → 2 >= 2, escalate
    /// ```
    ///
    /// Turn-granular, that escalates on turn 3. With the reset firing on the
    /// mid-turn continuation it escalates on turn 4 instead, having silently
    /// refunded turn 1's own recovery — so the recorded turn count is what
    /// discriminates.
    #[test]
    #[serial_test::serial]
    fn a_checkpoint_continuation_does_not_erase_a_recovery_from_the_same_turn() {
        let server = crate::test_support::GuardedMockServer::start();
        // Distinct tokens so the accumulation is never degenerate (the
        // degeneracy gate would otherwise abandon the prefill and take
        // `has_prefill()` — and with it the continuation — away). The
        // PREFILLMARK sentinel is what tells call 2's matcher apart from
        // call 1's: both see zero tool messages and zero nudges, and only
        // call 2 sees the banked prefill in the request body.
        let thought: String = format!(
            "<think>\nPREFILLMARK {}",
            (0..60).map(|i| format!("step{i}")).collect::<Vec<_>>().join(" ")
        );
        let _open = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
                    && b.matches("ended without a tool call and without a final answer").count() == 0
                    && !b.contains("PREFILLMARK")
            });
            then.status(200)
                .json_body(chat_response_json(Some(&thought), None, "length", 100, 200));
        });
        let _stall_same_turn = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
                    && b.matches("ended without a tool call and without a final answer").count() == 0
                    && b.contains("PREFILLMARK")
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "length", 100, 200));
        });
        let _dispatch_same_turn = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
                    && b.matches("ended without a tool call and without a final answer").count() == 1
            });
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([valid_read_call("call_same_turn", "/workspace/a.txt")])),
                "tool_calls",
                100,
                20,
            ));
        });
        // Every call after the tool dispatch: a plain useless stall.
        let _later_stalls = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() >= 1
            });
            then.status(200)
                .json_body(chat_response_json(None, None, "length", 100, 200));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("stall-midturn").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("think then act")];
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
            Some(200),
            Some(200),
            std::collections::BTreeMap::new(),
            None,
        )
        .expect("must not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "three useless `length` stalls against a budget of {MAX_STALL_RECOVERIES} must \
             escalate on the runaway-reasoning kind: {:?}",
            outcome.terminal_reason
        );
        assert_eq!(
            crate::trajectory::recorded(tmp.path()).turns(), 3,
            "turn 1 spent a recovery at its second call; the THIRD call of that same turn \
             is a checkpoint continuation, not a new turn, so dispatching a tool there must \
             not refund it. Escalating on turn 4 means it did."
        );
    }

    /// (#2190) A GENUINE runaway-reasoning cut (`finish_reason=length`, no
    /// content, no tool_calls — the shape `EmptyToolCallsExhausted` was
    /// split OUT of) must still produce the OLD kind,
    /// `IntraTurnStallExhausted`. This is the negative case for the split
    /// above: same recovery mechanism, different escalation reason,
    /// discriminated purely by which finish_reason produced the stall.
    #[test]
    #[serial_test::serial]
    fn genuine_reasoning_bound_cut_still_produces_intra_turn_stall_kind() {
        let server = crate::test_support::GuardedMockServer::start();
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
        let tmp = tempfile::Builder::new().prefix("genuine-reasoning-stall").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, Some(10_000), Some(1000), std::collections::BTreeMap::new(), None)
            .expect("genuine runaway-reasoning must recover+escalate, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::IntraTurnStallExhausted),
            "a finish_reason=length stall must keep the OLD kind (IntraTurnStallExhausted); \
             got {:?}",
            outcome.terminal_reason
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
        let server = crate::test_support::GuardedMockServer::start();
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
                // (#2165 CONSIDER item 5) This is the dispatch's very first
                // call (`dispatch_has_reasoned` still false) — it carries
                // the answer bound, not the reasoning interval.
                assert_eq!(
                    v.get("bound").and_then(|b| b.get("kind")).cloned(),
                    Some(serde_json::json!("max_tokens_per_call")),
                    "the dispatch's very first call carries the answer bound, got {v:?}"
                );
            }
        }
        assert!(
            found_recovered,
            "trajectory must contain dispatch.intra_turn_stall.recovered event"
        );
    }

    /// (#2190) Sibling of `loop_emits_intra_turn_stall_recovered_trajectory_event`
    /// above, for the NEW event: an empty-`tool_calls` recovery must emit
    /// `dispatch.empty_tool_calls.recovered`, NOT
    /// `dispatch.intra_turn_stall.recovered`. Red-proved by hand: reverting
    /// the arm's trajectory call back to `append_intra_turn_stall_recovered`
    /// makes `found_empty_tool_calls_recovered` stay false while
    /// `found_intra_turn_stall_recovered` goes true — this test would then
    /// fail on the first assertion.
    #[test]
    #[serial_test::serial]
    fn loop_emits_empty_tool_calls_recovered_trajectory_event() {
        let server = crate::test_support::GuardedMockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(|req| {
                    let body = req.body.as_deref().and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
                    !body.contains("darkmux-runtime] Your previous response")
                });
            then.status(200).json_body(chat_response_json(None, None, "tool_calls", 100, 50));
        });
        let _stop = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("darkmux-runtime] Your previous response");
            then.status(200).json_body(chat_response_json(Some("recovered"), None, "stop", 150, 10));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("empty-toolcalls-traj").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let _outcome = run(&client, &client, "test-model", initial, &tools, &mut traj, false, &cfg, Some(100), None, None, None, std::collections::BTreeMap::new(), None)
            .expect("recovery succeeds");

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let mut found_empty_tool_calls_recovered = false;
        let mut found_intra_turn_stall_recovered = false;
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            match v.get("type").and_then(|t| t.as_str()) {
                Some("dispatch.empty_tool_calls.recovered") => {
                    found_empty_tool_calls_recovered = true;
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
                Some("dispatch.intra_turn_stall.recovered") => {
                    found_intra_turn_stall_recovered = true;
                }
                _ => {}
            }
        }
        assert!(
            found_empty_tool_calls_recovered,
            "trajectory must contain dispatch.empty_tool_calls.recovered for the empty-array shape"
        );
        assert!(
            !found_intra_turn_stall_recovered,
            "an empty-tool_calls recovery must NOT also emit the runaway-reasoning event kind"
        );
    }

    /// (#2190) The escalation record (`dispatch.escalation.triggered`) must
    /// carry `model` and the prompt-token count AT THE MOMENT OF ESCALATION
    /// — the whole point being "which model, at what context, stopped
    /// producing calls" is answerable from this one line. Exercises the
    /// empty-tool-calls escalation path (the exact shape #2190's live
    /// evidence hit), but the trajectory call is shared by every
    /// `EscalationTriggered` return site.
    #[test]
    #[serial_test::serial]
    fn escalation_triggered_record_carries_model_and_prompt_tokens() {
        let server = crate::test_support::GuardedMockServer::start();
        let _stall = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(None, None, "tool_calls", 19133, 648));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempfile::Builder::new().prefix("escalation-model-context").tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("ask")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::never_compact();
        let outcome = run(
            &client, &client, "devstral-small-2-2512", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
        )
        .expect("empty-tool-calls exhaustion returns Ok(EscalationTriggered)");
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::EmptyToolCallsExhausted)
        );

        let traj_path = tmp.path().join(".darkmux-runtime/trajectory.jsonl");
        let raw = std::fs::read_to_string(&traj_path).expect("trajectory file exists");
        let mut found = false;
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            if v.get("type").and_then(|t| t.as_str()) == Some("dispatch.escalation.triggered") {
                found = true;
                assert_eq!(
                    v.get("reason").and_then(|r| r.as_str()),
                    Some("escalation_empty_tool_calls"),
                    "reason must be the exact envelope `result` string, got {v:?}"
                );
                assert_eq!(
                    v.get("model").and_then(|m| m.as_str()),
                    Some("devstral-small-2-2512"),
                    "escalation record must name the model, got {v:?}"
                );
                assert_eq!(
                    v.get("prompt_tokens").and_then(|p| p.as_u64()),
                    Some(19133),
                    "escalation record must carry the prompt-token count AT THE TIME, got {v:?}"
                );
            }
        }
        assert!(found, "trajectory must contain dispatch.escalation.triggered");
    }

    /// (#2190) `runtime.max_stall_recoveries` is a real knob, not a doc
    /// comment: setting the override to 1 escalates ONE recovery earlier
    /// than the built-in default (2), and setting it to 4 tolerates two
    /// MORE recoveries before escalating. Both call `run_with_sleeper`
    /// directly (the frozen `run()` wrapper always passes `None` — see its
    /// own doc), same pattern the generation-check-in override tests use.
    #[test]
    #[serial_test::serial]
    fn max_stall_recoveries_override_changes_the_escalation_point() {
        use crate::checkpoint;

        for (budget, expected_turns) in [(1u32, 2u32), (4u32, 5u32)] {
            let server = crate::test_support::GuardedMockServer::start();
            let _stall = server.mock(|when, then| {
                when.method(POST).path("/v1/chat/completions");
                then.status(200).json_body(chat_response_json(None, None, "tool_calls", 100, 50));
            });

            let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
            let tmp = tempfile::Builder::new().prefix("stall-budget-override").tempdir().unwrap();
            let mut traj = Trajectory::open(tmp.path());
            let initial = vec![Message::system("test"), Message::user("ask")];
            let tools = [Tool::Read];
            let cfg = compaction::CompactionConfig::never_compact();

            let outcome = run_with_sleeper(
                &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
                Some(100), None, None, None, None, Some(budget),
                std::collections::BTreeMap::new(), None, tmp.path(), "test-role",
                None::<checkpoint::RunCheckpoint>, &RealSleeper,
            )
            .expect("budget exhaustion returns Ok(EscalationTriggered)");

            assert_eq!(
                outcome.terminal_reason,
                TerminalReason::EscalationTriggered(EscalationReason::EmptyToolCallsExhausted),
                "budget={budget}: expected EmptyToolCallsExhausted, got {:?}",
                outcome.terminal_reason
            );
            assert_eq!(
                crate::trajectory::recorded(tmp.path()).turns(), expected_turns,
                "budget={budget}: expected exactly budget+1 turns (=={expected_turns}); got {}",
                crate::trajectory::recorded(tmp.path()).turns()
            );
        }
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

    /// (#3074) A budget of `0` means UNBOUNDED, so there is no kill to warn
    /// about: the soft threshold can never be reached.
    #[test]
    fn soft_threshold_for_an_unbounded_budget_is_never_reached() {
        assert_eq!(inactivity_soft_threshold_secs(0), u64::MAX);
    }

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
        let server = crate::test_support::GuardedMockServer::start();
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
            outcome.as_ref().map(|o| (&o.terminal_reason, crate::trajectory::recorded(tmp.path()).turns())).map_err(|e| e.to_string())
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

#[path = "loop_deciders.rs"]
mod loop_deciders;

#[path = "loop_phases.rs"]
mod loop_phases;

#[cfg(test)]
#[path = "checkpoint_regression_tests.rs"]
mod checkpoint_regression_tests;

#[cfg(test)]
#[path = "tool_writing_tests.rs"]
mod tool_writing_tests;

#[cfg(test)]
#[path = "loop_characterization_tests.rs"]
mod loop_characterization_tests;

#[cfg(test)]
#[path = "messages_cache_prefix_tests.rs"]
mod messages_cache_prefix_tests;
