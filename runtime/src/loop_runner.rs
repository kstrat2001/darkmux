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

use crate::checkpoint;
use crate::compaction;
use crate::cycle_detector::{CycleDetector, CycleSignal};
use crate::failure_rate::{FailureCascadeSignal, FailureRateDetector};
use crate::feedback::FeedbackInjector;
use crate::lmstudio::{ChatRequest, ChunkAccumulator, LmStudioClient, Message, ToolCall};
use crate::pace;
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

    /// (#1221) The turn's ANSWER text when the loop exits with a checkpoint
    /// prefill still pending.
    ///
    /// `main.rs` derives the deliverable as "the last assistant message", which
    /// is the prefill on every non-terminal exit (escalation, cumulative cap,
    /// max turns) — so the operator got the model's raw scratch work as its
    /// answer. The loop knows which region is the answer; nothing downstream
    /// should have to infer it from message order or delimiters.
    pub final_answer: Option<String>,
    /// Number of compaction events that fired during the loop.
    /// Phase 6: middle-replace via the companion compactor model.
    pub compactions: u32,

    /// (#2094) Sum of every inter-turn rest this dispatch took, in
    /// milliseconds — the AFTER-clamp duration actually slept. `wall_ms`
    /// (computed by the caller from `trajectory.elapsed_ms()`) INCLUDES
    /// this time; a caller wanting model-only time subtracts `rest_ms`.
    pub rest_ms: u64,
    /// (#2094) How many inter-turn rests fired during this dispatch.
    pub rests: u32,
    /// (#2094 finding 8) The POST-CLAMP `turn_delay_ms` this dispatch
    /// actually applied — i.e. `resolve_turn_delay_ms`'s output, not the
    /// operator's raw configured value. Distinct from `rest_ms`/`rests`
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

/// (#2114 finding 7) The pace-file pause wait, extracted so it can be
/// called from BOTH the main loop's turn-boundary check AND the resume
/// catch-up pass (`run_with_sleeper`'s pre-loop block) — a resume into an
/// active thermal pause must not barrel through its undispatched tool
/// calls before honoring it. Blocks in ≤2s increments, re-reading the
/// pace file each increment; returns once the file says `pause: false`,
/// is absent/malformed, or has expired past `max_pause_ms` (see
/// `PaceFile::is_expired` — a stale stamp is treated as abandoned).
#[allow(clippy::too_many_arguments)]
fn honor_pace_pause(
    pace_reader: &mut pace::PaceReader,
    out_dir: &std::path::Path,
    max_pause_ms: u64,
    pace_expiry_warned: &mut bool,
    sleeper: &dyn TurnSleeper,
    trajectory: &mut Trajectory,
    turns: u32,
    rest_ms: &mut u64,
    rests: &mut u32,
    last_proof_of_work: &mut std::time::Instant,
    inactivity_soft_warning_fired_in_window: &mut bool,
) {
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
        sleeper.sleep(PACE_POLL_INCREMENT_MS);
        *rest_ms = rest_ms.saturating_add(PACE_POLL_INCREMENT_MS);
        *rests = rests.saturating_add(1);
        trajectory.append_paced_rest(turns, PACE_POLL_INCREMENT_MS, &reason);
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
    fn carried(&self) -> &str {
        if self.writing_thought() {
            self.thought.trim()
        } else {
            self.answer.trim()
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
fn recover_intra_turn_stall(
    messages: &mut Vec<Message>,
    trajectory: &mut Trajectory,
    turns: u32,
    completion_tokens: Option<u32>,
    stall_recoveries_used: &mut u32,
    nudge: &str,
) {
    messages.pop();
    *stall_recoveries_used = stall_recoveries_used.saturating_add(1);
    trajectory.append_intra_turn_stall_recovered(
        turns,
        completion_tokens,
        *stall_recoveries_used,
        MAX_STALL_RECOVERIES,
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
/// checkpoint explicitly. This one is `#[allow(dead_code)]` because
/// nothing in the non-test binary calls it anymore, but it stays public
/// and exercised because the bulk of this file's test suite still calls
/// it and shouldn't have to care about a feature it isn't testing.
///
/// (#2114 finding 3) Each call gets its OWN fresh host tempdir as its
/// out-dir rather than a hardcoded path: this fn only ever runs on the
/// TEST host (never inside the container `main.rs` drives), where neither
/// `/darkmux-out` nor the old `/workspace` exist as writable paths. Before
/// this, every turn-boundary checkpoint write inside a `run()`-based test
/// failed and logged — 1,230 "failed to write checkpoint" lines across the
/// 124 tests that exercise this wrapper, noise that could mask a real
/// failure. Hand-rolled (no `tempfile` — that crate is dev-only, and this
/// fn compiles in the release binary) with a PID+nanos+counter suffix so
/// parallel test threads (same PID) never collide; the dir is left behind
/// in the OS temp root rather than cleaned up, same tradeoff `dispatch_
/// internal.rs`'s `host_out` makes for a real dispatch's out-dir.
#[allow(dead_code)]
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
    // (#2114) A resumed dispatch replaces the fresh `initial_messages` (the
    // system prompt + first user turn main.rs built) with the checkpoint's
    // own history — the checkpoint already carries whatever system/user
    // messages opened the ORIGINAL dispatch, so re-seeding from scratch
    // would duplicate them.
    let resume_seed = resume_from;
    let mut messages = match &resume_seed {
        Some(ckpt) => ckpt.messages.clone(),
        None => initial_messages,
    };
    // (#1221) Resolve the per-call cap once; every use below (the request's
    // max_tokens, cap-salvage detection, the budget snapshot, the length-arm
    // diagnostics) reads this so an override stays consistent end-to-end.
    // (#1221) The per-call cap is a CHECKPOINT INTERVAL, not a ceiling — it is
    // constant, and what changes at each checkpoint is whether the reasoning is
    // handed back open (continue) or closed (conclude).
    let answer_max_tokens: u32 = max_tokens_per_call.unwrap_or(MAX_TOKENS_PER_CALL);
    let reasoning_interval: u32 =
        reasoning_checkpoint_interval.unwrap_or(REASONING_CHECKPOINT_INTERVAL);
    // Assigned from the turn's region at the top of every iteration, before
    // the request that carries it is built; the length arm reads it back.
    let mut per_call_cap: u32;
    let mut checkpoints_used: u32 = 0;
    // Set when the previous iteration handed a turn back as a prefill; read by
    // the turn counter so the resumed call is not counted as a new turn.
    // (#2114) A resume whose checkpoint carried a pending #1221 hand-back
    // starts the SAME way: the loop's next request continues that turn
    // rather than opening a new one.
    let mut resuming_after_checkpoint = resume_seed
        .as_ref()
        .is_some_and(|c| c.pending_hand_back.is_some());
    // (#1221) The turn currently in flight: its two output regions and the
    // prefill message that carries them back to the model. See `TurnAccum` —
    // these were six loose `let mut`s mutated at seven sites, and the two
    // defects that cost were both "cleared the state, left the message".
    // (#2114) Seeded from the checkpoint's `pending_hand_back` on a resume so
    // a continuation resumes the SAME accumulation instead of an empty one;
    // `prefill_at` points at the LAST message, which `messages` (seeded
    // above from the same checkpoint) already carries as its final prefill.
    let mut turn = match resume_seed.as_ref().and_then(|c| c.pending_hand_back.as_ref()) {
        Some(hb) => TurnAccum {
            thought: hb.thought.clone(),
            answer: hb.answer.clone(),
            think_closed: hb.think_closed,
            is_reasoning: hb.is_reasoning,
            carries_own_opener: hb.carries_own_opener,
            prefill_at: if messages.is_empty() { None } else { Some(messages.len() - 1) },
        },
        None => TurnAccum::default(),
    };
    // (#2164) Dispatch-scoped: has this model shown a reasoning region on
    // ANY call so far, across every turn. NOT part of `TurnAccum` — that
    // struct's `is_reasoning` is only ever touched by `absorb()`, which
    // itself only runs for a turn that has already been checkpointed once
    // (see `in_answer_region`'s doc). A turn that completes cleanly in one
    // call — the modal case — never reaches `absorb()` at all, so a
    // TurnAccum-resident flag would stay false forever even for a model
    // that reasons on every turn. This is derived directly from each
    // response's extracted reasoning instead (see `per_turn_reasoning`
    // below), independent of the region machine's own bookkeeping.
    //
    // Resume seeding is conservative, not exact: `PendingHandBack` carries
    // only the RESUMING turn's own `is_reasoning`, not a dispatch-wide
    // fact — a dispatch that reasoned on an earlier, already-CONCLUDED
    // turn and later got checkpointed mid-ANSWER on a different turn would
    // resume with this false, and pay one extra turn's answer-bound first
    // call before re-proving itself. That is the same one-call lag #1221's
    // own follow-up already measured as small; carrying the exact fact
    // through the checkpoint file would need a schema bump for a rare
    // resume-time edge case, so it is not done here.
    let mut dispatch_has_reasoned: bool = resume_seed
        .as_ref()
        .and_then(|c| c.pending_hand_back.as_ref())
        .map(|hb| hb.is_reasoning)
        .unwrap_or(false);
    let tool_defs: Vec<_> = tools.iter().map(|t| t.to_tool_def()).collect();
    // Set of tool names the model is allowed to call. Drives the
    // plain-text-tool-call promoter (#406): any tool name in the
    // promoted markup that isn't here is rejected so adversarial /
    // malformed output can't smuggle arbitrary tool names into the
    // dispatch pipeline.
    let allowed_tool_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();

    // (#2114) Resumed counters pick up exactly where the checkpoint left
    // off; a fresh dispatch starts all four at zero as before.
    let mut turns: u32 = resume_seed.as_ref().map(|c| c.turns).unwrap_or(0);
    let mut total_prompt_tokens: u32 = resume_seed.as_ref().map(|c| c.total_prompt_tokens).unwrap_or(0);
    let mut total_completion_tokens: u32 =
        resume_seed.as_ref().map(|c| c.total_completion_tokens).unwrap_or(0);
    let mut compactions: u32 = resume_seed.as_ref().map(|c| c.compactions).unwrap_or(0);
    // (#2094) Sum + count of the inter-turn rests taken this dispatch.
    let mut rest_ms: u64 = resume_seed.as_ref().map(|c| c.rest_ms).unwrap_or(0);
    let mut rests: u32 = resume_seed.as_ref().map(|c| c.rests).unwrap_or(0);
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
    // (#2164) One-shot latch: has the runtime already recorded, for THIS
    // dispatch, that a call carrying real output produced no reasoning at
    // all. Fires once, the first time it becomes true, so the run record
    // explains why the reasoning check-in bound stopped applying to fresh
    // turns' first calls — without repeating the same line on every later
    // turn of a model that simply never reasons.
    let mut no_reasoning_region_logged = false;
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
    // (#887, superseded by #1222 shakedown-3) #887 verified the host
    // watchdog reset only on tool.completed + compaction, not on
    // `model.partial`. #1222 shakedown-3 changed that: two legitimate
    // long-reasoning dispatches were killed mid-generation because only
    // those two signals reset the HOST's deadline, so `model.partial` is
    // now a proof-of-work signal there too (`dispatch_internal.rs`'s
    // `"model.partial"` heartbeat arm). #2114 finding 5 brings the
    // runtime's OWN soft-inactivity clock in line — `run_streaming_turn`
    // resets `last_proof_of_work` on every chunk it ingests, the same
    // signal the host now trusts. A mid-stream SOFT nudge still isn't
    // actionable (can't inject into an in-progress generation) — the
    // reset just keeps the soft clock from drifting stale relative to the
    // hard one, not a claim that a nudge could fire mid-stream.
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

    // (#2114) Reads + parses `pace.json` on demand (not once at startup
    // like `turn_delay_ms` below — the pace file is meant to change
    // mid-dispatch); tracks whether a malformed sighting has already been
    // warned about so a broken writer doesn't spam stderr once per 2s
    // poll.
    let mut pace_reader = pace::PaceReader::new();
    // (#2114 finding 4) Ceiling past which a held `pause: true` is treated
    // as abandoned rather than honored forever — read once at startup,
    // same pattern as `inactivity_budget_secs` above.
    let max_pause_ms = pace::max_pause_ms();
    // Edge-trigger so the staleness warning fires once per abandoned-pause
    // episode, not once per turn boundary while the same stale file sits
    // there.
    let mut pace_expiry_warned = false;

    // (#2094) Global inter-turn rest — read once at startup, same pattern
    // as `inactivity_budget_secs` above (both are host-forwarded env vars
    // the container reads exactly once). Clamped below the inactivity
    // timeout so the operator's own pacing knob can never become the thing
    // that trips the watchdog; a clamp fires a loud warning naming both
    // numbers.
    let turn_delay_ms: u64 = std::env::var("DARKMUX_TURN_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (turn_delay_ms, turn_delay_warning) =
        resolve_turn_delay_ms(turn_delay_ms, inactivity_budget_secs);
    if let Some(w) = &turn_delay_warning {
        eprintln!("{w}");
    }

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

    // (#2114 finding 2) A resumed dispatch whose checkpoint captured an
    // IN-PROGRESS tool-call batch (a kill between tool N and tool N+1 of
    // the same turn) finishes dispatching the REMAINING calls before the
    // main loop ever requests a new completion — so a resume never
    // re-runs a tool call whose result the checkpoint already recorded
    // (see `RunCheckpoint::pending_tool_calls`'s doc for the ONE exception:
    // the single call in flight at kill time). `messages` (seeded above
    // from the same checkpoint) already carries the assistant's
    // tool_calls message plus every result recorded before the kill; this
    // dispatches exactly what's left, appending results the same way the
    // main loop's `tool_calls` arm does, and checkpointing after each one
    // so a SECOND kill during this catch-up pass loses no more than the
    // main loop's own per-tool checkpoint already guarantees.
    //
    // Deliberately NOT wired into the cycle/failure-rate/cadence
    // detectors or the feedback-injection queue below — `RunCheckpoint::
    // pending_tool_calls`'s own doc names detector state as reset-fresh
    // on resume rather than restored. This is a resume-time catch-up
    // pass, not a re-entry into the live loop's full bookkeeping; the gap
    // is tracked residue on #2114, not an oversight.
    if let Some(pending_calls) = resume_seed.as_ref().and_then(|c| c.pending_tool_calls.clone()) {
        // (#2114 finding N6) `tool_seq` for the FIRST pending call picks up
        // exactly where the killed run left off, so the SAME call gets the
        // SAME `tool_seq` in `trajectory.jsonl` whether it's seen from the
        // original run or from this catch-up.
        let seq_base = resume_seed.as_ref().map(|c| c.pending_tool_calls_seq_base).unwrap_or(0);
        let total_pending = pending_calls.len();
        for (idx, call) in pending_calls.iter().cloned().enumerate() {
            // (#2114 finding N7) Honor an active pace pause BEFORE every
            // catch-up dispatch — including the very first — so a resume
            // into a live thermal pause doesn't barrel through its
            // undispatched tool calls (often the most expensive ones,
            // since they're what got the dispatch killed in the first
            // place) before the governor's hold takes effect.
            if !resuming_after_checkpoint {
                honor_pace_pause(
                    &mut pace_reader,
                    out_dir,
                    max_pause_ms,
                    &mut pace_expiry_warned,
                    sleeper,
                    trajectory,
                    turns,
                    &mut rest_ms,
                    &mut rests,
                    &mut last_proof_of_work,
                    &mut inactivity_soft_warning_fired_in_window,
                );
            }
            let tool_seq = seq_base + idx as u32;
            let result = dispatch(&call.function.name, &call.function.arguments);
            let outcome = crate::failure_rate::classify_outcome(&call.function.name, &result);
            let tool_ok = outcome.tool_worked();
            if let Some(reason) =
                crate::failure_rate::classify_failed_to_run(&call.function.name, &result)
            {
                let command = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                    .ok()
                    .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(str::to_string))
                    .unwrap_or_else(|| call.function.arguments.clone());
                failed_to_run.push(FailedExec { command, reason: reason.to_string() });
            }
            trajectory.append_tool_completed(
                turns,
                tool_seq,
                &call.function.name,
                &call.function.arguments,
                &result,
                &outcome,
            );
            if tool_ok {
                last_proof_of_work = std::time::Instant::now();
                inactivity_soft_warning_fired_in_window = false;
            }
            messages.push(Message::tool_result(call.id, call.function.name, result));

            let remaining: Vec<ToolCall> = pending_calls[idx + 1..].to_vec();
            let remaining_is_empty = remaining.is_empty();
            let snapshot = checkpoint::RunCheckpoint {
                schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
                role_id: role_id.to_string(),
                messages: messages.clone(),
                turns,
                total_prompt_tokens,
                total_completion_tokens,
                compactions,
                rest_ms,
                rests,
                pending_hand_back: None,
                pending_tool_calls: if remaining_is_empty { None } else { Some(remaining) },
                pending_tool_calls_seq_base: if remaining_is_empty { 0 } else { tool_seq + 1 },
                written_at_unix_ms: checkpoint::unix_ms(),
            };
            if let Err(e) = checkpoint::write_checkpoint(out_dir, &snapshot) {
                eprintln!(
                    "darkmux-runtime: ⚠ failed to write checkpoint: {e} (continuing without one)"
                );
            }
        }

        // (#2114 finding N1) The same soft-trim + compaction check the
        // main loop's `tool_calls` arm runs right after ITS tool-dispatch
        // loop, applied here too — otherwise a resume whose catch-up pass
        // just appended a batch of large tool results sails straight into
        // the FIRST post-resume request oversized, with neither the trim
        // nor the compaction check that would have caught it on a live
        // (never-killed) run. No real `latest_prompt_tokens` exists yet
        // at this point (no request has been sent in this process), so
        // this always uses the local chars/4 estimate rather than the
        // main loop's reported-vs-estimate staleness gate — there's
        // nothing to compare the estimate against yet.
        let trim_stats = crate::tool_result_prune::soft_trim_old_tool_results(&mut messages);
        if trim_stats.results_trimmed > 0 {
            eprintln!(
                "darkmux-runtime: soft-trimmed {} old tool result(s), reclaiming {} bytes \
                 of transcript before the post-resume compaction check (#1391/#2114)",
                trim_stats.results_trimmed, trim_stats.bytes_reclaimed
            );
        }
        let (sys_chars, prompt_chars) = measure_request_context(&messages);
        let resume_estimate_tokens = ((sys_chars + prompt_chars) / 4) as u32;
        if compaction::needs_compaction(resume_estimate_tokens, messages.len(), compaction_cfg) {
            let before_count = messages.len();
            compactions = compactions.saturating_add(1);
            let summary_chars = match compaction_cfg.strategy {
                compaction::CompactionStrategy::Narrative => compaction::compact(
                    compactor_client,
                    &mut messages,
                    compactions,
                    compaction_cfg,
                )?,
                compaction::CompactionStrategy::StructuredSlot => {
                    let budget = compaction::BudgetSnapshot {
                        turns_used: turns,
                        max_turns,
                        cumulative_completion_tokens_used: total_completion_tokens,
                        max_cumulative_completion_tokens: max_cumulative_tokens,
                        max_tokens_per_call: answer_max_tokens,
                    };
                    let (parsed, summary_chars) = compaction::structured_compact(
                        compactor_client,
                        &mut messages,
                        compactions,
                        compaction_cfg,
                        Some(budget),
                    )?;
                    persist_structured_compaction_output(
                        &crate::trajectory::runtime_dir(),
                        compactions,
                        &parsed,
                    );
                    summary_chars
                }
            };
            let after_count = messages.len();
            let (sys_chars_after, prompt_chars_after) = measure_request_context(&messages);
            let tokens_after = ((sys_chars_after + prompt_chars_after) / 4) as u32;
            trajectory.append_compaction(
                compactions,
                before_count,
                after_count,
                summary_chars,
                resume_estimate_tokens,
                tokens_after,
            );
            eprintln!(
                "darkmux-runtime: compacted after the resume catch-up pass ({before_count} → \
                 {after_count} messages) before the first post-resume request. (#2114)"
            );

            // (#2114 finding 1) Resume-compaction parity with the main
            // loop's `tool_calls` arm (~:2680-2722): a compaction here is
            // the SAME event with the SAME consequences, whichever site
            // triggered it. Queue the same post-compaction feedback nudge,
            // reset proof-of-work + the soft-warning flag the same way,
            // and run the SAME `bail_after_compactions` escalation check
            // — without this, a resume that immediately compacts past the
            // operator's bound would silently send one more request
            // instead of escalating to the frontier the way a live
            // (never-killed) run in the identical position would.
            feedback_injector.queue_post_compaction(turns);
            last_proof_of_work = std::time::Instant::now();
            inactivity_soft_warning_fired_in_window = false;
            if let Some(bail) = compaction_cfg.bail_after_compactions {
                if compactions >= bail {
                    eprintln!(
                        "darkmux-runtime: escalation_triggered — \
                         compactions ({compactions}) reached bail_after_compactions ({bail}) \
                         during resume catch-up; emitting EscalationTriggered terminal for \
                         frontier handoff instead of requesting the next turn. (#2114)"
                    );
                    return Ok(LoopOutcome {
                        final_answer: turn.pending_answer(),
                        terminal_reason: TerminalReason::EscalationTriggered(
                            EscalationReason::CompactionLimitReached,
                        ),
                        messages,
                        turns,
                        total_prompt_tokens,
                        total_completion_tokens,
                        compactions,
                        rest_ms,
                        rests,
                        turn_delay_effective_ms: turn_delay_ms,
                        failed_to_run: failed_to_run.clone(),
                    });
                }
            }
        }

        eprintln!(
            "darkmux-runtime: ▶ resumed mid-turn — dispatched the {total_pending} tool call(s) \
             remaining from turn {turns} before requesting the next turn. (#2114)"
        );
    }

    loop {
        // (#2114) Sleep-safe deadline re-anchor. This runtime's
        // `last_proof_of_work: Instant` lives inside the Docker Desktop
        // Linux VM, whose clock behavior across a HOST macOS sleep is
        // UNVERIFIED (see the commit message this landed in for what was
        // and wasn't checked). If that clock kept advancing through a host
        // suspend — or the loop simply went a very long real-world time
        // between iterations for any other reason a live model call
        // wouldn't produce — `elapsed_secs` jumps far past even the FULL
        // inactivity budget in a single top-of-loop check (a live,
        // responsive loop's soft-check runs every iteration, so it would
        // have already fired the soft warning well before 2x the budget
        // elapsed). Treat that jump as a suspected sleep/wake: re-anchor to
        // now (the same "extend, don't reset-and-lose-context" shape
        // `absorb_rest_into_soft_inactivity_clock` uses for a rest) rather
        // than let a stale multi-hour "elapsed" number cascade into
        // repeated soft warnings once the loop resumes.
        {
            let elapsed_secs = last_proof_of_work.elapsed().as_secs();
            // (#2114 finding 5) Gated on `!inactivity_soft_warning_fired_
            // in_window` — the doc above (and `is_suspected_sleep_wake_
            // jump`'s own doc) already claims a genuine stall would have
            // fired the smaller soft warning BEFORE reaching 2x the full
            // budget, so a jump WITH the warning already fired is a real
            // stall that simply never got its proof-of-work reset, not a
            // suspected sleep/wake — re-anchoring that case would erase a
            // legitimate stall signal instead of correcting a clock.
            if turns > 0
                && !inactivity_soft_warning_fired_in_window
                && is_suspected_sleep_wake_jump(elapsed_secs, inactivity_budget_secs)
            {
                eprintln!(
                    "darkmux-runtime: ⚠ suspected host sleep/wake — {elapsed_secs}s elapsed \
                     since the last proof-of-work signal, more than 2x the {inactivity_budget_secs}s \
                     inactivity budget in a single loop iteration. Re-anchoring the deadline \
                     instead of treating this as a stall."
                );
                last_proof_of_work = std::time::Instant::now();
                inactivity_soft_warning_fired_in_window = false;
            }
        }

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
                    final_answer: turn.pending_answer(),
                    terminal_reason: TerminalReason::MaxTurns,
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                    rest_ms,
                    rests,
                    turn_delay_effective_ms: turn_delay_ms,
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
                    final_answer: turn.pending_answer(),
                    terminal_reason: TerminalReason::EscalationTriggered(
                        EscalationReason::CumulativeTokensExceeded,
                    ),
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                    rest_ms,
                    rests,
                    turn_delay_effective_ms: turn_delay_ms,
                    failed_to_run: failed_to_run.clone(),
                });
            }
        }

        // (#2094) Global inter-turn rest — GPU thermal/power relief between
        // inference bursts. Fires here: AFTER this turn's tool results were
        // appended (or after the terminal-return checks above bailed, in
        // which case this line never runs at all — no rest on a dispatch
        // that's about to end) and BEFORE the next chat request is built.
        //
        // Two guards, both load-bearing:
        // - `turns > 0` — never rest before the FIRST request; there is no
        //   prior turn to have rested "between."
        // - `!resuming_after_checkpoint` — a checkpoint continuation is the
        //   SAME logical turn resuming (see `resuming_after_checkpoint`'s
        //   own doc above), not a turn boundary; the model is still
        //   actively mid-thought and this is not "between turns."
        if turns > 0 && !resuming_after_checkpoint && turn_delay_ms > 0 {
            sleeper.sleep(turn_delay_ms);
            rest_ms = rest_ms.saturating_add(turn_delay_ms);
            rests = rests.saturating_add(1);
            trajectory.append_rest(turns, turn_delay_ms);
            // (#2094 finding 3b) Harness-owned time, not a stall: EXTEND
            // (never reset to "now") the soft-inactivity clock by exactly
            // the rest duration, and clear the edge-trigger flag so a
            // fresh rest buys a fresh chance before the next soft warning
            // — mirrors the tool.completed/compaction proof-of-work resets
            // elsewhere in this loop. Both effects are bundled in
            // `absorb_rest_into_soft_inactivity_clock` (see its own doc)
            // so this call site can't apply one half without the other.
            (last_proof_of_work, inactivity_soft_warning_fired_in_window) =
                absorb_rest_into_soft_inactivity_clock(last_proof_of_work, turn_delay_ms);
        }

        // (#2114 finding 1) Turn-boundary checkpoint — persists enough
        // state (messages, budget counters, compaction count, and any
        // pending #1221 hand-back) that a killed container — forced host
        // sleep, docker restart, a thermal breaker at the hard floor —
        // can resume from here (`--resume <path>`) instead of restarting
        // the whole dispatch. Written at every loop-top past the first
        // request, INCLUDING mid-#1221-continuation boundaries:
        // `pending_hand_back` captures the live accumulation on those, so
        // a kill mid checkpoint-sequence still has something recent to
        // resume from. Streaming (write-every-turn), not end-of-run — a
        // killed container leaves the LAST completed one, never nothing.
        // Best-effort: a write failure is logged and the dispatch
        // continues (losing resumability, not progress).
        //
        // MUST run BEFORE the pace-file pause check below: while parked
        // at a pause boundary N, the pause loop can hold for an arbitrary
        // amount of real time, and a checkpoint written AFTER it would
        // still be showing turn N-1's state for that whole window — a
        // kill during a long pause would resume one turn behind where the
        // dispatch actually is. Written first, the on-disk checkpoint is
        // never stale while parked.
        if turns > 0 {
            let pending_hand_back = if resuming_after_checkpoint {
                Some(checkpoint::PendingHandBack {
                    thought: turn.thought.clone(),
                    answer: turn.answer.clone(),
                    think_closed: turn.think_closed,
                    is_reasoning: turn.is_reasoning,
                    carries_own_opener: turn.carries_own_opener,
                })
            } else {
                None
            };
            let snapshot = checkpoint::RunCheckpoint {
                schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
                role_id: role_id.to_string(),
                messages: messages.clone(),
                turns,
                total_prompt_tokens,
                total_completion_tokens,
                compactions,
                rest_ms,
                rests,
                pending_hand_back,
                pending_tool_calls: None,
                pending_tool_calls_seq_base: 0,
                written_at_unix_ms: checkpoint::unix_ms(),
            };
            if let Err(e) = checkpoint::write_checkpoint(out_dir, &snapshot) {
                eprintln!(
                    "darkmux-runtime: ⚠ failed to write checkpoint: {e} (continuing without one)"
                );
            }
        }

        // (#2114 finding 8) Host-driven pause file — checked at the SAME
        // turn-boundary point as the rest above, guarded only by
        // `!resuming_after_checkpoint` (a #1221 continuation is the same
        // logical turn resuming, not a boundary between turns) — NOT by
        // `turns > 0`: a governor that wants the very first request held
        // back (e.g. a thermal ceiling already tripped before this
        // dispatch was even launched) can do so, the same as at any later
        // boundary. While `pace.json` holds `pause: true` the loop rests
        // in BOUNDED ≤2s increments, re-reading the file each increment —
        // never a single long sleep — so a pace flip from pause back to
        // resume is picked up within one increment, and so a long pause
        // never trips the inactivity detector: every increment is
        // proof-of-work through the SAME `absorb_rest_into_soft_inactivity_clock`
        // #2094's turn_delay rest uses.
        //
        // (#2114 finding 4) A pause is honored only while FRESH: once
        // `written_at_ms` falls more than `max_pause_ms` behind now, the
        // loop stops honoring it (logs once, falls through to the next
        // request) rather than resting forever — each rest increment
        // resets the HOST-side inactivity deadline too (see
        // `absorb_rest_into_soft_inactivity_clock`), so an unbounded
        // honored pause would make the container immortal against its own
        // watchdog. A killed or hung governor process can never hold a
        // dispatch past this ceiling.
        if !resuming_after_checkpoint {
            honor_pace_pause(
                &mut pace_reader,
                out_dir,
                max_pause_ms,
                &mut pace_expiry_warned,
                sleeper,
                trajectory,
                turns,
                &mut rest_ms,
                &mut rests,
                &mut last_proof_of_work,
                &mut inactivity_soft_warning_fired_in_window,
            );
        }

        // Pick the bound BEFORE building the request that carries it. This
        // sat after the struct literal, so every request went out with the
        // PREVIOUS iteration's value — the switch to the answer bound always
        // lagged one full call. Measured: reasoning=50 / answer=5000 produced
        // max_tokens 50, 50, 5000, 5000, so a non-reasoning turn was still
        // checkpointed at the small reasoning interval for one extra call,
        // which is exactly the case the split exists to prevent.
        //
        // (#2164) `in_answer_region()` alone is not enough: it reads `false`
        // for BOTH a genuine mid-thought continuation AND a brand-new turn
        // that has absorbed nothing yet — the two are indistinguishable by
        // that function alone. Applying the reasoning bound to the second
        // case truncated a non-reasoning model's very first tool-call batch
        // at the 1000-token check-in interval regardless of how large it
        // was; the #479 salvage then dispatched only the well-formed prefix
        // and nudged the model to "reduce its reasoning" — an instruction it
        // was never disobeying. `dispatch_has_reasoned` breaks the tie: it
        // is dispatch-scoped (updated every response, never reset), so a
        // turn's first call carries the reasoning bound only once THIS
        // dispatch has actually proven, on some earlier call, that it
        // reasons. Before that — including the dispatch's very first call —
        // the first call of every turn carries the answer bound instead,
        // same as an already-answering turn does. A thinking model checks
        // in one call later on its very first turn (the cost #1221's own
        // follow-up measured as small); a non-reasoning model's tool-call
        // batches are never capped by an interval meant to bound reasoning,
        // not answers.
        let carries_reasoning_bound = !turn.in_answer_region() && dispatch_has_reasoned;
        per_call_cap = if carries_reasoning_bound {
            reasoning_interval
        } else {
            answer_max_tokens
        };
        // (#1959) Which bound this request carried, captured HERE.
        //
        // Consumers downstream cannot re-derive it from `turn`: `absorb` has
        // not run for this turn yet when the salvage check fires, so the
        // region state still describes the PREVIOUS turn. A first attempt read
        // `turn.writing_thought()` at the salvage site and silently never
        // suppressed anything.
        let sent_reasoning_bound = carries_reasoning_bound;
        // (#2164) There is no separate `turns`-based guard here — the
        // detector below fires as soon as ONE call's response confirms it:
        // real dispatchable output produced with no reasoning at all. That
        // can be this dispatch's very first call (a non-reasoning model's
        // turn 1, the modal case this fix targets) and is not gated on
        // having "already produced a turn" first. What DOES prevent a
        // false positive on a genuinely thinking model is upstream of this
        // point, not a turns count: `dispatch_has_reasoned` (and, via it,
        // the detector's own `!dispatch_has_reasoned` condition below) is
        // computed from THIS call's own response — including reasoning
        // carried in the separate `reasoning_content` field, which
        // `promote_terminal_reasoning` strips before this line but returns
        // to its caller for exactly this reason. See the detector's
        // firing site below for the actual condition.

        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: Some(per_call_cap),
            response_format: response_format.clone(),
        };

        // (#1221) `turns` has not been incremented yet, so a FRESH turn is
        // `turns + 1` — but a checkpoint continuation does not increment at
        // all, and stamping it `turns + 1` gave `model.partial` a sequence one
        // ahead of the `model.completed` it pairs with. Streaming is the
        // production default, so that mismatch is what the viewer normally
        // reads: every continuation's partials filed under a turn that does
        // not exist yet.
        let next_seq = if resuming_after_checkpoint { turns } else { turns + 1 };
        let mut response = if streaming {
            run_streaming_turn(
                client,
                &request,
                next_seq,
                trajectory,
                &mut last_proof_of_work,
                &mut inactivity_soft_warning_fired_in_window,
            )?
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
            // (#1221) A new logical turn is a new thought, so the previous
            // turn's accumulation goes — AND the prefill message that carried
            // it, which is the half this used to forget. Clearing the index
            // alone orphaned a raw `<think>` block in history that nothing
            // could reconstruct an answer from, so `main.rs` handed that markup
            // over as the deliverable. `begin` does both or neither.
            turn.begin(&mut messages);
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
        //
        // (#2164) Its return value is the ONLY place `reasoning_content` is
        // still visible after this call for a tool_calls/stop turn — the
        // strip above wipes `choice.message.reasoning_content` before
        // `per_turn_reasoning` is assembled below, so a caller reading the
        // message field after this point sees nothing. Captured here and
        // folded into `dispatch_has_reasoned`'s decision alongside
        // `per_turn_reasoning`.
        let mut separate_field_reasoning_before_strip: Option<String> = None;
        if let Some(choice) = response.choices.first_mut() {
            let finish = choice.finish_reason.clone();
            separate_field_reasoning_before_strip =
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
        // (#2164) Dispatch-scoped "has this model ever reasoned" — the ONE
        // place `dispatch_has_reasoned` is set, and it only ever moves
        // false→true, never back. Three shapes count as reasoning: a
        // completed block (`per_turn_reasoning`, covers a closed inline
        // `<think>` via `extract_think_blocks` above, PLUS the separate
        // `reasoning_content` field on the ONE finish reason — "length" —
        // where `promote_terminal_reasoning` does not strip it before this
        // point runs); the separate `reasoning_content` field on every
        // OTHER finish reason, which `promote_terminal_reasoning` already
        // cleared from `assistant_message` before this line, so it can only
        // be read from `separate_field_reasoning_before_strip` — the value
        // captured at that call site, before the strip; and an INLINE block
        // this call OPENED but has not closed yet (the truncated-mid-
        // first-call shape `extract_think_blocks` deliberately skips, since
        // it requires a matched pair — mirrors the same delimiter check
        // `TurnAccum::absorb` uses for the identical shape on a
        // continuation call).
        if !per_turn_reasoning.trim().is_empty()
            || separate_field_reasoning_before_strip
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
        {
            dispatch_has_reasoned = true;
        } else if let Some(content) = assistant_message.content.as_deref() {
            let trimmed = content.trim_start();
            let opener = crate::budget_request::THINK_OPEN.trim();
            let closer = crate::budget_request::THINK_CLOSE.trim();
            if trimmed.starts_with(opener) && trimmed.matches(opener).count() > trimmed.matches(closer).count() {
                dispatch_has_reasoned = true;
            }
        }
        // (#2164) Captured HERE, before salvage or any other mutation below
        // clears `content` — whether THIS call produced real, dispatchable
        // output (an answer or tool calls), independent of whether it also
        // reasoned. Feeds the one-shot "this model emits no reasoning
        // region" detector after the response is fully processed.
        let call_had_dispatchable_output = assistant_message
            .content
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty())
            || assistant_message
                .tool_calls
                .as_ref()
                .is_some_and(|t| !t.is_empty());
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
        // (#1959) Compared against what we SENT on this request, deliberately.
        //
        // This is not asking "is that a big number." It asks: did WE cut this
        // turn, or did the context window? A `length` finish BELOW our own cap
        // means overflow, which is a different and fatal condition. So the
        // comparison has to be against whatever `max_tokens` this request
        // actually carried — `per_call_cap`, the region value.
        //
        // A first attempt at the bug below keyed this to `answer_max_tokens`
        // instead. That looked right and was worse: a reasoning turn cut at the
        // check-in interval stops being recognized as our-cut, and its
        // well-formed tool calls get dropped rather than dispatched.
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
            // (#1959) Only when the ANSWER budget was the thing that ran out.
            //
            // This nudge tells the model to reduce its per-call reasoning. On a
            // turn that genuinely blew a 10000-token answer budget that is
            // useful. Fired on every routine 1000-token reasoning CHECK-IN — as
            // it was, once #1221 made `per_call_cap` region-dependent — it
            // breaks the invariant this feature was most careful about: the
            // model is never told a checkpoint happened.
            //
            // That is not a style rule. Measured during #1221: a model invited
            // to wrap up wraps up, producing a tidy summary with ZERO findings
            // where the same model uninterrupted found real ones. A nudge to
            // "reduce your reasoning" arriving every thousand tokens is that
            // same instruction on a loop.
            if !sent_reasoning_bound {
                feedback_injector
                    .queue_per_turn_cap_approach(observed_tokens, per_call_cap);
            }
            // Clear truncated content — keep tool_calls. Mirrors the
            // stall-arm's pop reason: anchoring + prompt-token bloat.
            assistant_message.content = None;
            // (#1959) DROP the tool call the cap cut in half.
            //
            // The cap lands mid-serialization, so the LAST call in a salvaged
            // turn is routinely truncated to `arguments: ""`. Counting the
            // well-formed ones for the log above is not the same as dispatching
            // only those, and until this line the message went out whole: the
            // empty call executed, failed, and — the part that actually hurts —
            // stayed in the conversation, where `arguments: ""` is not valid
            // JSON. LMStudio answered the NEXT streaming request with HTTP 500
            // and the dispatch died outright.
            //
            // Observed live: a crawl emitted five `read` calls at the cap, four
            // with arguments and one with none. It ran 67s and returned no
            // envelope. A partial call carries no recoverable intent — half a
            // path is not a narrower path — so dropping it loses nothing the
            // model cannot simply re-issue on the turn it is about to get.
            retain_well_formed_tool_calls(&mut assistant_message);
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
        //
        // The deliverable is assembled from the regions, never recovered by
        // searching for a delimiter. `rfind("</think>")` was wrong twice over:
        // it truncated an answer that merely QUOTED the delimiter (which is
        // what a reviewer of this very file writes), and under
        // `response_format` the model cannot emit one at all, so it found
        // nothing and handed the raw thought over as the answer.
        // (#1959) TERMINAL means the turn is over — not merely that the
        // finish reason is no longer the string "length".
        //
        // A per-turn-cap SALVAGE rewrites the reason to `tool_calls` so the
        // recovered calls get dispatched, but the turn is still mid-flight: the
        // model will be called again with the tool results. Folding there is
        // wrong twice over. It ends the accumulation early, and it writes the
        // whole accumulated body into `assistant_message.content` — the field
        // salvage had just deliberately set to `None` to avoid anchoring the
        // model on truncated output and inflating every later prompt.
        //
        // The result reaching the provider was an assistant message carrying a
        // large content blob AND seven tool calls, followed by seven tool
        // results. Measured: the next request returned HTTP 500.
        let turn_is_over = effective_finish_reason != "length" && !salvaged_per_turn_cap;
        if turn_is_over {
            turn.fold(&mut messages, &mut assistant_message);
        } else if salvaged_per_turn_cap {
            // Mid-turn: the prefill is superseded by the message about to be
            // pushed, but the accumulation lives on. Removing the message
            // WITHOUT folding is the whole distinction — see `supersede`.
            turn.supersede(&mut messages);
        }

        messages.push(assistant_message.clone());

        // (#2164) One-shot detector: this dispatch has, cumulatively, never
        // shown a reasoning region — and this call, which produced real
        // dispatchable output, didn't either. Surfaced once so the run
        // record explains why the reasoning check-in bound stops applying
        // to fresh turns' first calls (`dispatch_has_reasoned` above),
        // rather than leaving that inference to whoever reads the
        // trajectory later.
        if !no_reasoning_region_logged
            && !dispatch_has_reasoned
            && per_turn_reasoning.trim().is_empty()
            && call_had_dispatchable_output
        {
            eprintln!(
                "darkmux-runtime: this model has produced no reasoning region \
                 in {turns} turn(s) so far — the reasoning check-in bound no \
                 longer applies to a fresh turn's first call; only the answer \
                 bound does. (#2164)"
            );
            trajectory.append_reasoning_bound_not_applied(turns);
            no_reasoning_region_logged = true;
        }

        match effective_finish_reason {
            "stop" => {
                return Ok(LoopOutcome {
                    final_answer: turn.pending_answer(),
                    terminal_reason: TerminalReason::Stop,
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                    rest_ms,
                    rests,
                    turn_delay_effective_ms: turn_delay_ms,
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
                        // (#1221) Drop the empty message before handing off.
                        // `main.rs` takes the LAST assistant message as the
                        // deliverable, and this arm pushes a wholly empty one
                        // every time it fires — so escalating with it still in
                        // place buries the turn's real work behind a blank
                        // message and the operator receives nothing. The
                        // recovery below already drops it; the escalation
                        // returned before reaching that.
                        if messages
                            .last()
                            .map(|m| {
                                m.role == "assistant"
                                    && m.content.as_deref().map(|c| c.trim().is_empty()).unwrap_or(true)
                                    && m.tool_calls.as_ref().map(|t| t.is_empty()).unwrap_or(true)
                            })
                            .unwrap_or(false)
                        {
                            messages.pop();
                        }
                        eprintln!(
                            "darkmux-runtime: escalation_triggered — finish_reason=\
                             tool_calls with no tool_calls, and the intra-turn stall \
                             recovery budget ({MAX_STALL_RECOVERIES}) is exhausted; \
                             {stall_recoveries_used} prior recoveries didn't break the \
                             pattern. Emitting EscalationTriggered for frontier handoff."
                        );
                        return Ok(LoopOutcome {
                            final_answer: turn.pending_answer(),
                            terminal_reason: TerminalReason::EscalationTriggered(
                                EscalationReason::IntraTurnStallExhausted,
                            ),
                            messages,
                            turns,
                            total_prompt_tokens,
                            total_completion_tokens,
                            compactions,
                            rest_ms,
                            rests,
                            turn_delay_effective_ms: turn_delay_ms,
                            failed_to_run: failed_to_run.clone(),
                        });
                    }
                    // (#1221) Pop ONLY a genuinely useless message. This arm is
                    // reached AFTER the terminal fold has written the turn's
                    // whole accumulation into this very message, so an
                    // unconditional pop deletes every banked checkpoint — and
                    // the fold has already cleared the region state, so nothing
                    // can reconstruct it and `pending_answer` returns None on
                    // every later exit. Proven by a review probe: a turn that
                    // banked a 200-item first chunk, then got the wholly-empty
                    // `tool_calls: []` shape, then concluded, delivered
                    // `"Done."` and nothing else.
                    //
                    // The #1123 shape this recovery was built for is a WHOLLY
                    // EMPTY message, which pops exactly as before. A message
                    // carrying real text is not a useless completion; the model
                    // still gets the budget spent and the nudge, but its work
                    // stays in history as ordinary assistant text.
                    let useless = messages
                        .last()
                        .map(|m| {
                            m.content.as_deref().map(|c| c.trim().is_empty()).unwrap_or(true)
                        })
                        .unwrap_or(false);
                    if useless {
                        messages.pop();
                    }
                    stall_recoveries_used = stall_recoveries_used.saturating_add(1);
                    trajectory.append_intra_turn_stall_recovered(
                        turns,
                        this_turn_completion_tokens,
                        stall_recoveries_used,
                        MAX_STALL_RECOVERIES,
                    );
                    messages.push(Message::system(STALL_NUDGE_MESSAGE));
                    let kept = if useless { "Dropped the useless turn" } else { "KEPT the turn's work" };
                    eprintln!(
                        "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} \
                         returned finish_reason=tool_calls with no tool_calls. {kept}, \
                         injected a nudge; budget \
                         {stall_recoveries_used}/{MAX_STALL_RECOVERIES} used. (#1123/#1221)"
                    );
                    continue;
                }

                // Dispatch each call; append a `tool` message per
                // result so the next request shows the model exactly
                // what each tool returned. Trajectory records each
                // tool.completed event so the operator can see what
                // ran post-dispatch.
                //
                // (#2114 finding 2) `calls_snapshot` stays around (the
                // loop below consumes `calls` itself) so each iteration
                // can compute exactly which calls are still UNDISPATCHED
                // after it and stamp that onto a checkpoint — see the
                // write at the bottom of this loop body.
                let calls_snapshot = calls.clone();
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
                    // (#469/#2008) Classify with the same function the
                    // failure-rate detector uses, and record it on the
                    // trajectory event so the host watchdog can gate its
                    // deadline reset.
                    //
                    // `tool_ok` is TOOL success, which is what the `ok` field
                    // has always documented itself as meaning — so a command
                    // that ran and reported non-zero (a red test) is `true`
                    // here. It did work: the watchdogs should count it, and
                    // the cascade should not.
                    let outcome =
                        crate::failure_rate::classify_outcome(&call.function.name, &result);
                    let tool_ok = outcome.tool_worked();
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
                        &result,
                        &outcome,
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
                        reason,
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
                            &reason,
                        );
                    }
                    messages.push(Message::tool_result(
                        call.id,
                        call.function.name,
                        result,
                    ));

                    // (#2114 finding 2) Per-tool-result checkpoint. Written
                    // after EVERY tool result, not just at the turn
                    // boundary above `calls`'s dispatch loop — a kill
                    // between tool N and tool N+1 of a multi-tool turn
                    // previously lost N's completed result entirely (the
                    // only checkpoint was the loop-top one, written before
                    // this loop even started). `messages` here already
                    // carries the assistant's tool_calls message plus
                    // every result recorded so far; `pending_tool_calls`
                    // names the calls from THIS turn not yet dispatched —
                    // `None` once the last one lands, matching a clean
                    // boundary. See the pre-loop resume block (top of
                    // `run_with_sleeper`) for the other half: dispatching
                    // exactly these calls, and none already recorded,
                    // when a resumed checkpoint carries them.
                    let remaining: Vec<ToolCall> = calls_snapshot[tool_seq + 1..].to_vec();
                    let remaining_is_empty = remaining.is_empty();
                    let mid_turn_snapshot = checkpoint::RunCheckpoint {
                        schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
                        role_id: role_id.to_string(),
                        messages: messages.clone(),
                        turns,
                        total_prompt_tokens,
                        total_completion_tokens,
                        compactions,
                        rest_ms,
                        rests,
                        pending_hand_back: None,
                        pending_tool_calls: if remaining_is_empty { None } else { Some(remaining) },
                        // (#2114 finding N6) A fresh (non-resumed) turn's
                        // calls always start at tool_seq 0, so the next
                        // pending call's seq is simply tool_seq + 1.
                        pending_tool_calls_seq_base: if remaining_is_empty { 0 } else { tool_seq as u32 + 1 },
                        written_at_unix_ms: checkpoint::unix_ms(),
                    };
                    if let Err(e) = checkpoint::write_checkpoint(out_dir, &mid_turn_snapshot) {
                        eprintln!(
                            "darkmux-runtime: ⚠ failed to write checkpoint: {e} (continuing without one)"
                        );
                    }
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
                                final_answer: turn.pending_answer(),
                                terminal_reason: TerminalReason::EscalationTriggered(
                                    EscalationReason::CompactionLimitReached,
                                ),
                                messages,
                                turns,
                                total_prompt_tokens,
                                total_completion_tokens,
                                compactions,
                                rest_ms,
                                rests,
                                turn_delay_effective_ms: turn_delay_ms,
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
                // (#1221) An ABSENT `usage` object means "we cannot tell", and
                // the safe reading of "cannot tell" at a `length` finish is a
                // CAP HIT, not a context overflow. `is_some_and` returned false
                // for unknown, which routed straight into the hard `Err` below
                // — and that `Err` kills the whole dispatch, so `main.rs` emits
                // `result: "error"` with no envelope, no metrics and no
                // deliverable. Every banked checkpoint goes with it.
                //
                // This matters far more on this branch than before it: the
                // per-call bound dropped from 10,000 to a 1,000-token
                // checkpoint interval, so the population reaching this boundary
                // went from rare to a measured 43-50% of turns. darkmux's own
                // local path sets `stream_options.include_usage`, so LMStudio
                // reports it — but a hosted endpoint or proxy that ignores that
                // flag would make every checkpointed dispatch fatal.
                //
                // The overflow diagnosis needs a MEASURED token count below the
                // cap; without one there is nothing to diagnose from.
                let cap_cliff = this_turn_completion_tokens
                    .map(|t| t.saturating_add(1) >= per_call_cap)
                    .unwrap_or(true);
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
                        final_answer: turn.pending_answer(),
                        terminal_reason: TerminalReason::EscalationTriggered(
                            EscalationReason::IntraTurnStallExhausted,
                        ),
                        messages,
                        turns,
                        total_prompt_tokens,
                        total_completion_tokens,
                        compactions,
                        rest_ms,
                        rests,
                        turn_delay_effective_ms: turn_delay_ms,
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
                // Route the slice into the region it belongs to. Nothing is
                // inserted between slices: the model was cut mid-sentence and
                // resumes at exactly that character, so any separator would
                // land inside a word. See `TurnAccum::absorb` for why the
                // ordering of the three shapes matters.
                turn.absorb(&per_turn_reasoning, content_slice);
                // What gets handed back is the WHOLE accumulation, never one
                // slice. That is also the scope the degeneracy gate needs: a
                // model re-treading ground from three checkpoints ago produces
                // slices that each look locally novel, so judging one slice in
                // isolation cannot see the cycle it exists to catch.
                let writing_thought = turn.writing_thought();
                let carried = turn.carried().to_string();
                // Emptiness is a property of THIS call, not of the turn. Testing
                // the accumulation meant that once a turn had checkpointed once
                // it could never be empty again, so the classic null-emission
                // runaway could never reach the drop-and-nudge branch a second
                // time. Measured: 41,383 checkpoints in 20s with
                // `Recovery budget 0/2` on every line, escalation unreachable.
                let this_call_produced_nothing =
                    per_turn_reasoning.trim().is_empty() && content_slice.trim().is_empty();
                if this_call_produced_nothing {
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
                    // Checkpointing cannot help with THIS call — there is
                    // nothing in it to resume. But the accumulation from
                    // earlier checkpoints is untouched work, so it stays: the
                    // prefill is not abandoned, and the turn does not end.
                    // Probed live — a system message after a prefill does NOT
                    // break continuation, so the nudge can sit behind it.
                    recover_intra_turn_stall(
                        &mut messages,
                        trajectory,
                        turns,
                        this_turn_completion_tokens,
                        &mut stall_recoveries_used,
                        STALL_NUDGE_MESSAGE,
                    );
                    // Only a turn with nothing banked is a fresh start; one
                    // mid-accumulation keeps going, or the empty call would
                    // cost every checkpoint before it.
                    resuming_after_checkpoint = turn.has_prefill();
                    eprintln!(
                        "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} hit                          the boundary with an EMPTY completion, so there is nothing to                          resume. Dropped the useless turn, injected a nudge; budget                          {stall_recoveries_used}/{MAX_STALL_RECOVERIES} used. (#1123/#1221)"
                    );
                } else {
                    checkpoints_used = checkpoints_used.saturating_add(1);
                    // Only judge while the thought is still open. After the
                    // close the accumulation is reasoning PLUS the answer being
                    // written, and its ratio stays low forever — judging it
                    // would re-fire the verdict on every checkpoint.
                    // Judge the region being written, ALWAYS — including after
                    // the thought is closed.
                    //
                    // This used to return `None` once closed, justified by "the
                    // accumulation is reasoning PLUS the answer, so its ratio
                    // stays low forever". That was simply false: `carried()`
                    // returns the ANSWER ALONE once the thought is closed — the
                    // reasoning is not in the judged slice at all. The
                    // measurement the gate was disabled to avoid does not
                    // exist, and disabling it left the post-close answer region
                    // with no gate whatsoever.
                    //
                    // Measured by a review probe: after a forced conclude, a
                    // model repeating in the answer region ran 337 checkpoints
                    // with no terminal reached, every line reading `turn 1` and
                    // `Recovery budget 0/2`. Under default config the only
                    // backstop is the host's 600s SIGKILL — which produces NO
                    // envelope, so every banked checkpoint is lost too.
                    let tail_ratio = crate::reasoning_loop::tail_repetition_ratio(
                        &carried,
                        crate::reasoning_loop::TAIL_WINDOW_TOKENS,
                        crate::reasoning_loop::tail_sample_tokens(reasoning_interval),
                    );
                    // Degeneracy DETECTION applies to any output; only the
                    // remedy differs. An earlier cut gated the detection itself
                    // on `turn_is_reasoning`, which left repeating plain content
                    // with no gate at all — it checkpointed forever, and the
                    // pre-existing intra-turn stall escalation that used to
                    // bound exactly that shape became unreachable.
                    let degenerate =
                        crate::reasoning_loop::slice_is_degenerate(&carried, reasoning_interval);
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
                    // belongs to the operator's existing, CONFIGURABLE e-stops.
                    //
                    // Which of those actually reach THIS shape, stated
                    // precisely, because an earlier version of this comment
                    // named `runtime.max_turns` and that was simply false:
                    //
                    //   `runtime.max_turns`  does NOT bound a checkpointing
                    //       turn. Continuations are the same logical turn by
                    //       design, so the counter never moves. Naming it here
                    //       told a reader a bound existed where none did.
                    //   `runtime.max_tokens` (cumulative) DOES bound it, and is
                    //       the right knob — checkpointing spends tokens, which
                    //       is exactly what it meters. It defaults to unset
                    //       (uncapped), so it bounds this only for an operator
                    //       who set it.
                    //   the inactivity budget bounds it only as an absolute
                    //       600s SIGKILL: a checkpoint is not a proof-of-work
                    //       signal, so the timer never resets on one. That is a
                    //       HARD kill — no conclusion, no envelope.
                    //
                    // So under default config the only backstop is that hard
                    // kill. Deliberately left as-is rather than inventing a
                    // checkpoint ceiling here, which is the thing this change
                    // exists to remove — but it is a real gap, and the fix
                    // belongs at the config layer (a default for
                    // `runtime.max_tokens`), not in this gate. Tracked
                    // separately.
                    // A degenerate turn that never opened a thought has no
                    // delimiter to close, so it does not get a prefill at all —
                    // it goes back to the recovery path that already owns this
                    // shape.
                    // (#1221) The remedy DIFFERS by region, and conflating them
                    // trades one defect for another.
                    //
                    // A degenerate ANSWER after we already forced a conclude is
                    // a model that will not converge. Bound it — but do NOT
                    // abandon the accumulation the way the no-thought path
                    // does. Measured separately: legitimate repetitive ANSWER
                    // shapes score far below the threshold (an enum-valued JSON
                    // array, a block of identical match arms, an ASCII table
                    // frame, a checklist with an invariant line all land near
                    // 0.003), so deleting on this verdict would destroy real
                    // work on output that is repetitive by nature rather than
                    // by pathology. Hand it off instead, with everything banked
                    // still attached.
                    if degenerate && !writing_thought {
                        eprintln!(
                            "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — the ANSWER \
                             region is repeating and there is no thought left to close. \
                             Escalating for handoff with everything banked so far ATTACHED. \
                             (#1221)"
                        );
                        return Ok(LoopOutcome {
                            final_answer: turn.pending_answer(),
                            terminal_reason: TerminalReason::EscalationTriggered(
                                EscalationReason::IntraTurnStallExhausted,
                            ),
                            messages,
                            turns,
                            total_prompt_tokens,
                            total_completion_tokens,
                            compactions,
                            rest_ms,
                            rests,
                            turn_delay_effective_ms: turn_delay_ms,
                            failed_to_run: failed_to_run.clone(),
                        });
                    }
                    // Everything reaching here is either a clean continue or a
                    // degenerate THOUGHT. The old third branch — abandon the
                    // accumulation, spend a recovery unit, nudge — is gone: it
                    // deleted real work on a verdict the metric gets wrong for
                    // whole classes of legitimate output. Measured on realistic
                    // answer shapes at the shipped threshold: an enum-valued
                    // JSON array 0.003, a block of identical match arms 0.003,
                    // an ASCII table frame 0.003, a checklist with an invariant
                    // line 0.002 — all "degenerate", none pathological. A
                    // review probe drove that path with an 11 KB first chunk
                    // and the operator received "Done."
                    //
                    // Repetition is a reason to STOP, never a reason to DELETE.
                    if degenerate && writing_thought {
                        eprintln!(
                            "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — the reasoning is \
                             repeating (degeneracy gate); closing the thought so the model \
                             answers from what it has. (#1221)"
                        );
                        // The close is written INTO the accumulation, so every
                        // later checkpoint keeps handing back a thought that is
                        // already closed followed by the answer so far, and
                        // everything after it is the ANSWER region.
                        turn.close_thought();
                    } else if turn.think_closed {
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
                    // invented around it. Either way the truncated raw response
                    // goes and the prefill REPLACES its predecessor, so the
                    // thread carries ONE growing assistant message rather than
                    // a chain of restarts.
                    messages.pop();
                    turn.hand_back(&mut messages);
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
        *last_proof_of_work = std::time::Instant::now();
        *inactivity_soft_warning_fired_in_window = false;
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
        tcs.retain(|tc| serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok());
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
    fn register_three_turn_tool_then_stop_script(server: &httpmock::MockServer) {
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
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::set_var("DARKMUX_TURN_DELAY_MS", "500");

        let server = MockServer::start();
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(outcome.turns, 3, "sanity: three logical turns");
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [500, 500],
            "rests fire BETWEEN turns only — 2 rests for 3 turns, never before the first"
        );
        assert_eq!(outcome.rest_ms, 1000, "LoopOutcome carries the same sum the sleeper saw");
        assert_eq!(outcome.rests, 2);
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

        let server = MockServer::start();
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok even though it paused mid-run");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(
            outcome.turns, 3,
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
        let server = MockServer::start();
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
            total_prompt_tokens: 100,
            total_completion_tokens: 20,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
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

        let server = MockServer::start();
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", None, &RealSleeper,
        )
        .expect("2-turn scripted dispatch (tool call, then stop) returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(outcome.turns, 2, "sanity: a tool-call turn followed by a stop turn");

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

        let server = MockServer::start();
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}" },
        }]);
        // Turns 1 and 2 — a RESUMED dispatch must NEVER hit these; it
        // starts directly at the request a fresh dispatch would send as
        // its THIRD call.
        let tc1 = tool_calls.clone();
        let turn1_mock = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(None, Some(tc1.clone()), "tool_calls", 100, 20));
        });
        let tc2 = tool_calls.clone();
        let turn2_mock = server.mock(move |when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 1
            });
            then.status(200).json_body(chat_response_json(None, Some(tc2.clone()), "tool_calls", 120, 20));
        });
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
            total_prompt_tokens: 220,
            total_completion_tokens: 40,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("resumed dispatch returns Ok");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(
            outcome.turns, 3,
            "resumed at turns=2; one more call brings it to turn 3 and the loop stops"
        );
        turn1_mock.assert_hits(0);
        turn2_mock.assert_hits(0);
        turn3_mock.assert_hits(1);
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

        let server = MockServer::start();
        // A request shaped like the ORIGINAL turn 1 (no tool results yet)
        // must NEVER land — proves the resume doesn't re-request the
        // model for a turn it already has an assistant message for.
        let original_turn1_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions").matches(|req| {
                let b = req.body.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default();
                b.matches("\"role\":\"tool\"").count() == 0
            });
            then.status(200).json_body(chat_response_json(Some("should not be reached"), None, "stop", 100, 5));
        });
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
            total_prompt_tokens: 100,
            total_completion_tokens: 20,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2, call3]),
            // call_1 (index 0) already completed, so the next pending
            // call (call_2) resumes at tool_seq 1.
            pending_tool_calls_seq_base: 1,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
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

        let server = MockServer::start();
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
            total_prompt_tokens: 100,
            total_completion_tokens: 20,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2, call3]),
            pending_tool_calls_seq_base: 1,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
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

        let server = MockServer::start();
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
            total_prompt_tokens: 100,
            total_completion_tokens: 20,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![call2]),
            pending_tool_calls_seq_base: 1,
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
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

        let server = MockServer::start();
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
            total_prompt_tokens: 300,
            total_completion_tokens: 60,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![c2, c3]),
            pending_tool_calls_seq_base: 1,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-model", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
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
        use httpmock::prelude::*;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS"); // unset → 0 default

        let server = MockServer::start();
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
            Some(100), None, None, None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("3-turn scripted dispatch returns Ok");

        assert_eq!(outcome.turns, 3, "sanity: still three turns");
        assert!(sleeper.calls.borrow().is_empty(), "delay=0 must never sleep");
        assert_eq!(outcome.rest_ms, 0);
        assert_eq!(outcome.rests, 0);
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

        let server = MockServer::start();
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

        let outcome = run_with_sleeper(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("single-turn dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(outcome.turns, 1);
        assert!(
            sleeper.calls.borrow().is_empty(),
            "a single turn has no prior turn to rest AFTER — never before the first request"
        );
        assert_eq!(outcome.rest_ms, 0);
        assert_eq!(outcome.rests, 0);
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

        let server = MockServer::start();
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
            Some(100), None, None, Some(40), std::collections::BTreeMap::new(), None, tmp.path(), "test-role", None, &sleeper,
        )
        .expect("checkpoint-continuation scripted dispatch returns Ok");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
        assert_eq!(outcome.turns, 2, "sanity: two logical turns (the continuation is NOT a third)");
        assert_eq!(
            sleeper.calls.borrow().as_slice(),
            [500],
            "exactly ONE rest — between turn 1 and turn 2 — never a second one before \
             the checkpoint continuation call"
        );
        assert_eq!(outcome.rest_ms, 500);
        assert_eq!(outcome.rests, 1);
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
        let server = MockServer::start();
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
            outcome.turns, 1,
            "an empty call mid-accumulation does not start a new turn (turns={})",
            outcome.turns
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
        let server = MockServer::start();
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

        let outcome = run(
            &client, &client, "test-model", initial, &tools, &mut traj, false, &cfg,
            Some(100), None, Some(3000), Some(100), std::collections::BTreeMap::new(), None,
        )
        .expect("a fresh turn's first call, sent under the answer bound, dispatches cleanly");

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

    /// (#2114 finding 1) Resume-compaction parity: a checkpoint whose
    /// `compactions` already sits at `bail_after_compactions - 1` must
    /// escalate the INSTANT the resume catch-up pass's own compaction
    /// check pushes it over the bound — the same as a live (never-killed)
    /// dispatch would at the identical count. Before this fix, the resume
    /// catch-up's compaction path didn't run the `bail_after_compactions`
    /// check at all, so it would silently issue one MORE request past the
    /// operator's bound instead of escalating to the frontier.
    #[test]
    #[serial_test::serial]
    fn resume_catch_up_compaction_honors_bail_after_compactions() {
        use crate::lmstudio::{LmStudioClient, Message};
        use crate::tools::Tool;
        use crate::trajectory::Trajectory;

        std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        std::env::remove_var("DARKMUX_TURN_DELAY_MS");

        let cfg = compaction::CompactionConfig {
            // Low enough that the resume catch-up's local chars/4 estimate
            // trips it unconditionally once the message-count floor (7) is
            // met — isolates the bail check from needing a precise token
            // count.
            threshold_tokens: 1,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };

        let server = MockServer::start();
        // Must NEVER be hit — escalating means the resume never reaches
        // the main loop's first post-resume request at all.
        let primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(Some("should not be reached"), None, "stop", 100, 5));
        });
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
            total_prompt_tokens: 200,
            total_completion_tokens: 40,
            // (#2114 finding 1 test) bail_after_compactions - 1: the
            // catch-up's own compaction is the ONE that crosses the bound.
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: Some(vec![c2]),
            pending_tool_calls_seq_base: 1,
            written_at_unix_ms: checkpoint::unix_ms(),
        };

        let outcome = run_with_sleeper(
            &client, &client, "test-primary", vec![], &tools, &mut traj, false, &cfg,
            Some(100), None, None, None, std::collections::BTreeMap::new(), None,
            tmp.path(), "test-role", Some(resume_checkpoint), &RealSleeper,
        )
        .expect("bail should produce Ok with EscalationTriggered, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionLimitReached),
            "the resume catch-up's own compaction must escalate at the bound, not just the \
             main loop's"
        );
        assert_eq!(outcome.compactions, 1, "the bound-crossing compaction is counted");
        assert_eq!(compactor_mock.hits(), 1, "exactly one compactor call, during catch-up");
        primary_mock.assert_hits(0);
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
        let verdicts: Vec<String> = traj
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "dispatch.checkpoint")
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

#[cfg(test)]
#[path = "checkpoint_regression_tests.rs"]
mod checkpoint_regression_tests;
