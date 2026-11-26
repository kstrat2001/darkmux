//! (#3136) The agent loop's rules, one function each.
//!
//! `run_with_sleeper` used to hold every rule inline: one function, cyclomatic
//! complexity 151, whose every rule could only be tested by scripting a whole
//! dispatch against a mock server. Here each rule is a DECIDER: a pure
//! function from [`LoopState`] (plus whatever the last call observed) to the
//! one thing the loop should do next. No decider does I/O, reads a clock or
//! touches the message thread; the loop (`loop_phases.rs`) performs the effect
//! a decider names and folds the result back into the state.
//!
//! These are plain functions on THIS loop. They are deliberately not a
//! registry or a trait a mission opts into: the loop is one procedure, and its
//! rules are its own.
//!
//! Each decider's table test lives in `loop_deciders_tests.rs`. A change to
//! one rule should need only that rule's table plus the loop's
//! characterization tests (`loop_characterization_tests.rs`), which pin the
//! wiring end to end.

use super::*;

/// Every counter and flag the loop carries from one model call to the next.
///
/// Clocks are NOT here (`last_proof_of_work` is an `Instant`, and reading it
/// is I/O); a decider that needs elapsed time takes it as an argument. Neither
/// are the detectors, which keep their own windows, nor the message thread and
/// the turn's accumulation, which the effects mutate.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct LoopState {
    /// Logical turns started. A checkpoint continuation is the same turn and
    /// does not move it (#1221).
    pub(super) turns: u32,
    pub(super) total_completion_tokens: u32,
    /// Compactions that actually INSTALLED (#2792 merge-gate).
    pub(super) compactions: u32,
    /// The endpoint's prompt-token count for the last request that reported one.
    pub(super) latest_prompt_tokens: u32,
    // (#2792 round-4) The endpoint's own count paired with the characters it
    // counted, carried across turns so the next estimate only has to guess
    // what was ADDED since. `None` until the first response reports usage.
    pub(super) prompt_anchor: Option<PromptAnchor>,
    // (#854) Endpoint stale-token detection state. `prev_prompt_tokens` is the
    // prior turn's reported count; `frozen_prompt_turns` counts consecutive
    // turns it hasn't changed. When it sticks (the count can't gate compaction),
    // the loop substitutes a local size estimate for the compaction decision.
    pub(super) prev_prompt_tokens: Option<u32>,
    pub(super) frozen_prompt_turns: u32,
    // (#414 PR A) Per-dispatch budget for intra-turn stall recoveries.
    // Each occurrence of `finish_reason=length` with empty content +
    // no tool_calls (the classic Beat 47 / Run 1 runaway-reasoning
    // shape) consumes one slot. Exhausted budget escalates the
    // dispatch via `IntraTurnStallExhausted` so the operator/frontier
    // can intervene instead of burning more turns on the same stall.
    pub(super) stall_recoveries_used: u32,
    // (#2169 merge-gate finding 4) Consecutive-all-invalid-tool-call-turns
    // counter. Resets to 0 on any turn that dispatches at least one real
    // call; escalates via `MalformedToolCallsExhausted` at
    // `MAX_CONSECUTIVE_MALFORMED_TURNS`. Deliberately NOT seeded from
    // `resume_seed` — see the increment site's own doc for why.
    pub(super) consecutive_malformed_turns: u32,
    pub(super) checkpoints_used: u32,
    // (#2171) Reset every time a FRESH turn begins (never on a checkpoint
    // continuation of the same turn) — see the `turn.begin()` site. Counts
    // only continuations that were themselves generation-bound.
    pub(super) generation_continuations_this_turn: u32,
    // (#3074) Completion tokens this logical turn has generated across its
    // checkpoint continuations. Reset with the counter above; compared with
    // the context window, which every continuation's resent prefill must fit.
    pub(super) turn_completion_tokens: u32,
    // Set when the previous iteration handed a turn back as a prefill; read by
    // the turn counter so the resumed call is not counted as a new turn.
    // (#2114) A resume whose checkpoint carried a pending #1221 hand-back
    // starts the SAME way: the loop's next request continues that turn
    // rather than opening a new one.
    pub(super) resuming_after_checkpoint: bool,
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
    pub(super) dispatch_has_reasoned: bool,
    // (#2164) One-shot latch: has the runtime already recorded, for THIS
    // dispatch, that a call carrying real output produced no reasoning at
    // all. Fires once, the first time it becomes true, so the run record
    // explains why the reasoning check-in bound stopped applying to fresh
    // turns' first calls — without repeating the same line on every later
    // turn of a model that simply never reasons.
    pub(super) no_reasoning_region_logged: bool,
    /// (#466) Edge-trigger flag so the soft inactivity warning fires once per
    /// stuck window, not on every loop iteration.
    pub(super) soft_warning_fired: bool,
    /// (#465) The test-cadence drift detector's state: the most recently
    /// edited path and the same-file repetition counter.
    pub(super) last_edited_path: Option<String>,
    pub(super) consecutive_same_file_edits: u32,
}

impl LoopState {
    /// The counters a resumed dispatch restores from its checkpoint (#2114);
    /// a fresh dispatch starts every one at zero. Detector state is never
    /// restored (see `run_with_sleeper`'s note on why).
    pub(super) fn seeded(seed: Option<&checkpoint::RunCheckpoint>) -> Self {
        // (#2114) Resumed counters pick up exactly where the checkpoint left
        // off; a fresh dispatch starts all four at zero as before.
        let hand_back = seed.and_then(|c| c.pending_hand_back.as_ref());
        Self {
            turns: seed.map(|c| c.turns).unwrap_or(0),
            total_completion_tokens: seed.map(|c| c.total_completion_tokens).unwrap_or(0),
            compactions: seed.map(|c| c.compactions).unwrap_or(0),
            resuming_after_checkpoint: hand_back.is_some(),
            dispatch_has_reasoned: hand_back.map(|hb| hb.is_reasoning).unwrap_or(false),
            ..Self::default()
        }
    }

    /// Fold the start of a model call that returned. Returns whether it
    /// opened a new logical turn: a checkpoint continuation consumes the
    /// `resuming_after_checkpoint` latch instead, and spends no turn (#1221).
    pub(super) fn begin_call(&mut self) -> bool {
        if self.resuming_after_checkpoint {
            self.resuming_after_checkpoint = false;
            return false;
        }
        self.turns += 1;
        self.generation_continuations_this_turn = 0;
        self.turn_completion_tokens = 0;
        true
    }

    /// (B1) Fold a call's completion tokens: the endpoint's count, else the
    /// runtime's own estimate of a call it cut, else nothing.
    pub(super) fn fold_completion(&mut self, reported: Option<u32>, estimate: Option<u32>) {
        let spent = reported.or(estimate).unwrap_or(0);
        self.total_completion_tokens = self.total_completion_tokens.saturating_add(spent);
        self.turn_completion_tokens = self.turn_completion_tokens.saturating_add(spent);
    }

    /// (#854, #2792 round-4) Fold a prompt-token count the endpoint reported
    /// for a request that carried `request_chars` message characters.
    pub(super) fn fold_prompt_count(&mut self, prompt_tokens: u32, request_chars: usize) {
        self.frozen_prompt_turns =
            update_frozen_prompt_turns(self.prev_prompt_tokens, prompt_tokens, self.frozen_prompt_turns);
        self.prev_prompt_tokens = Some(prompt_tokens);
        self.latest_prompt_tokens = prompt_tokens;
        self.prompt_anchor = Some(PromptAnchor { chars: request_chars, tokens: prompt_tokens });
    }
}

/// The loop's knobs, each resolved once from its operator override or its
/// built-in default, so every use reads the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub(super) answer_max_tokens: u32,
    pub(super) reasoning_interval: u32,
    pub(super) generation_interval: u32,
    pub(super) stall_recovery_budget: u32,
    /// (#2171) How many generation-bound continuations one turn may spend;
    /// see `Limits::resolve` for why it is floored at 4.
    pub(super) max_generation_continuations: u32,
    pub(super) max_turns: Option<u32>,
    pub(super) max_cumulative_tokens: Option<u32>,
    pub(super) inactivity_budget_secs: u64,
    /// The post-clamp inter-turn rest (#2094).
    pub(super) turn_delay_ms: u64,
    pub(super) streaming: bool,
}

/// The operator's overrides, as `run_with_sleeper` receives them.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Knobs {
    pub(super) max_tokens_per_call: Option<u32>,
    pub(super) reasoning_checkpoint_interval: Option<u32>,
    pub(super) generation_checkpoint_interval: Option<u32>,
    pub(super) max_stall_recoveries: Option<u32>,
    pub(super) max_turns: Option<u32>,
    pub(super) max_cumulative_tokens: Option<u32>,
}

impl Limits {
    pub(super) fn resolve(k: Knobs, inactivity_budget_secs: u64, turn_delay_ms: u64, streaming: bool) -> Self {
        // (#1221) Resolve the per-call cap once; every use below (the request's
        // max_tokens, cap-salvage detection, the budget snapshot, the length-arm
        // diagnostics) reads this so an override stays consistent end-to-end.
        // (#1221) The per-call cap is a CHECKPOINT INTERVAL, not a ceiling — it is
        // constant, and what changes at each checkpoint is whether the reasoning is
        // handed back open (continue) or closed (conclude).
        let answer_max_tokens: u32 = k.max_tokens_per_call.unwrap_or(MAX_TOKENS_PER_CALL);
        // (#2171) The generation check-in. Only takes effect on a call that does
        // NOT carry the reasoning bound, and only when it is actually tighter
        // than the raw answer bound (an operator who sets it >= answer_max_tokens
        // has effectively opted back out — see `capped_by_generation_interval`
        // at the cap-selection site).
        let generation_interval: u32 =
            k.generation_checkpoint_interval.unwrap_or(GENERATION_CHECKPOINT_INTERVAL);
        // (#2171, floor added on merge-gate review) How many GENERATION-bound
        // continuations (never reasoning-bound ones — those stay deliberately
        // open-ended, see the checkpoint-continuation site's own comment on why
        // a THOUGHT gets no ceiling) a single turn may spend before the loop
        // gives up and escalates.
        //
        // This is a NEW per-turn ceiling that did not exist before #2171: pre-
        // #2171, a non-reasoning turn's answer/tool-call batch was capped ONCE,
        // at `answer_max_tokens`, with no separate continuation budget — a
        // length-finish at that cap went through the SAME open-ended checkpoint
        // machinery the reasoning check-in uses (see `length_with_content_at_
        // cap_keeps_the_turn_and_asks_for_a_conclusion`, which checkpoints seven
        // times before the degeneracy gate ever catches it). #2171 lowers the
        // per-call cap for that population to the smaller generation interval,
        // which means the SAME turn now needs multiple continuations to reach
        // the token budget it used to spend in one call — and an open-ended
        // continuation budget for that population would recreate exactly the
        // failure this PR exists to fix: a turn that keeps re-hitting a small
        // cap forever, never producing a terminal outcome, is indistinguishable
        // from the original inactivity-timeout hang from the operator's side —
        // it just fails via MaxTurns/wall-clock instead of exit 137. A runaway
        // must end SOMEWHERE, and unlike a reasoning check-in (where letting a
        // thought run long is the point — degeneracy detection is the only
        // backstop, by design), a generation-bound turn that cannot converge in
        // a bounded number of check-ins is the pathology signal itself.
        //
        // `answer_max_tokens / generation_interval` alone (2 at the shipped
        // defaults: 10000/4000) is too tight — it can starve a THINKING model's
        // long final answer (an answer-region continuation still carries the
        // generation bound once dispatch_has_reasoned is true and the turn has
        // closed its thought) or a tool call whose JSON got cut mid-argument,
        // either of which can legitimately need more than 2 tries to converge.
        // `max(4, ...)` decouples the floor from the ratio: raising
        // `max_tokens_per_call` without touching the generation interval no
        // longer silently shrinks the number of tries available. `.max(1)` on
        // the interval itself just guards a `0`-configured knob from a divide
        // panic; `capped_by_generation_interval` at the cap-selection site is
        // what actually gates whether this value is ever consulted.
        let max_generation_continuations: u32 =
            (answer_max_tokens / generation_interval.max(1)).max(4);
        Self {
            answer_max_tokens,
            reasoning_interval: k.reasoning_checkpoint_interval.unwrap_or(REASONING_CHECKPOINT_INTERVAL),
            generation_interval,
            // (#2190) The stall-recovery budget — how many useless turns (empty
            // `tool_calls`, or a runaway-reasoning cut) the loop tolerates before
            // escalating out of local-tier. Resolved once, same pattern as the
            // three per-call knobs above; every live use below reads this instead
            // of the built-in constant directly, so an operator override is
            // consistent end-to-end (comparisons, the recovery helper, and every
            // stderr/trajectory line that names the budget).
            stall_recovery_budget: k.max_stall_recoveries.unwrap_or(MAX_STALL_RECOVERIES),
            max_generation_continuations,
            max_turns: k.max_turns,
            max_cumulative_tokens: k.max_cumulative_tokens,
            inactivity_budget_secs,
            turn_delay_ms,
            streaming,
        }
    }
}

// ─── the inactivity clock ────────────────────────────────────────────────

/// (#2114) Re-anchor the inactivity clock: a jump past twice the budget in
/// one iteration, with no soft warning in between, is a suspected host
/// sleep/wake, not a stall. A jump WITH the warning already fired is a real
/// stall and keeps its signal (#2114 finding 5).
pub(super) fn reanchors_after_sleep(s: &LoopState, elapsed_secs: u64, budget_secs: u64) -> bool {
    s.turns > 0 && !s.soft_warning_fired && is_suspected_sleep_wake_jump(elapsed_secs, budget_secs)
}

/// (#466) Queue the soft inactivity warning, once per window.
pub(super) fn warns_of_inactivity(s: &LoopState, elapsed_secs: u64, budget_secs: u64) -> bool {
    !s.soft_warning_fired && elapsed_secs >= inactivity_soft_threshold_secs(budget_secs)
}

// ─── the turn boundary ───────────────────────────────────────────────────

/// An operator bound that ends the dispatch before the next call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BudgetStop {
    /// (#325, #457) `max_turns`, carrying the cap.
    MaxTurns(u32),
    /// (#423, #457) the cumulative completion-token cap.
    CumulativeTokens(u32),
}

/// Both caps are operator-opt-in; `None` never stops. The turn cap is
/// checked first.
pub(super) fn budget_stop(s: &LoopState, l: &Limits) -> Option<BudgetStop> {
    if let Some(cap) = l.max_turns.filter(|cap| s.turns >= *cap) {
        return Some(BudgetStop::MaxTurns(cap));
    }
    l.max_cumulative_tokens
        .filter(|cap| s.total_completion_tokens >= *cap)
        .map(BudgetStop::CumulativeTokens)
}

/// (#2094) The inter-turn rest: never before the first request, never inside
/// a checkpoint continuation (the same turn, not a boundary), never when off.
pub(super) fn rests_before_call(s: &LoopState, l: &Limits) -> bool {
    s.turns > 0 && !s.resuming_after_checkpoint && l.turn_delay_ms > 0
}

/// What the turn-boundary checkpoint writes (#2114 finding 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundaryCheckpoint {
    /// Before the first request there is nothing to resume.
    Skip,
    Write,
    /// Mid-continuation: the live accumulation rides along as the hand-back.
    WriteWithHandBack,
}

pub(super) fn boundary_checkpoint(s: &LoopState) -> BoundaryCheckpoint {
    match (s.turns, s.resuming_after_checkpoint) {
        (0, _) => BoundaryCheckpoint::Skip,
        (_, true) => BoundaryCheckpoint::WriteWithHandBack,
        (_, false) => BoundaryCheckpoint::Write,
    }
}

/// (#2114 finding 8) The pace file is honored at every boundary between
/// turns, including before the first request, and never inside a
/// continuation of the same turn. The resume catch-up pass asks the same.
pub(super) fn honors_pace(s: &LoopState) -> bool {
    !s.resuming_after_checkpoint
}

/// The bound one request carries, decided before the request is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallBound {
    /// The region's observation interval (#1221, #2171).
    pub(super) per_call_cap: u32,
    /// What the wire carries as `max_tokens` (#2836).
    pub(super) wire_max_tokens: u32,
    /// The call carried the reasoning check-in.
    pub(super) reasoning: bool,
    /// The call carried the generation check-in.
    pub(super) generation: bool,
}

impl CallBound {
    /// The bound that ACTUALLY ended this call, with its provenance.
    pub(super) fn cut_bound(&self, cut: CutSource) -> BoundRef {
        cut_bound(cut, self.reasoning, self.generation, self.per_call_cap, self.wire_max_tokens)
    }
}

/// (#1221, #2164, #2171, #2836) Which bound a call carries. A turn's first
/// call carries the reasoning bound only once this dispatch has proven it
/// reasons; any call without it carries the generation check-in when that is
/// tighter than the answer bound. A streamed call puts the ceiling on the
/// wire; a non-streamed one has nothing to watch, so the interval stays there.
pub(super) fn call_bound(in_answer_region: bool, dispatch_has_reasoned: bool, l: &Limits) -> CallBound {
    let reasoning = !in_answer_region && dispatch_has_reasoned;
    let generation = !reasoning && l.generation_interval < l.answer_max_tokens;
    let per_call_cap = match (reasoning, generation) {
        (true, _) => l.reasoning_interval,
        (false, true) => l.generation_interval,
        (false, false) => l.answer_max_tokens,
    };
    let wire_max_tokens = if l.streaming { l.answer_max_tokens } else { per_call_cap };
    CallBound { per_call_cap, wire_max_tokens, reasoning, generation }
}

/// (#1221, #2792 round-2) The sequence the NEXT request's records carry: a
/// fresh turn is `turns + 1`, a continuation is the turn it continues.
pub(super) fn request_seq(s: &LoopState) -> u32 {
    if s.resuming_after_checkpoint {
        s.turns
    } else {
        s.turns + 1
    }
}

// ─── reading a response ──────────────────────────────────────────────────

/// (#2164) Did this call reason? A completed block (inline or the separate
/// field), the separate field captured before its strip, or an inline block
/// this call OPENED and has not closed.
pub(super) fn call_reasoned(per_turn_reasoning: &str, separate_field: Option<&str>, content: Option<&str>) -> bool {
    if has_text(per_turn_reasoning) || separate_field.is_some_and(has_text) {
        return true;
    }
    content.is_some_and(opens_unclosed_thought)
}

fn has_text(s: &str) -> bool {
    !s.trim().is_empty()
}

/// An inline thought opened at the head of `content` and not closed, the
/// truncated-mid-first-call shape `extract_think_blocks` skips.
fn opens_unclosed_thought(content: &str) -> bool {
    let trimmed = content.trim_start();
    let opener = crate::budget_request::THINK_OPEN.trim();
    let closer = crate::budget_request::THINK_CLOSE.trim();
    trimmed.starts_with(opener) && trimmed.matches(opener).count() > trimmed.matches(closer).count()
}

/// (#2164) Real, dispatchable output: an answer or tool calls.
pub(super) fn has_dispatchable_output(m: &Message) -> bool {
    m.content.as_deref().is_some_and(has_text) || has_tool_calls(m)
}

pub(super) fn has_tool_calls(m: &Message) -> bool {
    m.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
}

/// No text at all: absent, or only whitespace.
pub(super) fn has_no_text(m: &Message) -> bool {
    !m.content.as_deref().is_some_and(has_text)
}

/// (#1221) A wholly empty assistant message: no text and no calls.
pub(super) fn is_blank_assistant(m: &Message) -> bool {
    m.role == "assistant" && has_no_text(m) && !has_tool_calls(m)
}

/// (#2164) Record, once, that this model shows no reasoning region: it has
/// never reasoned, and this call produced real output without reasoning.
pub(super) fn reports_no_reasoning_region(s: &LoopState, per_turn_reasoning: &str, had_output: bool) -> bool {
    !s.no_reasoning_region_logged && !s.dispatch_has_reasoned && !has_text(per_turn_reasoning) && had_output
}

/// (#1959, #2171) A salvage tells the model to reduce its reasoning only when
/// the ANSWER budget ran out, never on a routine check-in.
pub(super) fn salvage_nudges(b: &CallBound) -> bool {
    !b.reasoning && !b.generation
}

/// What the just-landed message does to the turn's accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnEnd {
    /// (#1221) The turn is over: fold the accumulation into the message.
    Fold,
    /// (#1959) A salvage continues the turn: drop the prefill, keep the work.
    Supersede,
    /// A `length` finish is mid-turn; the length arm owns the prefill.
    Continue,
}

pub(super) fn turn_end(effective_finish: &str, salvaged: bool) -> TurnEnd {
    match (effective_finish != "length", salvaged) {
        (_, true) => TurnEnd::Supersede,
        (true, false) => TurnEnd::Fold,
        (false, false) => TurnEnd::Continue,
    }
}

// ─── tool calls ──────────────────────────────────────────────────────────

/// (#2190, #2229) The stall-recovery budget is spent: both the empty
/// tool-calls arm and the length arm escalate instead of recovering again.
pub(super) fn stall_budget_exhausted(s: &LoopState, l: &Limits) -> bool {
    s.stall_recoveries_used >= l.stall_recovery_budget
}

/// The counters a tool-calls turn leaves behind, and whether it escalates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DispatchTally {
    pub(super) consecutive_malformed_turns: u32,
    pub(super) stall_recoveries_used: u32,
    /// (#2169) `MAX_CONSECUTIVE_MALFORMED_TURNS` turns ran nothing.
    pub(super) escalate: bool,
}

/// (#2169, #2229) A turn that ran nothing counts toward the malformed bound;
/// a turn that ran anything resets it, and pays the stall budget DOWN by one,
/// only when it opened a new turn (a continuation must not refund a recovery
/// its own turn spent).
pub(super) fn tally_dispatch(s: &LoopState, dispatched_any: bool, opened_a_new_turn: bool) -> DispatchTally {
    if !dispatched_any {
        let consecutive = s.consecutive_malformed_turns.saturating_add(1);
        return DispatchTally {
            consecutive_malformed_turns: consecutive,
            stall_recoveries_used: s.stall_recoveries_used,
            escalate: consecutive >= MAX_CONSECUTIVE_MALFORMED_TURNS,
        };
    }
    let stall = if opened_a_new_turn {
        s.stall_recoveries_used.saturating_sub(1)
    } else {
        s.stall_recoveries_used
    };
    DispatchTally { consecutive_malformed_turns: 0, stall_recoveries_used: stall, escalate: false }
}

/// The test-cadence drift detector's next state (#465).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cadence {
    pub(super) last_edited_path: Option<String>,
    pub(super) consecutive_same_file_edits: u32,
    /// The path to name in the nudge, when this call crossed the threshold.
    pub(super) fired: Option<String>,
}

/// An edit or write steps the same-file counter; a bash call (verification)
/// clears it; any other tool leaves it alone.
pub(super) fn cadence_after_call(s: &LoopState, tool_name: &str, arguments: &str) -> Cadence {
    match tool_name {
        "edit" | "write" => {
            let path = extract_edit_target_path(arguments);
            let (last, count, fired) = cadence_drift_step(
                path.as_deref(),
                s.last_edited_path.clone(),
                s.consecutive_same_file_edits,
                TEST_CADENCE_DRIFT_THRESHOLD,
            );
            Cadence { last_edited_path: last, consecutive_same_file_edits: count, fired }
        }
        "bash" => Cadence { last_edited_path: None, consecutive_same_file_edits: 0, fired: None },
        _ => Cadence {
            last_edited_path: s.last_edited_path.clone(),
            consecutive_same_file_edits: s.consecutive_same_file_edits,
            fired: None,
        },
    }
}

// ─── compaction ──────────────────────────────────────────────────────────

/// (#854) The endpoint's count has just been frozen long enough to report.
pub(super) fn stale_count_crossed(s: &LoopState) -> bool {
    s.frozen_prompt_turns == STALE_PROMPT_TOKENS_TURNS
}

/// (#2805) Enough compactions in a row left the thread above its trigger.
pub(super) fn unproductive_compactions_escalate(consecutive: u32) -> bool {
    consecutive >= UNPRODUCTIVE_COMPACTION_TURNS
}

/// (#3013) Enough compactions in a row were followed by a re-read.
pub(super) fn reread_loop_escalates(consecutive: u32) -> bool {
    consecutive >= COMPACTION_REREAD_TURNS
}

/// (#377) The operator's `bail_after_compactions` bound has been reached.
pub(super) fn compaction_bound_reached(compactions: u32, bail_after: Option<u32>) -> bool {
    bail_after.is_some_and(|bail| compactions >= bail)
}

// ─── the length arm ──────────────────────────────────────────────────────

/// What a `length` finish leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LengthEffect {
    /// A cut BELOW our cap with partial output: the context overflowed, a
    /// configuration problem recovery cannot fix (hard error).
    Overflow,
    /// (#2229) The stall-recovery budget is spent.
    Escalate,
    /// (#1123/#1221) An empty completion: nothing to resume, so drop and nudge.
    RecoverStall,
    /// (#1221) Hand the turn back as a prefill and keep going.
    Checkpoint,
}

/// `useless_stall`: no text and no calls. `cap_cliff`: our cap (or an
/// unknown count) cut it. `produced_nothing`: this call's reasoning and
/// content are both empty.
pub(super) fn length_effect(
    s: &LoopState,
    l: &Limits,
    useless_stall: bool,
    cap_cliff: bool,
    produced_nothing: bool,
) -> LengthEffect {
    if !useless_stall && !cap_cliff {
        LengthEffect::Overflow
    } else if stall_budget_exhausted(s, l) {
        LengthEffect::Escalate
    } else if produced_nothing {
        LengthEffect::RecoverStall
    } else {
        LengthEffect::Checkpoint
    }
}

/// (#2171, #2633) Draw one generation continuation when the call carried the
/// generation check-in. Returns the new count and whether it now exceeds the
/// budget; the decision to stop on it is taken after the degeneracy gate.
pub(super) fn draw_generation_budget(s: &LoopState, b: &CallBound, l: &Limits) -> (u32, bool) {
    if !b.generation {
        return (s.generation_continuations_this_turn, false);
    }
    let drawn = s.generation_continuations_this_turn.saturating_add(1);
    (drawn, drawn > l.max_generation_continuations)
}

/// (#2258, #2836 stage 1) The interval that governed this call, which sizes
/// the degeneracy gate's tail: the observation interval when the runtime cut
/// it or the wire carried that interval, else the wire ceiling.
pub(super) fn governing_interval(cut: CutSource, b: &CallBound) -> u32 {
    if matches!(cut, CutSource::RuntimeAbort(_)) || b.wire_max_tokens == b.per_call_cap {
        b.per_call_cap
    } else {
        b.wire_max_tokens
    }
}

/// What a checkpoint does once the gate has judged the slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Remedy {
    /// (#1221) A degenerate ANSWER with no thought left to close: hand off
    /// with everything banked attached.
    HandOff,
    /// (#2633, #3074) The turn ran out of continuations.
    StopAt(ContinuationLimit),
    /// (#1221) A degenerate THOUGHT: close it so the model answers.
    CloseThought,
    /// The thought is already closed; keep handing the answer back.
    ContinueClosed,
    /// The reasoning is not repeating; hand it back open.
    ContinueOpen,
}

/// The ordering is load-bearing (#2633): the repetition verdict outranks a
/// continuation limit, and both outrank the remedies that need another call.
pub(super) fn checkpoint_remedy(
    degenerate: bool,
    writing_thought: bool,
    limit: Option<ContinuationLimit>,
    think_closed: bool,
) -> Remedy {
    if degenerate && !writing_thought {
        return Remedy::HandOff;
    }
    if let Some(limit) = limit {
        return Remedy::StopAt(limit);
    }
    match (degenerate, think_closed) {
        (true, _) => Remedy::CloseThought,
        (false, true) => Remedy::ContinueClosed,
        (false, false) => Remedy::ContinueOpen,
    }
}

#[cfg(test)]
#[path = "loop_deciders_tests.rs"]
mod tests;
