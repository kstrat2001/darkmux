//! (#3136) The agent loop's effects: everything `run_with_sleeper` does that
//! touches the world — the model call, tool dispatch, compaction, rests,
//! checkpoints, trajectory records and stderr.
//!
//! Every DECISION about which of those happens next is a decider in
//! `loop_deciders.rs`, a pure function of [`LoopState`]. The methods here
//! perform the effect a decider chose and fold what came back into the state.
//! They are split by phase of one iteration, in the order the iteration runs
//! them: the inactivity clock, the turn boundary, the pre-send bound, the
//! call, reading the response, then the arm its finish reason routes to.

use super::loop_deciders::*;
use super::*;

/// The references a dispatch runs against, as `run_with_sleeper` receives them.
pub(super) struct Wiring<'a> {
    pub(super) client: &'a LmStudioClient,
    pub(super) compactor_client: &'a LmStudioClient,
    pub(super) model: &'a str,
    pub(super) tools: &'a [Tool],
    pub(super) trajectory: &'a mut Trajectory,
    pub(super) compaction_cfg: &'a compaction::CompactionConfig,
    pub(super) feedback_templates: std::collections::BTreeMap<String, String>,
    pub(super) response_format: Option<serde_json::Value>,
    pub(super) out_dir: &'a std::path::Path,
    pub(super) role_id: &'a str,
    pub(super) sleeper: &'a dyn TurnSleeper,
}

/// One dispatch in flight: its wiring, its [`LoopState`], and the effectful
/// state the deciders never see (the thread, the turn's accumulation, the
/// clock, the detectors).
pub(super) struct AgentLoop<'a> {
    client: &'a LmStudioClient,
    compactor_client: &'a LmStudioClient,
    model: &'a str,
    trajectory: &'a mut Trajectory,
    cfg: &'a compaction::CompactionConfig,
    response_format: Option<serde_json::Value>,
    out_dir: &'a std::path::Path,
    role_id: &'a str,
    sleeper: &'a dyn TurnSleeper,
    tool_defs: Vec<crate::lmstudio::ToolDef>,
    allowed_tool_names: HashSet<String>,
    limits: Limits,
    inactivity_bound: BoundRef,
    max_pause_ms: u64,
    state: LoopState,
    messages: Vec<Message>,
    turn: TurnAccum,
    last_proof_of_work: std::time::Instant,
    // (#799) Accumulate bash invocations that FAILED TO RUN (never executed) —
    // stamped onto the outcome/envelope as the verifier-fabrication backstop.
    failed_to_run: Vec<FailedExec>,
    cycle_detector: CycleDetector,
    failure_rate_detector: FailureRateDetector,
    reasoning_loop_detector: ReasoningLoopDetector,
    feedback_injector: FeedbackInjector,
    unproductive_compactions: crate::unproductive_compactions::UnproductiveCompactions,
    compaction_repeat: crate::compaction_repeat::CompactionRepeat,
    pace_reader: pace::PaceReader,
    pace_expiry_warned: bool,
}

/// What came back from one model call, before any of it is read.
struct Sent {
    response: crate::lmstudio::ChatResponse,
    runtime_cut: CutSource,
    cut_estimate: Option<u32>,
    request_message_chars: usize,
    opened_a_new_turn: bool,
}

/// What the response's FIRST choice says, read before `model.completed` is
/// written, because that record says which calls will run (#2963).
struct Plan {
    finish_reason: String,
    tool_calls: Option<Vec<ToolCall>>,
    cut: CutSource,
    salvaged: bool,
    call_plan: Vec<CallFate>,
}

/// One call, fully read: everything the finish-reason arms act on.
struct Call {
    bound: CallBound,
    opened_a_new_turn: bool,
    cut: CutSource,
    cut_estimate: Option<u32>,
    completion_tokens: Option<u32>,
    call_plan: Vec<CallFate>,
    planned_tool_calls: Option<Vec<ToolCall>>,
    assistant_message: Message,
    per_turn_reasoning: String,
    effective_finish_reason: String,
}

/// The resume catch-up's view of the checkpoint it resumes.
struct CatchUp<'c> {
    seed: Option<&'c checkpoint::RunCheckpoint>,
    pending: &'c [ToolCall],
    seq_base: u32,
}

impl<'a> AgentLoop<'a> {
    pub(super) fn new(
        w: Wiring<'a>,
        knobs: Knobs,
        streaming: bool,
        initial_messages: Vec<Message>,
        resume_seed: Option<&checkpoint::RunCheckpoint>,
    ) -> Self {
        // (#2114) A resumed dispatch replaces the fresh `initial_messages` (the
        // system prompt + first user turn main.rs built) with the checkpoint's
        // own history — the checkpoint already carries whatever system/user
        // messages opened the ORIGINAL dispatch, so re-seeding from scratch
        // would duplicate them.
        let messages = match resume_seed {
            Some(ckpt) => ckpt.messages.clone(),
            None => initial_messages,
        };
        // (#1221) The turn currently in flight: its two output regions and the
        // prefill message that carries them back to the model. See `TurnAccum` —
        // these were six loose `let mut`s mutated at seven sites, and the two
        // defects that cost were both "cleared the state, left the message".
        // (#2114) Seeded from the checkpoint's `pending_hand_back` on a resume so
        // a continuation resumes the SAME accumulation instead of an empty one;
        // `prefill_at` points at the LAST message, which `messages` (seeded
        // above from the same checkpoint) already carries as its final prefill.
        let turn = match resume_seed.and_then(|c| c.pending_hand_back.as_ref()) {
            Some(hb) => TurnAccum {
                thought: hb.thought.clone(),
                answer: hb.answer.clone(),
                think_closed: hb.think_closed,
                is_reasoning: hb.is_reasoning,
                carries_own_opener: hb.carries_own_opener,
                prefill_at: messages.len().checked_sub(1),
            },
            None => TurnAccum::default(),
        };
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
        // (#2165) The host forwards a companion source var alongside the value
        // above — same "value + tier" pairing every other host-forwarded knob
        // uses. Absent (a manual `docker run` with no host wrapper) reads as
        // built-in, matching the value default just above.
        let inactivity_bound = BoundRef::new(
            BoundKind::InactivityTimeout,
            inactivity_budget_secs,
            bounds::BoundSource::from_cli_str(
                &std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE").unwrap_or_default(),
            ),
        );
        // (#2479 audit) `Instant`, deliberately: this tracks the container's
        // OWN proof-of-work (tool.completed / compaction) inside the Docker
        // Desktop Linux VM. This is NOT the same clock as the host-side
        // watchdog it races against (darkmux-crew's `dispatch_internal.rs`,
        // same audit note) — that one is macOS monotonic time, confirmed by
        // this audit to sit flat across a host sleep. This one is the guest
        // VM's clock, and its behavior across a host suspend is UNVERIFIED —
        // see the `is_suspected_sleep_wake_jump` doc in `loop_runner.rs`
        // (#2114), which already treats "this clock kept advancing through a
        // host suspend" as a live possibility worth re-anchoring against, not
        // a closed question. Don't read this comment as settling that;
        // wall clock would also be wrong here in the case this audit DID
        // confirm (a host sleep with no accompanying guest-clock jump): it
        // would fire the soft warning / hard kill on a dispatch that was never
        // actually stuck, purely because the laptop's lid was closed a while.
        let last_proof_of_work = std::time::Instant::now();
        // (#2114 finding 4) Ceiling past which a held `pause: true` is treated
        // as abandoned rather than honored forever — read once at startup,
        // same pattern as `inactivity_budget_secs` above.
        let max_pause_ms = pace::max_pause_ms();
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
        let (turn_delay_ms, turn_delay_warning) = resolve_turn_delay_ms(turn_delay_ms, inactivity_budget_secs);
        if let Some(w) = &turn_delay_warning {
            eprintln!("{w}");
        }
        Self {
            client: w.client,
            compactor_client: w.compactor_client,
            model: w.model,
            trajectory: w.trajectory,
            cfg: w.compaction_cfg,
            response_format: w.response_format,
            out_dir: w.out_dir,
            role_id: w.role_id,
            sleeper: w.sleeper,
            tool_defs: w.tools.iter().map(|t| t.to_tool_def()).collect(),
            // Set of tool names the model is allowed to call. Drives the
            // plain-text-tool-call promoter (#406): any tool name in the
            // promoted markup that isn't here is rejected so adversarial /
            // malformed output can't smuggle arbitrary tool names into the
            // dispatch pipeline.
            allowed_tool_names: w.tools.iter().map(|t| t.name().to_string()).collect(),
            limits: Limits::resolve(knobs, inactivity_budget_secs, turn_delay_ms, streaming),
            inactivity_bound,
            max_pause_ms,
            state: LoopState::seeded(resume_seed),
            messages,
            turn,
            last_proof_of_work,
            failed_to_run: Vec::new(),
            // (#418) Per-dispatch cycle detector — warns on repeated tool
            // calls within a sliding window. Observability-only in the MVP;
            // bail-on-cycle is a follow-up if warn alone proves insufficient.
            cycle_detector: CycleDetector::new(),
            // (#419) Per-dispatch tool-failure-rate detector — warns on
            // repeated failures of one `(tool, args)` signature (e.g., agent
            // retrying gcc inside sandbox where it doesn't exist). Sibling to the cycle
            // detector; same MVP shape (warn-only).
            failure_rate_detector: FailureRateDetector::new(),
            // (#461) Per-dispatch reasoning-loop detector — warns when the
            // model's reasoning stream repeats across turns. Catches the
            // Beat 54 Run 5 case where every tool call looks unique but
            // the reasoning is visibly stuck. Sibling of cycle_detector;
            // same sliding-window shape applied to reasoning text instead
            // of tool args.
            reasoning_loop_detector: ReasoningLoopDetector::new(),
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
            feedback_injector: FeedbackInjector::with_templates(w.feedback_templates),
            // (#2793) Consecutive compactions that installed and still left occupancy
            // above the trigger. (#2805) No latch — reaching the bound escalates and
            // returns, so the episode cannot repeat within a dispatch.
            unproductive_compactions: crate::unproductive_compactions::UnproductiveCompactions::new(),
            compaction_repeat: crate::compaction_repeat::CompactionRepeat::new(),
            // (#2114) Reads + parses `pace.json` on demand (not once at startup
            // like `turn_delay_ms` below — the pace file is meant to change
            // mid-dispatch); tracks whether a malformed sighting has already been
            // warned about so a broken writer doesn't spam stderr once per 2s
            // poll.
            pace_reader: pace::PaceReader::new(),
            // Edge-trigger so the staleness warning fires once per abandoned-pause
            // episode, not once per turn boundary while the same stale file sits
            // there.
            pace_expiry_warned: false,
        }
    }

    /// Run the loop until a terminal outcome.
    pub(super) fn run(mut self) -> Result<LoopOutcome> {
        loop {
            if let Some(outcome) = self.iterate()? {
                return Ok(outcome);
            }
        }
    }

    /// One iteration: one model call and whatever its finish reason routes
    /// to. `Some` ends the dispatch.
    fn iterate(&mut self) -> Result<Option<LoopOutcome>> {
        self.watch_inactivity();
        self.drain_feedback();
        if let Some(stop) = budget_stop(&self.state, &self.limits) {
            return Ok(Some(self.stop_on_budget(stop)));
        }
        self.cross_turn_boundary();
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
        // the first call of every turn carries the GENERATION bound instead
        // (#2171 — `generation_checkpoint_interval_tokens`, tighter than the
        // raw answer bound by default), same as an already-answering turn
        // does. A thinking model checks in one call later on its very first
        // turn (the cost #1221's own follow-up measured as small); a
        // non-reasoning model's tool-call batches are never capped by an
        // interval meant to bound reasoning, not answers — but they ARE
        // capped by the generation check-in, and deliberately without the
        // "reduce your reasoning" nudge that reasoning-bound cuts can carry
        // (a model that was never reasoning has nothing to reduce — see
        // `sent_generation_bound`'s use at the salvage nudge guard).
        let bound = call_bound(self.turn.in_answer_region(), self.state.dispatch_has_reasoned, &self.limits);
        // (#1959) Which bound this request carried, captured HERE.
        //
        // Consumers downstream cannot re-derive it from `turn`: `absorb` has
        // not run for this turn yet when the salvage check fires, so the
        // region state still describes the PREVIOUS turn. A first attempt read
        // `turn.writing_thought()` at the salvage site and silently never
        // suppressed anything.
        // (#2836 stage 1) THE WIRE CARRIES ONLY THE CEILING.
        //
        // `per_call_cap` keeps its name and its meaning for everything
        // downstream — it is the region's OBSERVATION interval, and it still
        // sizes the degeneracy detector's tail and names the bound in every
        // record. What changed is that it no longer leaves the machine.
        //
        // Sending it as `max_tokens` is what made the check-in destructive:
        // the bound was enforced SERVER side, so it truncated whatever was in
        // flight, and `tool_calls` and `content` are separate response
        // channels but one generation stream. Measured over four runs on
        // 2026-09-20: 9 of 14 firings (64%) destroyed a tool call, every one
        // of them an `edit` cut after a single character of arguments.
        //
        // The interval was never meant to be a ceiling —
        // `REASONING_CHECKPOINT_INTERVAL`'s own doc says so. It is a
        // check-in, and a check-in that deletes the work it is checking on
        // is not observing, it is interrupting.
        // The exception, and it is not a carve-out but the same rule: the
        // interval comes off the wire BECAUSE the runtime can watch the
        // stream instead. On a non-streamed call there is nothing to watch
        // — the whole response arrives at once, and the runtime never gets
        // the chance to intervene before the endpoint has already generated
        // everything. There the server-side bound is still the only
        // check-in that exists, so it stays exactly as it was.
        // (#2836) The wire carries the CEILING, never the check-in interval.
        //
        // Sending the interval is what made the check-in destructive: it was
        // enforced server-side, so it truncated whatever was in flight, and
        // `tool_calls` and `content` are separate response channels but one
        // generation stream. Measured over four runs before the fix: 9 of 14
        // firings destroyed a tool call.
        //
        // The ceiling stays here rather than moving client-side, because the
        // engine counts tokens and the runtime counts characters — see
        // `MAX_TOKENS_PER_CALL`. Raised to 32,000, it is a genuine-pathology
        // backstop instead of the thing interrupting ordinary work.
        //
        // On a non-streamed call there is nothing to watch, so the wire keeps
        // the interval exactly as before: the interval can only come off the
        // wire BECAUSE the runtime can observe the stream instead.
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
        self.enforce_pre_send_bound();
        let sent = self.send(&bound)?;
        let call = self.read_response(bound, sent)?;
        self.route(call)
    }

    // ─── outcomes ────────────────────────────────────────────────────────

    /// End the dispatch, carrying the thread and every banked turn out.
    fn finish(&mut self, final_answer: Option<String>, terminal_reason: TerminalReason) -> LoopOutcome {
        LoopOutcome {
            final_answer,
            terminal_reason,
            messages: std::mem::take(&mut self.messages),
            turn_delay_effective_ms: self.limits.turn_delay_ms,
            failed_to_run: self.failed_to_run.clone(),
        }
    }

    /// Record the escalation, then end the dispatch with it.
    fn escalate(&mut self, reason: EscalationReason, final_answer: Option<String>) -> LoopOutcome {
        self.trajectory.append_escalation_triggered(
            self.state.turns,
            escalation_reason_str(reason),
            self.model,
            self.state.latest_prompt_tokens,
        );
        self.finish(final_answer, TerminalReason::EscalationTriggered(reason))
    }

    /// (#466/#469) A proof-of-work signal: restart the inactivity window.
    fn prove_work(&mut self) {
        self.last_proof_of_work = std::time::Instant::now();
        self.state.soft_warning_fired = false;
    }

    fn honor_pace(&mut self) {
        honor_pace_pause(
            &mut self.pace_reader,
            self.out_dir,
            self.max_pause_ms,
            self.limits.inactivity_budget_secs,
            &mut self.pace_expiry_warned,
            self.sleeper,
            self.trajectory,
            self.state.turns,
            &mut self.last_proof_of_work,
            &mut self.state.soft_warning_fired,
        );
    }

    /// Persist the thread and counters so a killed container can resume
    /// (#2114). Best-effort: a failed write loses resumability, not progress.
    fn write_checkpoint(
        &self,
        pending_hand_back: Option<checkpoint::PendingHandBack>,
        pending_tool_calls: Option<Vec<ToolCall>>,
        pending_tool_calls_seq_base: u32,
    ) {
        let snapshot = checkpoint::RunCheckpoint {
            schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
            role_id: self.role_id.to_string(),
            messages: self.messages.clone(),
            turns: self.state.turns,
            total_completion_tokens: self.state.total_completion_tokens,
            compactions: self.state.compactions,
            pending_hand_back,
            pending_tool_calls,
            pending_tool_calls_seq_base,
            pending_head_started: false,
            written_at_unix_ms: checkpoint::unix_ms(),
        };
        if let Err(e) = checkpoint::write_checkpoint(self.out_dir, &snapshot) {
            eprintln!(
                "darkmux-runtime: ⚠ failed to write checkpoint: {e} (continuing without one)"
            );
        }
    }

    fn tool_start<'s>(&'s self, pending: &'s [ToolCall], seq_base: u32) -> checkpoint::ToolStart<'s> {
        checkpoint::ToolStart {
            out_dir: self.out_dir,
            role_id: self.role_id,
            messages: &self.messages,
            turns: self.state.turns,
            total_completion_tokens: self.state.total_completion_tokens,
            compactions: self.state.compactions,
            pending,
            seq_base,
        }
    }

    // ─── resume ──────────────────────────────────────────────────────────

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
    pub(super) fn resume_catch_up(&mut self, seed: Option<&checkpoint::RunCheckpoint>) -> Option<LoopOutcome> {
        let pending_calls = seed.and_then(|c| c.pending_tool_calls.clone())?;
        // (#2114 finding N6) `tool_seq` for the FIRST pending call picks up
        // exactly where the killed run left off, so the SAME call gets the
        // SAME `tool_seq` in `trajectory.jsonl` whether it's seen from the
        // original run or from this catch-up.
        let catch_up = CatchUp {
            seed,
            pending: &pending_calls,
            seq_base: seed.map(|c| c.pending_tool_calls_seq_base).unwrap_or(0),
        };
        for (idx, call) in pending_calls.iter().cloned().enumerate() {
            self.catch_up_one(&catch_up, idx, call);
        }
        if let Some(outcome) = self.compact_after_resume() {
            return Some(outcome);
        }
        let total_pending = pending_calls.len();
        let turns = self.state.turns;
        eprintln!(
            "darkmux-runtime: ▶ resumed mid-turn — dispatched the {total_pending} tool call(s) \
             remaining from turn {turns} before requesting the next turn. (#2114)"
        );
        None
    }

    fn catch_up_one(&mut self, c: &CatchUp<'_>, idx: usize, call: ToolCall) {
        // (#2114 finding N7) Honor an active pace pause BEFORE every
        // catch-up dispatch — including the very first — so a resume
        // into a live thermal pause doesn't barrel through its
        // undispatched tool calls (often the most expensive ones,
        // since they're what got the dispatch killed in the first
        // place) before the governor's hold takes effect.
        if honors_pace(&self.state) {
            self.honor_pace();
        }
        let tool_seq = c.seq_base + idx as u32;
        let caught = catch_up_dispatch(c.seed, idx, &self.tool_start(&c.pending[idx..], tool_seq), &call, dispatch);
        let outcome = caught.outcome(&call.function.name);
        let run = caught.run;
        let result = run.result;
        let tool_ok = outcome.tool_worked();
        self.record_failed_to_run(&call, &result);
        self.trajectory.append_tool_completed(
            self.state.turns,
            tool_seq,
            &call.function.name,
            &call.function.arguments,
            &result,
            &outcome,
            run.emitted.as_ref(),
            run.emit_seq,
        );
        if tool_ok {
            self.prove_work();
        }
        self.messages.push(Message::tool_result(call.id, call.function.name, result));
        let (pending, seq_base) = pending_after(c.pending, idx, tool_seq + 1);
        self.write_checkpoint(None, pending, seq_base);
    }

    fn compact_after_resume(&mut self) -> Option<LoopOutcome> {
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
        let trim_stats = crate::tool_result_prune::soft_trim_old_tool_results(&mut self.messages);
        if trim_stats.results_trimmed > 0 {
            eprintln!(
                "darkmux-runtime: soft-trimmed {} old tool result(s), reclaiming {} bytes \
                 of transcript before the post-resume compaction check (#1391/#2114)",
                trim_stats.results_trimmed, trim_stats.bytes_reclaimed
            );
        }
        // (#2792) The same occupancy measure the main loop uses. This path
        // was ALREADY correct — it has always measured the current thread
        // rather than consulting a reported count, because after a resume
        // there is no reported count to consult. That made it the one place
        // in the loop that could not exhibit the #2792 overshoot, and the
        // reason the fix is a collapse onto existing behavior rather than a
        // new policy. `0` for the reported count: nothing has been sent yet
        // this invocation, so the estimate is necessarily the larger.
        // `None` for the anchor, and it is not an omission: this block runs
        // BEFORE the turn loop, so nothing has been sent this invocation and
        // `prompt_anchor` is provably still `None`. Passing the variable read
        // as though it could be `Some`.
        let resume_estimate_tokens = effective_prompt_occupancy(&self.messages, 0, None);
        if !compaction::needs_compaction(resume_estimate_tokens, self.messages.len(), self.cfg) {
            return None;
        }
        let before_count = self.messages.len();
        // (#2792 merge-gate, second pass) The RESUME catch-up site gets
        // the same treatment as the main loop: a refusal is a recorded
        // skip, not a dispatch kill, and `compactions` counts only what
        // actually installed.
        //
        // An earlier revision converted the main loop alone and left this
        // one propagating with `?`. That is the WORSE of the two places
        // to leave it: a resume starts from a checkpoint whose thread is
        // already large, so it is exactly where a middle too small to
        // compact meets a thread big enough to trigger — and killing the
        // dispatch there discards the work the checkpoint existed to
        // preserve.
        let installed = self.attempt_compaction(self.limits.answer_max_tokens, "resume catch-up compaction");
        if let Some(summary) = installed {
            self.record_resume_compaction(summary, before_count, resume_estimate_tokens);
        }
        self.prove_work();
        if !compaction_bound_reached(self.state.compactions, self.cfg.bail_after_compactions) {
            return None;
        }
        let compactions = self.state.compactions;
        let bail = self.cfg.bail_after_compactions.unwrap_or_default();
        eprintln!(
            "darkmux-runtime: escalation_triggered — \
             compactions ({compactions}) reached bail_after_compactions ({bail}) \
             during resume catch-up; emitting EscalationTriggered terminal for \
             frontier handoff instead of requesting the next turn. (#2114)"
        );
        // (#2190) `latest_prompt_tokens` is still its pre-loop 0
        // here — no real turn has completed yet in this resume
        // catch-up path (see the occupancy note above on the same
        // limitation).
        let final_answer = self.turn.pending_answer();
        Some(self.escalate(EscalationReason::CompactionLimitReached, final_answer))
    }

    fn record_resume_compaction(&mut self, (summary_chars, lexically_repaired): (usize, bool), before_count: usize, tokens_before: u32) {
        let after_count = self.messages.len();
        let (sys_chars_after, prompt_chars_after) = measure_request_context(&self.messages);
        let tokens_after = ((sys_chars_after + prompt_chars_after) / 4) as u32;
        self.trajectory.append_compaction(
            self.state.compactions,
            before_count,
            after_count,
            crate::trajectory::InstalledSummary { summary_chars, lexically_repaired },
            tokens_before,
            tokens_after,
        );
        eprintln!(
            "darkmux-runtime: compacted after the resume catch-up pass ({before_count} → \
             {after_count} messages) before the first post-resume request. (#2114)"
        );
        // (#2114 finding 1) Resume-compaction parity with the main
        // loop's `tool_calls` arm (`compact_thread`): a compaction here is
        // the SAME event with the SAME consequences, whichever site
        // triggered it. Queue the same post-compaction feedback nudge,
        // reset proof-of-work + the soft-warning flag the same way,
        // and run the SAME `bail_after_compactions` escalation check
        // — without this, a resume that immediately compacts past the
        // operator's bound would silently send one more request
        // instead of escalating to the frontier the way a live
        // (never-killed) run in the identical position would.
        self.feedback_injector.queue_post_compaction(self.state.turns);
    }

    // ─── the inactivity clock and the turn boundary ─────────────────────

    fn watch_inactivity(&mut self) {
        let budget = self.limits.inactivity_budget_secs;
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
        let elapsed_secs = self.last_proof_of_work.elapsed().as_secs();
        // (#2114 finding 5) Gated on `!inactivity_soft_warning_fired_
        // in_window` — the doc above (and `is_suspected_sleep_wake_
        // jump`'s own doc) already claims a genuine stall would have
        // fired the smaller soft warning BEFORE reaching 2x the full
        // budget, so a jump WITH the warning already fired is a real
        // stall that simply never got its proof-of-work reset, not a
        // suspected sleep/wake — re-anchoring that case would erase a
        // legitimate stall signal instead of correcting a clock.
        if reanchors_after_sleep(&self.state, elapsed_secs, budget) {
            let inactivity_budget_secs = budget;
            eprintln!(
                "darkmux-runtime: ⚠ suspected host sleep/wake — {elapsed_secs}s elapsed \
                 since the last proof-of-work signal, more than 2x the {inactivity_budget_secs}s \
                 inactivity budget in a single loop iteration. Re-anchoring the deadline \
                 instead of treating this as a stall."
            );
            self.prove_work();
        }
        // (#466) Check soft-deadline approach before draining. If the
        // dispatch has gone past 75% of the inactivity budget without
        // a proof-of-work signal AND we haven't already warned in
        // this window, queue the warning so it drains alongside any
        // other pending signals on this iteration. Edge-triggered:
        // the flag clears on the next proof-of-work reset.
        let elapsed_secs = self.last_proof_of_work.elapsed().as_secs();
        if warns_of_inactivity(&self.state, elapsed_secs, budget) {
            eprintln!(
                "darkmux-runtime: ⚠ inactivity-approach — {}s elapsed without a \
                 proof-of-work signal, approaching {}. Queueing soft warning before \
                 the host-side hard kill.",
                elapsed_secs, self.inactivity_bound.describe()
            );
            self.feedback_injector.queue_inactivity_approach(elapsed_secs, budget);
            self.state.soft_warning_fired = true;
        }
    }

    fn drain_feedback(&mut self) {
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
        let pending_feedback = self.feedback_injector.drain();
        if pending_feedback.is_empty() {
            return;
        }
        let count = pending_feedback.len();
        // (#457 Step 3) Replace Step 1's combined "cycle_or_cascade"
        // bucket with per-signal-kind discrimination. The injector
        // tracks which kinds were drained on the most recent call;
        // we read them and stamp on the trajectory event so
        // analytics can distinguish cycle / cascade / compaction /
        // cadence-drift firings.
        let kinds = self.feedback_injector.last_drained_kinds().to_vec();
        self.messages.extend(pending_feedback);
        self.trajectory.append_feedback_injected(self.state.turns, count, &kinds);
    }

    fn stop_on_budget(&mut self, stop: BudgetStop) -> LoopOutcome {
        let final_answer = self.turn.pending_answer();
        match stop {
            BudgetStop::MaxTurns(cap) => {
                // (#325, #457) max_turns is operator-opt-in. When set, hitting
                // the cap returns a structured `result: "max_turns"` terminal —
                // distinguishable from Docker / LMStudio failures (which would
                // surface as `result: "error"`). When unset (`None`), the loop
                // runs unbounded turn-count-wise; other bounds (inactivity
                // timeout, per-call token cap, cumulative-tokens cap) still
                // apply if set.
                eprintln!(
                    "darkmux-runtime: loop hit max_turns={cap} without reaching stop; \
                     returning partial outcome"
                );
                self.finish(final_answer, TerminalReason::MaxTurns)
            }
            BudgetStop::CumulativeTokens(cap) => {
                // (#423, #457) Cumulative completion-tokens cap is operator-
                // opt-in. When set, hitting it triggers an
                // `EscalationTriggered(CumulativeTokensExceeded)` terminal so
                // the operator's intervention layer can investigate without
                // unbounded cost. When unset (`None`), no cap applies —
                // operators running on their own hardware can let long-arc
                // work continue.
                let total_completion_tokens = self.state.total_completion_tokens;
                eprintln!(
                    "darkmux-runtime: cumulative completion_tokens={total_completion_tokens} \
                     reached cap max_tokens={cap}; escalating out of local tier with \
                     partial outcome (#423, #457)"
                );
                self.escalate(EscalationReason::CumulativeTokensExceeded, final_answer)
            }
        }
    }

    fn cross_turn_boundary(&mut self) {
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
        if rests_before_call(&self.state, &self.limits) {
            let turn_delay_ms = self.limits.turn_delay_ms;
            // (#2877) Recorded as the rest starts; see the duty-cycle rest.
            self.trajectory.append_rest(self.state.turns, turn_delay_ms);
            self.sleeper.sleep(turn_delay_ms);
            // (#2094 finding 3b) Harness-owned time, not a stall: EXTEND
            // (never reset to "now") the soft-inactivity clock by exactly
            // the rest duration, and clear the edge-trigger flag so a
            // fresh rest buys a fresh chance before the next soft warning
            // — mirrors the tool.completed/compaction proof-of-work resets
            // elsewhere in this loop. Both effects are bundled in
            // `absorb_rest_into_soft_inactivity_clock` (see its own doc)
            // so this call site can't apply one half without the other.
            (self.last_proof_of_work, self.state.soft_warning_fired) =
                absorb_rest_into_soft_inactivity_clock(self.last_proof_of_work, turn_delay_ms);
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
        match boundary_checkpoint(&self.state) {
            BoundaryCheckpoint::Skip => {}
            BoundaryCheckpoint::Write => self.write_checkpoint(None, None, 0),
            BoundaryCheckpoint::WriteWithHandBack => {
                let hand_back = checkpoint::PendingHandBack {
                    thought: self.turn.thought.clone(),
                    answer: self.turn.answer.clone(),
                    think_closed: self.turn.think_closed,
                    is_reasoning: self.turn.is_reasoning,
                    carries_own_opener: self.turn.carries_own_opener,
                };
                self.write_checkpoint(Some(hand_back), None, 0);
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
        if honors_pace(&self.state) {
            self.honor_pace();
        }
    }

    // ─── the pre-send bound ──────────────────────────────────────────────

    fn enforce_pre_send_bound(&mut self) {
        // (#2792) PRE-SEND BOUND. The last thing before the request is built:
        // is what is about to go out actually inside the window the profile
        // DECLARES?
        //
        // Everything upstream is a trigger, not a bound. `needs_compaction`
        // decides whether to compact BETWEEN turns; the growth that breaks the
        // window happens WITHIN one, when a tool result lands after the last
        // compaction and before this line. Measured with the trigger fix
        // already in: 14 of 124 turns went out at up to 39,973 against a
        // declared 32,000, because a result in the protected recent window is
        // untouchable by both compaction and the soft trim.
        //
        // MEASURE WHAT IS SENT, NOT JUST THE MESSAGES (round-2 merge gate).
        // `measure_request_context` sums message content and tool-call args.
        // The request also carries the TOOLS SCHEMA — 12,050 bytes for the
        // default 8-tool palette, ~3,000 tokens on this ruler, present on
        // every single request. Budgeting without it certified `fits: true`
        // on a body 36% over the window, which is the defect this bound
        // exists to prevent, wearing a record that says it was checked.
        let Some(window) = self.cfg.context_window else {
            return;
        };
        // Serialized inside the guard, not above it: it is used nowhere
        // else, and hoisting it cost a ~12KB serialization every turn on
        // dispatches that declare no window at all.
        let tools_bytes = serde_json::to_string(&self.tool_defs).map(|t| t.len()).unwrap_or(0);
        let (sys_c, prompt_c) = measure_request_context(&self.messages);
        // MEASURE AGAINST THE ENDPOINT'S OWN COUNT (round-4 merge gate).
        // The previous revision divided every character by 4 and compared
        // to the window. Measured on the dogfood run that reopened this
        // issue: the overflowing turn estimated 30,132 against a 32,000
        // budget — under, so nothing trimmed — and the endpoint counted
        // the request it then sent at 38,434. A 27% under-count at
        // exactly the turn that overflows means the bound was silent on
        // the only send it exists to catch, and fired one turn late.
        // `estimate_prompt_tokens` carries `usage.prompt_tokens` forward
        // instead, so only this turn's new characters are guessed.
        let before_tokens = estimate_prompt_tokens(sys_c + prompt_c, tools_bytes, self.state.prompt_anchor);
        if before_tokens <= window {
            return;
        }
        let prompt_anchor = self.state.prompt_anchor;
        // Derived by INVERTING the same estimator the line above
        // decided with. A budget on a different ruler would trim to a
        // target that still measures over.
        let message_budget = message_chars_budget(window, tools_bytes, prompt_anchor);
        // Floor chosen for THIS path, not inherited from the soft
        // trim's 4,000 — see `hard_trim_to_fit`'s own note on why that
        // constant made the bound inert on the modal shape.
        let stats = crate::tool_result_prune::hard_trim_to_fit(
            &mut self.messages,
            message_budget,
            HARD_TRIM_MIN_BODY_BYTES,
        );
        let (sys_a, prompt_a) = measure_request_context(&self.messages);
        let after_tokens = estimate_prompt_tokens(sys_a + prompt_a, tools_bytes, prompt_anchor);
        let fits = after_tokens <= window;
        if stats.results_trimmed > 0 {
            eprintln!(
                "darkmux-runtime: the next request was ~{before_tokens} tokens against \
                 the {window}-token window this profile DECLARES (including \
                 ~{} for the tool schemas) — hard-trimmed {} tool result(s), \
                 reclaiming {} bytes, to ~{after_tokens}. Each trimmed result keeps \
                 its head and tail around an elision marker. (#2792)",
                (tools_bytes as f64 / UNCOUNTED_CHARS_PER_TOKEN) as u32,
                stats.results_trimmed,
                stats.bytes_reclaimed
            );
        }
        if !fits {
            self.report_unfittable_request(after_tokens, window, stats.results_trimmed);
        }
        self.trajectory.append_pre_send_bound(
            // (#2792 round-2) `turns` has NOT been incremented yet at
            // this point — the sequence for this request is computed
            // by `request_seq`, as in `send`. Stamping the raw `turns` put the event
            // one behind the `model.completed` it pairs with, which is
            // the #1221 off-by-one this file already learned once.
            request_seq(&self.state),
            before_tokens,
            after_tokens,
            window,
            stats.results_trimmed,
        );
    }

    fn report_unfittable_request(&self, after_tokens: u32, window: u32, results_trimmed: usize) {
        // Say WHICH fact is true. An earlier revision always
        // claimed "the weight is not in tool results", which was
        // a false factual claim on the shape where every result
        // is merely below the trim floor — and sent the operator
        // to raise n_ctx when clearing results would have worked.
        let trimmable = self
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_ref())
            .filter(|b| b.len() > HARD_TRIM_MIN_BODY_BYTES)
            .count();
        // (round-5 merge gate) The `else` arm used to read "the
        // weight is not in tool results", which is reached when
        // there ARE trimmable results and none were trimmed — and
        // is false there. Measured by the review: a turn printed
        // it while holding results the very next turn trimmed
        // 44,000 bytes out of. It sent the operator to raise
        // n_ctx when clearing results would have worked, which is
        // the same false-reason defect the arm above it was added
        // to fix.
        let already_elided = self
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_ref())
            .filter(|b| b.len() > HARD_TRIM_MIN_BODY_BYTES)
            .filter(|b| {
                b.contains(crate::tool_result_prune::TOOL_RESULT_TRIM_MARKER_SENTINEL)
            })
            .count();
        let why = why_the_bound_could_not_fit(
            results_trimmed,
            trimmable,
            already_elided,
        );
        eprintln!(
            // SAY ESTIMATE, NOT PROPHECY (#2792 round-4). This
            // used to end "the endpoint will REFUSE this
            // request". On the live dogfood run validating the
            // anchored estimator it printed exactly that for a
            // request the endpoint then counted at 27,745 against
            // the same 32,000 window — a proven falsehood, and
            // the second diagnostic on this path to state a
            // certainty its own input cannot support. The
            // estimate errs HIGH by design, which is right for
            // deciding to trim and wrong for predicting a
            // refusal, so the message reports what was measured
            // and what follows from it, conditionally.
            "darkmux-runtime: the next request ESTIMATES at ~{after_tokens} \
             tokens against the {window}-token window this profile DECLARES, \
             and nothing further can reduce it — {why}. This estimate errs \
             high on purpose so the bound never misses an overflow, so the \
             request may still fit; if the endpoint refuses it, raise the \
             profile's n_ctx or reduce what this role puts in context. (#2792)"
        );
    }

    // ─── the call ────────────────────────────────────────────────────────

    fn send(&mut self, bound: &CallBound) -> Result<Sent> {
        // (#2792 round-4) The characters this request carries, measured AFTER
        // the bound above may have trimmed them. Paired with the
        // `usage.prompt_tokens` the endpoint reports for this very request,
        // it becomes the next turn's anchor — the one exact number in the
        // whole estimate.
        let request_message_chars = {
            let (s_c, p_c) = measure_request_context(&self.messages);
            s_c + p_c
        };
        let request = ChatRequest {
            model: self.model.to_string(),
            messages: self.messages.clone(),
            tools: self.tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: Some(bound.wire_max_tokens),
            response_format: self.response_format.clone(),
        };
        // (#1221) `turns` has not been incremented yet, so a FRESH turn is
        // `turns + 1` — but a checkpoint continuation does not increment at
        // all, and stamping it `turns + 1` gave `model.partial` a sequence one
        // ahead of the `model.completed` it pairs with. Streaming is the
        // production default, so that mismatch is what the viewer normally
        // reads: every continuation's partials filed under a turn that does
        // not exist yet.
        // (#2229 round-2 blocker 2) Snapshot BEFORE the clear a few lines
        // below: `resuming_after_checkpoint` is a one-shot latch consumed at
        // the top of this iteration, so by the time the `"tool_calls"` arm
        // runs it is always false and cannot answer "was this call a
        // continuation?". The stall budget's decay is TURN-granular and this
        // is the only variable that says whether this call opened a new turn
        // (`turns` incremented, `turn.begin()` ran) or resumed the current
        // one. Read-only from here down.
        let next_seq = request_seq(&self.state);
        let (response, runtime_cut, cut_estimate) = if self.limits.streaming {
            let outcome = run_streaming_turn(
                self.client,
                &request,
                next_seq,
                self.trajectory,
                &mut self.last_proof_of_work,
                &mut self.state.soft_warning_fired,
                Watch {
                    interval: bound.per_call_cap,
                    carried: self.turn.carried(),
                    tick: STREAM_TICK,
                },
            )?;
            (outcome.response, outcome.cut, outcome.estimated_completion_tokens)
        } else {
            // (#2836) Nothing to observe on a non-streamed call: the whole
            // response arrives at once, so the runtime never has the chance
            // to end it early. The server is the only possible cutter here,
            // and it stays that way through Stage 1.
            (self.client.chat(&request)?, CutSource::None, None)
        };
        // (#1221) A checkpoint continuation is the SAME logical turn resuming,
        // so it must not consume a turn. It is a new API CALL, which is why
        // this used to increment — but `turns` is what `max_turns` is checked
        // against, so counting continuations silently divides the operator's
        // turn budget by the number of checkpoints: with a 1000-token interval,
        // one long reasoning turn spent thirteen of them. The knob would mean
        // something different depending on an unrelated interval setting.
        let opened_a_new_turn = self.state.begin_call();
        if opened_a_new_turn {
            // (#1221) A new logical turn is a new thought, so the previous
            // turn's accumulation goes — AND the prefill message that carried
            // it, which is the half this used to forget. Clearing the index
            // alone orphaned a raw `<think>` block in history that nothing
            // could reconstruct an answer from, so `main.rs` handed that markup
            // over as the deliverable. `begin` does both or neither.
            self.turn.begin(&mut self.messages);
            // (#2171) A fresh turn gets a fresh generation-continuation
            // budget — the cap bounds how long ONE turn may keep hitting the
            // generation check-in, not the whole dispatch.
        }
        Ok(Sent { response, runtime_cut, cut_estimate, request_message_chars, opened_a_new_turn })
    }

    // ─── reading the response ────────────────────────────────────────────

    fn read_response(&mut self, bound: CallBound, sent: Sent) -> Result<Call> {
        let Sent { mut response, runtime_cut, cut_estimate, request_message_chars, opened_a_new_turn } = sent;
        self.promote_plain_text_calls(&mut response);
        let separate_field_reasoning_before_strip = strip_reasoning(&mut response);
        let completion_tokens = self.fold_usage(&response, cut_estimate, request_message_chars);
        let plan = self.plan_calls(&response, runtime_cut, completion_tokens, &bound, cut_estimate);
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
        // (#2836, #2963) `cut` and `salvaged_per_turn_cap` were read above,
        // before `model.completed`; `finish_reason` is that record's.
        debug_assert_eq!(finish_reason, plan.finish_reason);
        let (per_turn_reasoning, call_had_dispatchable_output) =
            self.observe_reasoning(&assistant_message, separate_field_reasoning_before_strip.as_deref());
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
        //
        // (#2836) `is_ours_confirmed` is the same reading this used to
        // compute inline, named: an absent `usage` answers NO here, because
        // salvaging DISPATCHES the tool calls that were in flight, and an
        // unproven "our cap cut it" costs a truncated call reaching a real
        // tool. The cap-cliff below asks the other question of the same
        // uncertainty and gets the opposite answer, on purpose.
        // (#2963) `salvaged_per_turn_cap` is read above, before
        // `model.completed`, from this same message.
        if plan.salvaged {
            self.salvage(&mut assistant_message, &bound, plan.cut, completion_tokens);
        }
        let effective_finish_reason =
            resolve_finish_reason(finish_reason.as_str(), has_tool_calls(&assistant_message), plan.salvaged)
                .to_string();
        self.land_message(&mut assistant_message, &effective_finish_reason, plan.salvaged);
        self.note_missing_reasoning_region(&per_turn_reasoning, call_had_dispatchable_output);
        Ok(Call {
            bound,
            opened_a_new_turn,
            cut: plan.cut,
            cut_estimate,
            completion_tokens,
            call_plan: plan.call_plan,
            planned_tool_calls: plan.tool_calls,
            assistant_message,
            per_turn_reasoning,
            effective_finish_reason,
        })
    }

    fn promote_plain_text_calls(&mut self, response: &mut crate::lmstudio::ChatResponse) {
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
        let promotion = response.choices.first_mut().map(|choice| {
            let outcome = promote_plain_text_tool_calls(&mut choice.message, &self.allowed_tool_names);
            if outcome.info.is_some() {
                choice.finish_reason = "tool_calls".to_string();
            }
            outcome
        });
        // (#2230) Both arms are recorded. A promotion carries its skip count so
        // PARTIAL suppression is visible next to what did run; a non-promotion
        // that skipped openers gets its own event, because TOTAL suppression is
        // the case that is otherwise indistinguishable from a turn that emitted
        // no call at all. Silence on either arm is what would leave a wrong
        // fence verdict undiagnosable after the fact.
        let Some(outcome) = promotion else {
            return;
        };
        match outcome.info {
            Some(info) => self.trajectory.append_tool_call_promoted(
                self.state.turns,
                info.source.as_str(),
                info.format.as_str(),
                info.call_count,
                outcome.xml_openers_skipped_as_fenced,
            ),
            None if outcome.xml_openers_skipped_as_fenced > 0 => self
                .trajectory
                .append_tool_call_promotion_suppressed(
                    self.state.turns,
                    outcome.xml_openers_skipped_as_fenced,
                ),
            None => {}
        }
    }

    /// Returns this call's completion-token count, when the endpoint gave one.
    fn fold_usage(
        &mut self,
        response: &crate::lmstudio::ChatResponse,
        cut_estimate: Option<u32>,
        request_message_chars: usize,
    ) -> Option<u32> {
        // (#414 PR A) Capture this turn's completion-token count BEFORE
        // it folds into the cumulative total, so the stall-recovery
        // branch below can record it in the trajectory event. Kept as
        // Option so an absent-usage response (rare) is distinguishable
        // from a legitimate zero in the trajectory event — the event's
        // purpose is to discriminate per-call-cap stalls (count ≈
        // MAX_TOKENS_PER_CALL) from context-overflow stalls, so the
        // distinction matters.
        let usage = response.usage.as_ref();
        let this_turn_completion_tokens: Option<u32> = usage.and_then(|u| u.completion).map(saturating_u32);
        // (B1) A turn the runtime cut has no endpoint count, but its tokens
        // were spent: the run's total and the cumulative cap take the
        // runtime's own estimate. The trajectory keeps the two apart
        // (`usage: null` plus `completion_estimate`), so nothing downstream
        // reads the estimate as a reported figure.
        self.state.fold_completion(this_turn_completion_tokens, cut_estimate);
        // The prompt count is the ground truth everything below calibrates
        // against, so all of it needs one the endpoint actually reported.
        if let Some(prompt_tokens) = usage.and_then(|u| u.prompt).map(saturating_u32) {
            // (#854) Track endpoint staleness BEFORE overwriting the running
            // value: a count identical to last turn (while the thread grew)
            // means the endpoint froze it. Deliberately inside the reported-prompt
            // arm: a usage-less turn (e.g. streaming without include_usage) is
            // BRIDGED — it neither increments nor resets the counter, so it
            // can't corrupt the run of identical reports. Don't "fix" this into
            // an unconditional reset; that would zero the counter on every
            // usage-less turn and defeat the detector.
            // (#2792 round-4) Ground truth for the request that just went
            // out, paired with the characters it carried. Everything the
            // local ruler cannot see — the chat template's per-message
            // envelope, the tools schema, this model's actual tokenization —
            // is inside this number, so the next turn estimates only what it
            // adds on top.
            self.state.fold_prompt_count(prompt_tokens, request_message_chars);
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
            self.trajectory.append_context_window(
                self.state.turns,
                self.state.latest_prompt_tokens,
                self.cfg.context_window,
            );
        }
        this_turn_completion_tokens
    }

    fn plan_calls(
        &mut self,
        response: &crate::lmstudio::ChatResponse,
        runtime_cut: CutSource,
        completion_tokens: Option<u32>,
        bound: &CallBound,
        cut_estimate: Option<u32>,
    ) -> Plan {
        // Record model.completed for trajectory. We grab the first
        // choice's finish_reason + tool_calls below; mirror it here.
        let first = response.choices.first();
        let finish_reason = first.map(|c| c.finish_reason.clone()).unwrap_or_default();
        let tool_calls = first.and_then(|c| c.message.tool_calls.as_ref()).cloned();
        // (#2836) Who ended this call. Two predicates below — the #479
        // salvage and the #1221 cap-cliff — used to answer that by comparing
        // `completion_tokens` against `per_call_cap` inline, resolving an
        // absent `usage` in OPPOSITE directions four hundred lines apart with
        // no name on the distinction. `CutSource` carries the reading once.
        //
        // The comparison stays against `per_call_cap` — what THIS request
        // actually sent — deliberately. An earlier attempt keyed it to
        // `answer_max_tokens`, which stops recognizing a check-in cut as ours
        // and drops well-formed tool calls that should have dispatched.
        //
        // (#2963) Read here, before `model.completed`, because the record
        // says which calls will run and that depends on it.
        let cut = match runtime_cut {
            CutSource::RuntimeAbort(_) => runtime_cut,
            _ => CutSource::classify(&finish_reason, completion_tokens, bound.wire_max_tokens),
        };
        // (#479) Per-turn-cap salvage — see the block that acts on it below
        // for the full reasoning. (#2836) `is_ours_confirmed`: an absent
        // `usage` answers NO here, because salvaging DISPATCHES the tool
        // calls that were in flight.
        let salvaged = finish_reason == "length"
            && cut.is_ours_confirmed()
            && first.is_some_and(|c| assistant_message_has_well_formed_tool_calls(&c.message));
        // (#2963) Which of this turn's calls will RUN, decided ONCE, here,
        // and followed by the dispatch below (`partition_by_plan`): the
        // record and the loop cannot disagree. A call that will not run —
        // ungranted, not a tool, cut off mid-arguments, or on a turn that
        // dispatches nothing — is marked `runs: false` on `model.completed`,
        // so the viewer never names it as the call running now.
        let dispatches_tool_calls = resolve_finish_reason(
            &finish_reason,
            tool_calls.as_ref().is_some_and(|t| !t.is_empty()),
            salvaged,
        ) == "tool_calls";
        let call_plan = plan_tool_calls(
            tool_calls.as_deref().unwrap_or(&[]),
            dispatches_tool_calls,
            salvaged,
            &self.allowed_tool_names,
        );
        let call_runs: Vec<bool> = call_plan.iter().map(|f| *f == CallFate::Runs).collect();
        self.trajectory.append_model_completed(
            self.state.turns,
            &finish_reason,
            crate::trajectory::CallTokens {
                reported: response.usage.as_ref(),
                estimate: cut_estimate.map(u64::from),
            },
            tool_calls.as_deref(),
            Some(&call_runs),
            response.served_model(),
        );
        Plan { finish_reason, tool_calls, cut, salvaged, call_plan }
    }

    /// Returns this call's reasoning text and whether it produced real,
    /// dispatchable output.
    fn observe_reasoning(&mut self, message: &Message, separate_field: Option<&str>) -> (String, bool) {
        // Extract reasoning content from `<think>...</think>` blocks in
        // the assistant message content (#204). Thinking-mode models
        // (qwen 3.x line, in particular) emit reasoning inline; we
        // surface it as a separate trajectory event so the flow
        // stream + viewer can render it as a collapse/expand block
        // (operator discretion to expand). The original content stays
        // unchanged in `assistant_message` — downstream consumers
        // (compaction, conversation history) see everything as-was.
        let mut per_turn_reasoning = String::new();
        if let Some(content) = message.content.as_deref() {
            for reasoning_text in extract_think_blocks(content) {
                self.trajectory.append_model_reasoning(
                    self.state.turns,
                    &reasoning_text,
                    "inline-think-tags",
                );
                per_turn_reasoning.push_str(&reasoning_text);
                per_turn_reasoning.push('\n');
            }
        }
        if let Some(separate) = message.reasoning_content.as_deref() {
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
        if call_reasoned(&per_turn_reasoning, separate_field, message.content.as_deref()) {
            self.state.dispatch_has_reasoned = true;
        }
        // (#2164) Captured HERE, before salvage or any other mutation below
        // clears `content` — whether THIS call produced real, dispatchable
        // output (an answer or tool calls), independent of whether it also
        // reasoned. Feeds the one-shot "this model emits no reasoning
        // region" detector after the response is fully processed.
        let call_had_dispatchable_output = has_dispatchable_output(message);
        // (#461) Feed the combined reasoning to the loop detector. The
        // detector skips empty / too-short reasoning internally so
        // turns without reasoning content don't pollute the window.
        if let Some(ReasoningLoopSignal::Suspected { count, window_size }) =
            self.reasoning_loop_detector.record(&per_turn_reasoning)
        {
            eprintln!(
                "darkmux-runtime: ⚠ reasoning-loop suspected — same reasoning content \
                 appeared {} times in {} turns. Queueing feedback nudge.",
                count, window_size
            );
            self.trajectory.append_reasoning_loop_suspected(self.state.turns, count, window_size);
            self.feedback_injector.queue_reasoning_loop(count, window_size);
        }
        (per_turn_reasoning, call_had_dispatchable_output)
    }

    fn salvage(&mut self, assistant_message: &mut Message, bound: &CallBound, cut: CutSource, completion_tokens: Option<u32>) {
        // (#2169 merge-gate CONSIDER 6) `salvaged_count` measures ONLY
        // JSON well-formedness (#479's own filter) — it is computed
        // BEFORE the #2169 name-allowlist partition runs in
        // `handle_tool_calls`, the shared `"tool_calls"` arm every source
        // of `calls` (including this salvage) routes through. A call
        // counted here as "salvaged" can still turn out to be
        // invalid-name or ungranted and never actually dispatch — the
        // two counts are ANSWERING DIFFERENT QUESTIONS ("how many
        // survived JSON truncation" vs "how many were real, granted
        // tools"), not duplicating each other. Reconcile them by
        // reading the SAME turn's `dispatch.tool.malformed_names`
        // event(s) (same `seq`) alongside this one: `salvaged_count -
        // (sum of that turn's malformed `count` fields)` is what
        // actually reached `tools::dispatch`.
        let salvaged_count = count_well_formed_tool_calls(assistant_message);
        let per_call_cap = bound.per_call_cap;
        let observed_tokens = completion_tokens.unwrap_or(per_call_cap);
        // (#2165) Which bound was actually hit — the #1221 reasoning
        // check-in interval or `max_tokens_per_call` — so a remote
        // reader never has to reconstruct it from memory of the design
        // (the miss this whole feature exists to close).
        let cut_bound = bound.cut_bound(cut);
        eprintln!(
            "darkmux-runtime: ⚡ per-turn-cap salvage — completion_tokens=\
             {} hit {}; dispatching {} well-formed tool call(s) and \
             nudging the model to reduce per-call reasoning.",
            observed_tokens, cut_bound.describe(), salvaged_count
        );
        self.trajectory.append_per_turn_cap_salvaged(
            self.state.turns,
            observed_tokens,
            per_call_cap,
            salvaged_count,
            cut_bound,
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
        //
        // (#2171) `!sent_generation_bound` extends the same guard to the
        // GENERATION check-in — a routine 4000-token generation
        // check-in is exactly as silent to the model as a routine
        // reasoning check-in always was. Telling a non-reasoning model
        // to "reduce its reasoning" would be an instruction it was never
        // disobeying, the same defect #2166 fixed for turn-1 calls.
        if salvage_nudges(bound) {
            self.feedback_injector
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
        retain_well_formed_tool_calls(assistant_message);
    }

    fn land_message(&mut self, assistant_message: &mut Message, effective_finish_reason: &str, salvaged: bool) {
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
        match turn_end(effective_finish_reason, salvaged) {
            TurnEnd::Fold => self.turn.fold(&mut self.messages, assistant_message),
            // Mid-turn: the prefill is superseded by the message about to be
            // pushed, but the accumulation lives on. Removing the message
            // WITHOUT folding is the whole distinction — see `supersede`.
            TurnEnd::Supersede => self.turn.supersede(&mut self.messages),
            TurnEnd::Continue => {}
        }
        self.messages.push(assistant_message.clone());
    }

    fn note_missing_reasoning_region(&mut self, per_turn_reasoning: &str, call_had_dispatchable_output: bool) {
        // (#2164) One-shot detector: this dispatch has, cumulatively, never
        // shown a reasoning region — and this call, which produced real
        // dispatchable output, didn't either. Surfaced once so the run
        // record explains why the reasoning check-in bound stops applying
        // to fresh turns' first calls (`dispatch_has_reasoned` above),
        // rather than leaving that inference to whoever reads the
        // trajectory later.
        if !reports_no_reasoning_region(&self.state, per_turn_reasoning, call_had_dispatchable_output) {
            return;
        }
        let turns = self.state.turns;
        eprintln!(
            "darkmux-runtime: this model has produced no reasoning region \
             in {turns} turn(s) so far — the reasoning check-in bound no \
             longer applies to a fresh turn's first call; only the answer \
             bound does. (#2164)"
        );
        self.trajectory.append_reasoning_bound_not_applied(turns);
        self.state.no_reasoning_region_logged = true;
    }

    fn route(&mut self, call: Call) -> Result<Option<LoopOutcome>> {
        match call.effective_finish_reason.as_str() {
            "stop" => {
                let final_answer = self.turn.pending_answer();
                Ok(Some(self.finish(final_answer, TerminalReason::Stop)))
            }
            "tool_calls" => Ok(self.handle_tool_calls(&call)),
            "length" => self.handle_length(&call),
            other => Err(anyhow!(
                "unexpected finish_reason: {other} — runtime doesn't know \
                 how to handle this. Aborting."
            )),
        }
    }

    // ─── the tool_calls arm ──────────────────────────────────────────────

    fn handle_tool_calls(&mut self, call: &Call) -> Option<LoopOutcome> {
        let calls = call
            .assistant_message
            .tool_calls
            .clone()
            .unwrap_or_default();
        if calls.is_empty() {
            return self.handle_empty_tool_calls(call);
        }
        // (#2169) Partition BEFORE any dispatch: a structured call
        // whose `name` isn't dispatchable — either not a real tool,
        // or a real tool this dispatch wasn't GRANTED — is never
        // executed, never checkpointed as pending, and never
        // reaches the cycle/failure-rate detectors. See
        // `partition_by_plan`'s doc for why this has to
        // happen here (before `calls_snapshot`) rather than inside
        // the dispatch loop below, for how this composes with
        // #479's per-turn-cap salvage, and for why the two invalid
        // buckets are kept separate (merge-gate finding: they used
        // to be one bucket, which both mislabeled a real-tool
        // permission refusal as "looks like quoted code" and mixed
        // it into the Devstral-pattern metric).
        // (#2963) By the plan made before `model.completed`, so the
        // calls that run are exactly the ones that record left
        // unmarked. `calls` above (the message's, after the #479
        // salvage dropped the malformed ones) is the plan's
        // `Runs`/`NotGranted`/`NotATool` calls in the same order.
        debug_assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            call.planned_tool_calls
                .iter()
                .flatten()
                .zip(&call.call_plan)
                .filter(|(_, f)| **f != CallFate::Discarded)
                .map(|(c, _)| c.id.as_str())
                .collect::<Vec<_>>(),
            "the plan must cover exactly the calls this arm sees"
        );
        let (calls, ungranted_calls, not_a_tool_calls) =
            partition_by_plan(call.planned_tool_calls.clone().unwrap_or_default(), &call.call_plan);
        self.refuse_invalid_calls(&ungranted_calls, MalformedReason::RealToolNotGranted);
        self.refuse_invalid_calls(&not_a_tool_calls, MalformedReason::NotATool);
        // (#2169 merge-gate finding 4) A turn whose tool_calls were
        // ALL invalid/ungranted (`calls` here is the post-partition
        // DISPATCHABLE list — empty means nothing this turn ran)
        // makes zero progress: no tool executed, nothing for the
        // #419/#418 detectors to see, no proof-of-work signal. Left
        // unbounded, N such turns in a row under default config
        // (max_turns/max_cumulative_tokens both `None`, #419 is
        // warn-only, `model.partial` heartbeats keep the host
        // watchdog alive regardless) spin forever with no operator
        // signal beyond a repeating detector line — this fix makes
        // that spin QUIETER than the pre-#2169 one-tool-message-
        // per-call shape, so it needs its own bound. Reset-on-any-
        // dispatched-call, edge-triggered at `MAX_CONSECUTIVE_
        // MALFORMED_TURNS`. NOT the same shape as
        // `stall_recoveries_used` just below (#2229): that counter
        // has TWO increment sites straddling the turn boundary, so
        // it PAYS DOWN by one on a turn that opened, rather than
        // resetting to zero on any dispatched call. Do not
        // "harmonize" the two — zeroing the stall budget is
        // exactly the regression #2229's review caught. Deliberately NOT restored on resume (same
        // "detector state resets fresh" precedent `RunCheckpoint::
        // pending_tool_calls`'s doc names for cycle_detector/
        // failure_rate_detector) — a resume that lands mid-spin
        // gets a fresh budget rather than inheriting a partial
        // count from a checkpoint that predates this field.
        // (#2229) The stall budget pays DOWN on the same signal,
        // in the same place — but it is not the same shape as
        // the counter above, and the comment that first landed
        // here claimed it was. `consecutive_malformed_turns`
        // increments in THIS arm and resets in THIS arm, so it
        // is genuinely arm-local and `= 0` is right for it.
        // `stall_recoveries_used` increments in TWO arms (the
        // empty-tool-calls recovery just above, and the
        // `"length"` arm via `recover_intra_turn_stall`) and
        // pays down only here. Two differences follow:
        //
        //   1. DECAY, not zero. `MAX_STALL_RECOVERIES`' doc
        //      promised a CONSECUTIVE budget while the counter
        //      had no pay-down site at all — one `= 0` init and
        //      the `saturating_add` bumps — so a dispatch that
        //      recovered on turn 5, ran a thousand productive
        //      turns, then stalled again on turn 1005 escalated
        //      out with the counter still at 1/2. That is
        //      #2229's bug. But a hard `= 0` over-corrects: it
        //      bounds only BACK-TO-BACK stalls, so ONE
        //      dispatched call between two stalls wipes the
        //      counter. `saturating_sub(1)` forgives the
        //      isolated stall and still lets any sustained rate
        //      above 1:1 climb to the bound. The exactly-1:1
        //      alternation stays unbounded here by design — see
        //      `MAX_STALL_RECOVERIES`' doc for the measured
        //      shape and why closing it is deferred rather than
        //      forgotten.
        //
        //   2. TURN-granular, and this site is not that on its
        //      own. After `recover_intra_turn_stall` the
        //      `"length"` arm sets `resuming_after_checkpoint =
        //      turn.has_prefill()`, and a continuation does NOT
        //      increment `turns` — so a mid-turn continuation
        //      that returns a real tool call lands right here
        //      and would refund a recovery the SAME turn spent
        //      moments earlier. Proven: turn 1 checkpoints,
        //      stalls (budget 1/2), then dispatches, and the
        //      escalation slides from turn 3 to turn 4 with turn
        //      2 logging 1/2 where it should log 2/2.
        //      `opened_a_new_turn` is what makes the granularity
        //      real. (The earlier rejection note here claimed
        //      the length arm's CHECKPOINT branch was the only
        //      mid-turn risk and that this site was already
        //      whole-turn — both false; the checkpoint branch
        //      never touches this counter, and this site is
        //      reached mid-turn.)
        //
        // The `"stop"` arm still needs nothing: it returns.
        let tally = tally_dispatch(&self.state, !calls.is_empty(), call.opened_a_new_turn);
        self.state.consecutive_malformed_turns = tally.consecutive_malformed_turns;
        self.state.stall_recoveries_used = tally.stall_recoveries_used;
        if tally.escalate {
            let consecutive_malformed_turns = tally.consecutive_malformed_turns;
            eprintln!(
                "darkmux-runtime: escalation_triggered — {consecutive_malformed_turns} \
                 consecutive turns produced ONLY invalid/ungranted tool-call names, \
                 none dispatched, no progress possible. Emitting EscalationTriggered \
                 for frontier handoff. (#2169)"
            );
            let final_answer = self.turn.pending_answer();
            return Some(self.escalate(EscalationReason::MalformedToolCallsExhausted, final_answer));
        }
        self.dispatch_calls(calls);
        // Loop back and call chat() again, unless compaction ends the dispatch.
        self.trim_and_compact(call.bound.wire_max_tokens)
    }

    fn refuse_invalid_calls(&mut self, calls: &[ToolCall], reason: MalformedReason) {
        handle_invalid_tool_calls(
            calls,
            reason,
            &self.allowed_tool_names,
            self.model,
            self.state.turns,
            self.trajectory,
            &mut self.feedback_injector,
            &mut self.messages,
        );
    }

    fn handle_empty_tool_calls(&mut self, call: &Call) -> Option<LoopOutcome> {
        // (#1123) finish_reason=tool_calls but no tool_calls — an
        // empty/useless completion (observed: a degraded run on
        // devstral-24b returned a wholly empty message under this
        // finish_reason — no content, no reasoning, no calls).
        // Pre-#1123 this hard-killed the dispatch.
        //
        // (#2190) This shape is NOT the runaway-reasoning stall
        // the length-arm recovers — it's a protocol-shaped miss
        // (the model claimed a tool call and produced none), a
        // DIFFERENT failure with a different cause. It shares
        // the SAME bounded recovery mechanism (drop the useless
        // turn + nudge + retry, bounded by the stall budget,
        // escalating when exhausted) but gets its own detector
        // kind and escalation reason
        // (`EscalationReason::EmptyToolCallsExhausted`) rather
        // than borrowing `IntraTurnStallExhausted` — conflating
        // the two sent a live diagnosis down the wrong path
        // twice (measured: the dropped turns were 286-648
        // completion tokens, nowhere near any configured bound,
        // so "runaway reasoning" was factually wrong).
        if stall_budget_exhausted(&self.state, &self.limits) {
            return Some(self.escalate_empty_tool_calls());
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
        let useless = self.messages.last().is_some_and(has_no_text);
        if useless {
            self.messages.pop();
        }
        self.state.stall_recoveries_used = self.state.stall_recoveries_used.saturating_add(1);
        let bound = call.bound.cut_bound(call.cut);
        let (turns, stall_recoveries_used, stall_recovery_budget) =
            (self.state.turns, self.state.stall_recoveries_used, self.limits.stall_recovery_budget);
        self.trajectory.append_empty_tool_calls_recovered(
            turns,
            call.completion_tokens,
            stall_recoveries_used,
            stall_recovery_budget,
            bound,
        );
        self.messages.push(Message::system(STALL_NUDGE_MESSAGE));
        let kept = if useless { "Dropped the useless turn" } else { "KEPT the turn's work" };
        eprintln!(
            "darkmux-runtime: ⏸ empty-tool-calls turn recovered — turn {turns} \
             returned finish_reason=tool_calls with no tool calls. {kept}, \
             injected a nudge; budget \
             {stall_recoveries_used}/{stall_recovery_budget} used, hit {}. (#2190)",
            bound.describe()
        );
        None
    }

    fn escalate_empty_tool_calls(&mut self) -> LoopOutcome {
        // (#1221) Drop the empty message before handing off.
        // `main.rs` takes the LAST assistant message as the
        // deliverable, and this arm pushes a wholly empty one
        // every time it fires — so escalating with it still in
        // place buries the turn's real work behind a blank
        // message and the operator receives nothing. The
        // recovery below already drops it; the escalation
        // returned before reaching that.
        if self.messages.last().is_some_and(is_blank_assistant) {
            self.messages.pop();
        }
        let (stall_recoveries_used, stall_recovery_budget) =
            (self.state.stall_recoveries_used, self.limits.stall_recovery_budget);
        // (#2229) The budget is a stall RATE, not a lifetime
        // count — it decays by one per productive turn — so
        // the line says so rather than letting an operator
        // read `{stall_recoveries_used}` as "this dispatch
        // stalled exactly that many times in total".
        eprintln!(
            "darkmux-runtime: escalation_triggered — the model returned \
             finish_reason=tool_calls with no tool calls, and the empty- \
             tool-calls recovery budget \
             ({stall_recoveries_used}/{stall_recovery_budget}, decays by \
             one per productive turn) is exhausted; the nudge isn't \
             breaking the pattern. Emitting EscalationTriggered for \
             frontier handoff. (#2190/#2229)"
        );
        let final_answer = self.turn.pending_answer();
        self.escalate(EscalationReason::EmptyToolCallsExhausted, final_answer)
    }

    fn dispatch_calls(&mut self, calls: Vec<ToolCall>) {
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
        self.compaction_repeat.record_turn(crate::compaction_repeat::inspected_set(&calls_snapshot));
        for (tool_seq, call) in calls.into_iter().enumerate() {
            self.dispatch_one(&calls_snapshot, tool_seq, call);
        }
    }

    fn dispatch_one(&mut self, calls_snapshot: &[ToolCall], tool_seq: usize, call: ToolCall) {
        self.watch_for_cycle(&call);
        let run = dispatch_marked(&self.tool_start(&calls_snapshot[tool_seq..], tool_seq as u32), &call, dispatch);
        let result = run.result;
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
        self.record_failed_to_run(&call, &result);
        self.trajectory.append_tool_completed(
            self.state.turns,
            tool_seq as u32,
            &call.function.name,
            &call.function.arguments,
            &result,
            &outcome,
            run.emitted.as_ref(),
            run.emit_seq,
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
            self.prove_work();
        }
        self.track_cadence(&call);
        self.watch_for_failure_cascade(&call, &result);
        self.messages.push(Message::tool_result(
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
        // boundary. See the pre-loop resume block
        // (`resume_catch_up`) for the other half: dispatching
        // exactly these calls, and none already recorded,
        // when a resumed checkpoint carries them.
        // (#2114 finding N6) A fresh (non-resumed) turn's
        // calls always start at tool_seq 0, so the next
        // pending call's seq is simply tool_seq + 1.
        let (pending, seq_base) = pending_after(calls_snapshot, tool_seq, tool_seq as u32 + 1);
        self.write_checkpoint(None, pending, seq_base);
    }

    fn watch_for_cycle(&mut self, call: &ToolCall) {
        // (#418) Record the call into the cycle detector
        // BEFORE dispatch so the suspicion event lands
        // immediately next to the tool.completed event in
        // trajectory order. Edge-triggered: same hash
        // continuing to repeat does NOT re-fire.
        let Some(CycleSignal::Suspected {
            tool_name,
            canonical_args,
            count,
            window_size,
        }) = self.cycle_detector.record(&call.function.name, &call.function.arguments)
        else {
            return;
        };
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
        self.trajectory.append_cycle_suspected(
            self.state.turns,
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
        self.feedback_injector.queue_cycle_suspected(
            &tool_name,
            count,
            window_size,
        );
    }

    /// (#799) A verifier that never RAN (vs ran-and-failed) is the
    /// trust-critical class — stamp it so a SIGNOFF claiming it passed can be
    /// mechanically contradicted at the gate.
    fn record_failed_to_run(&mut self, call: &ToolCall, result: &str) {
        let Some(reason) = crate::failure_rate::classify_failed_to_run(&call.function.name, result) else {
            return;
        };
        // Best-effort display text: the parsed `command` field,
        // falling back to the raw args. The gate treats it as
        // advisory (what the model asked to run), not a
        // re-parseable command.
        let command = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
            .ok()
            .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_else(|| call.function.arguments.clone());
        self.failed_to_run.push(FailedExec {
            command,
            reason: reason.to_string(),
        });
    }

    fn track_cadence(&mut self, call: &ToolCall) {
        // (#465) Track test-cadence drift via same-file
        // repetition. See state-machine doc above the
        // declaration of `last_edited_path` for full
        // rationale. Edge-triggered: counter + path reset
        // after firing so the next nudge requires another
        // THRESHOLD consecutive same-file edits.
        let cadence = cadence_after_call(&self.state, &call.function.name, &call.function.arguments);
        self.state.last_edited_path = cadence.last_edited_path;
        self.state.consecutive_same_file_edits = cadence.consecutive_same_file_edits;
        if let Some(fired_path) = cadence.fired {
            eprintln!(
                "darkmux-runtime: ⚠ test-cadence drift — {} \
                 consecutive edits to `{}` without a bash \
                 verification call. Queueing feedback nudge.",
                TEST_CADENCE_DRIFT_THRESHOLD, fired_path
            );
            self.feedback_injector.queue_test_cadence_drift(
                TEST_CADENCE_DRIFT_THRESHOLD,
                &fired_path,
            );
        }
    }

    fn watch_for_failure_cascade(&mut self, call: &ToolCall, result: &str) {
        // (#419) Record into the failure-rate detector
        // AFTER dispatch so the result is available to
        // classify. Edge-triggered: a signature's counter
        // resets when that signature next succeeds, warn
        // fires once per cascade.
        let Some(FailureCascadeSignal::Suspected {
            tool_name,
            failure_count,
            reason,
        }) = self.failure_rate_detector.record(
            &call.function.name,
            &call.function.arguments,
            result,
        )
        else {
            return;
        };
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
        self.trajectory.append_tool_repeated_failure(
            self.state.turns,
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
        self.feedback_injector.queue_tool_failure_cascade(
            &tool_name,
            failure_count as usize,
            &reason,
        );
    }

    // ─── compaction ──────────────────────────────────────────────────────

    /// The context check after a tool turn. `Some` ends the dispatch.
    fn trim_and_compact(&mut self, wire_max_tokens: u32) -> Option<LoopOutcome> {
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
            crate::tool_result_prune::soft_trim_old_tool_results(&mut self.messages);
        if trim_stats.results_trimmed > 0 {
            eprintln!(
                "darkmux-runtime: soft-trimmed {} old tool result(s), reclaiming {} bytes \
                 of transcript before the compaction check (#1391)",
                trim_stats.results_trimmed, trim_stats.bytes_reclaimed
            );
        }
        // (#854) When the endpoint's reported count is stale (frozen
        // across turns while the thread grew), it can't gate compaction
        // — it silently suppressed it into a degenerate cycle. A local
        // chars/4 size estimate stands in as the EFFECTIVE occupancy
        // and the SAME threshold decides; even when stale it only
        // compacts if real occupancy actually warrants it (no needless
        // compaction if the conversation genuinely plateaued). The
        // endpoint misreport is surfaced regardless.
        //
        // (#2792 — supersedes this entry's original scope) That text
        // used to add "this changes nothing in normal operation
        // (frozen=0 → effective == reported)". That is no longer true
        // and the sentence is removed rather than left to rot: the
        // estimate is now computed unconditionally, so `effective` can
        // exceed `reported` on any turn, which is the whole point.
        // What #854 still owns exclusively is the SIGNAL below — the
        // stale-count eureka is emitted only at the staleness crossing.
        //
        // (#2792) The local estimate is computed on EVERY turn, not
        // only on the stale path above. `latest_prompt_tokens` is the
        // count the endpoint reported for the request that ALREADY
        // WENT OUT — it predates the tool results appended since, so
        // gating compaction on it alone decides using a number that
        // describes a smaller conversation than the one about to be
        // sent. Measured: a turn reporting 6,290 was followed, after
        // one large tool result, by a request of 38,446 against a
        // declared 32,000 window. Nothing compacted in between,
        // because 6,290 is below the trigger.
        //
        // The consequence is not cosmetic. On a model loaded at the
        // window its profile DECLARES, LMStudio rejects the oversized
        // request outright — HTTP 400, "the number of tokens to keep
        // from the initial prompt is greater than the context length"
        // — so the dispatch dies. The overshoot was invisible in
        // testing only because the loaded instance happened to be
        // larger than the declared window.
        //
        // The estimator was already here and already trusted for this
        // exact decision; it was fenced behind the staleness case. The
        // `max` keeps the endpoint's own count authoritative whenever
        // it is the larger number, so this can only ever compact
        // EARLIER than before, never later.
        let effective_prompt_tokens =
            effective_prompt_occupancy(&self.messages, self.state.latest_prompt_tokens, self.state.prompt_anchor);
        // (#854) The endpoint's reported count going STALE — frozen
        // across turns while the thread grew — is surfaced as its own
        // signal, once at the crossing. It no longer gates the
        // compaction decision (the estimate above is unconditional
        // now), so this is purely the eureka: the endpoint is
        // misreporting, and that is worth telling the operator even
        // though darkmux now compacts correctly regardless.
        if stale_count_crossed(&self.state) {
            self.report_stale_prompt_count();
        }
        // Phase 6: check whether the most recent prompt's
        // token count crossed the compaction threshold, AND
        // whether the conversation is long enough to compact.
        // If so, compact BEFORE the next chat() call so the
        // next request sees a smaller message thread.
        if !compaction::needs_compaction(
            effective_prompt_tokens,
            self.messages.len(),
            self.cfg,
        ) {
            // (#2793) A turn that did not compact at all ends the
            // episode: the counter tracks a CONSECUTIVE run of
            // unproductive compactions, and an intervening turn that
            // needed none means the thread came back under the line
            // on its own. Without this the count would carry across a
            // resolved episode and fire early on the next one.
            self.unproductive_compactions.end_episode();
            return None;
        }
        self.compact_thread(wire_max_tokens, effective_prompt_tokens)
    }

    fn report_stale_prompt_count(&mut self) {
        let (latest_prompt_tokens, frozen_prompt_turns) =
            (self.state.latest_prompt_tokens, self.state.frozen_prompt_turns);
        let (sys_chars, prompt_chars) = measure_request_context(&self.messages);
        let estimate = ((sys_chars + prompt_chars) / 4) as u32;
        eprintln!(
            "darkmux-runtime: the endpoint's context token count has been frozen \
             at {latest_prompt_tokens} for {frozen_prompt_turns} turns while the \
             message thread grew — substituting a local estimate ({estimate}) for \
             the compaction decision (the reported count can't gate it). (#854)"
        );
        self.trajectory.append_stale_context_tokens(
            self.state.turns,
            latest_prompt_tokens,
            frozen_prompt_turns,
            estimate,
            self.messages.len(),
        );
    }

    fn compact_thread(&mut self, wire_max_tokens: u32, effective_prompt_tokens: u32) -> Option<LoopOutcome> {
        let before_count = self.messages.len();
        let installed_summary_chars = self.attempt_compaction(wire_max_tokens, "compaction");
        // (#2792 merge-gate) Everything in this block describes a
        // compaction that ACTUALLY INSTALLED — the trajectory record,
        // the staleness-counter reset, the post-compaction nudge. A
        // refused attempt did none of those things to the thread, so
        // none of them may claim it did. The liveness stamp below is
        // deliberately OUTSIDE: a refused attempt still spent a real
        // compactor call, and it is real proof of work.
        if let Some(summary) = installed_summary_chars {
            if let Some(outcome) = self.after_installed_compaction(summary, before_count, effective_prompt_tokens) {
                return Some(outcome);
            }
        }
        // (#466) Compaction is a proof-of-work signal for
        // the inactivity-approach detector. Same trigger
        // set as #468 on the host-side reset.
        self.prove_work();
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
        if !compaction_bound_reached(self.state.compactions, self.cfg.bail_after_compactions) {
            return None;
        }
        let compactions = self.state.compactions;
        let bail = self.cfg.bail_after_compactions.unwrap_or_default();
        eprintln!(
            "darkmux-runtime: escalation_triggered — \
             compactions ({compactions}) reached bail_after_compactions ({bail}); \
             emitting EscalationTriggered terminal for frontier handoff"
        );
        let final_answer = self.turn.pending_answer();
        Some(self.escalate(EscalationReason::CompactionLimitReached, final_answer))
    }

    /// One compaction attempt, shared by the live loop and the resume
    /// catch-up. Returns the installed summary's `(chars, lexically_repaired)`,
    /// or `None` when the compactor's result was refused and skipped.
    /// `label` names the site in the refusal line.
    fn attempt_compaction(&mut self, max_tokens_per_call: u32, label: &str) -> Option<(usize, bool)> {
        // (#2792 merge-gate) `compactions` is incremented only
        // AFTER a compaction actually installs. It used to be
        // bumped here, before the attempt, so a refused
        // compaction still counted toward `bail_after_compactions`.
        let attempted_generation = self.state.compactions.saturating_add(1);
        // (#372 T2-C) Route by strategy. Narrative is
        // today's default (prose summary as synthetic
        // USER message). StructuredSlot is tier-2 (typed
        // schema + JSON mode + SYSTEM message); on
        // success the parsed output is persisted to
        // `<RUNTIME_OUT_BASE>/.darkmux-runtime/compaction-<gen>.json`
        // per #352 Step 5 "persistence falls out for free."
        // (#2915) Announce the attempt before the compactor is
        // called: the host shows "compacting" from this marker
        // until the calls' usage records land.
        self.trajectory.append_compaction_start(attempted_generation, self.cfg.compactor_model.as_deref());
        // (#2902 step 1b) Every compactor call that got a reply,
        // installed or refused, drained below into one
        // `compaction.call` event each.
        let mut compactor_calls = Vec::new();
        let summary_chars = match self.cfg.strategy {
            compaction::CompactionStrategy::Narrative => compaction::compact(
                self.compactor_client,
                &mut self.messages,
                attempted_generation,
                self.cfg,
                &mut compactor_calls,
            )
            .map(|chars| (chars, false)),
            compaction::CompactionStrategy::StructuredSlot => {
                self.structured_compaction(attempted_generation, max_tokens_per_call, &mut compactor_calls)
            }
        };
        for call in &compactor_calls {
            self.trajectory.append_compaction_call(call);
        }
        // (#2792 merge-gate) A compaction that cannot help must
        // not kill the dispatch.
        //
        // Both compactors return `Err` when their guards refuse
        // the result — most commonly the #1389 min-reduction
        // guard, which fires on a SIZE RELATIONSHIP between the
        // summary and the middle it replaces. That relationship
        // is a property of the THREAD SHAPE, not a transient
        // model failure: a thread whose weight sits in the
        // preserved head and the preserved 4-message tail has a
        // middle too small to yield the required reduction, and
        // re-trying cannot change that.
        //
        // Propagating that `Err` with `?` killed the whole
        // dispatch — every banked turn lost, `result: "error"`,
        // no envelope — which is the failure class #1221 already
        // taught this loop once. It became reachable far more
        // often once occupancy started being measured every turn
        // (above), so the two changes had to land together.
        //
        // The refusal itself is right: a summary that does not
        // shrink the thread should not be installed. What was
        // wrong was the consequence. The attempt is now recorded
        // and skipped, leaving `messages` exactly as the
        // compactor found them — the pre-#2792 behavior for this
        // thread shape — and the loop continues.
        match summary_chars {
            Ok(chars) => {
                self.state.compactions = attempted_generation;
                Some(chars)
            }
            Err(e) => {
                eprintln!(
                    "darkmux-runtime: {label} #{attempted_generation} was \
                     refused and SKIPPED, not installed — the conversation is \
                     unchanged and the dispatch continues: {e}"
                );
                self.trajectory.append_compaction_skipped(
                    self.state.turns,
                    attempted_generation,
                    self.messages.len(),
                    &e.to_string(),
                );
                // Leave the thread untouched and carry on. NOT a
                // `continue`: the per-turn liveness bookkeeping
                // below (proof-of-work stamp, inactivity window)
                // must still run, because a refused compaction
                // still spent a real compactor call and skipping
                // those updates would let the inactivity deadline
                // fire on a dispatch that was working.
                None
            }
        }
    }

    fn structured_compaction(
        &mut self,
        attempted_generation: u32,
        max_tokens_per_call: u32,
        compactor_calls: &mut Vec<compaction::CompactorCall>,
    ) -> Result<(usize, bool)> {
        // (#439) Build budget snapshot so the
        // compacted SYSTEM message can surface
        // remaining budget to the model. Lets
        // the model pace within bounds + use the
        // BLOCKED: escalation convention before
        // cap exhaustion.
        let budget = compaction::BudgetSnapshot {
            turns_used: self.state.turns,
            // (#457) Pass-through of the operator-
            // set caps (None = unlimited; renderer
            // skips the corresponding budget line).
            max_turns: self.limits.max_turns,
            cumulative_completion_tokens_used: self.state.total_completion_tokens,
            max_cumulative_completion_tokens: self.limits.max_cumulative_tokens,
            max_tokens_per_call,
        };
        compaction::structured_compact(
            self.compactor_client,
            &mut self.messages,
            attempted_generation,
            self.cfg,
            Some(budget),
            compactor_calls,
        )
        .map(|(parsed, summary_chars)| {
            // Persist the JSON for downstream
            // consumers (replay, methodology
            // research, cross-phase memory). Best-
            // effort: a write failure logs but does
            // NOT fail the dispatch — observability,
            // not correctness.
            persist_structured_compaction_output(
                &crate::trajectory::runtime_dir(),
                attempted_generation,
                &parsed,
            );
            (summary_chars, parsed.compaction_metadata.lexically_repaired == Some(true))
        })
    }

    /// Everything that describes a compaction that ACTUALLY INSTALLED. `Some`
    /// ends the dispatch.
    fn after_installed_compaction(
        &mut self,
        (summary_chars, lexically_repaired): (usize, bool),
        before_count: usize,
        effective_prompt_tokens: u32,
    ) -> Option<LoopOutcome> {
        let after_count = self.messages.len();
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
        let (sys_chars, prompt_chars) = measure_request_context(&self.messages);
        let tokens_after = ((sys_chars + prompt_chars) / 4) as u32;
        self.trajectory.append_compaction(
            self.state.compactions,
            before_count,
            after_count,
            crate::trajectory::InstalledSummary { summary_chars, lexically_repaired },
            tokens_before,
            tokens_after,
        );
        // (#2793) Did this compaction get BELOW the line that
        // summoned it? If not, `needs_compaction` is already
        // true again for the next turn on a thread this
        // compactor cannot reduce further — every remaining
        // turn will pay a compactor dispatch and none will buy
        // a turn without one. Each individual compaction looks
        // successful, which is exactly why this is invisible
        // from the compaction records alone.
        let trigger = self.cfg.effective_trigger_tokens();
        let consecutive_unproductive_compactions =
            self.unproductive_compactions.record(tokens_after, trigger);
        // (#2805) No "fire once" latch any more: this block
        // RETURNS, so the condition cannot recur within a
        // dispatch. #2793 needed the latch because it reported
        // and carried on, and a per-turn line would have buried
        // the run it was describing.
        if unproductive_compactions_escalate(consecutive_unproductive_compactions) {
            return Some(self.escalate_unproductive_compaction(consecutive_unproductive_compactions, tokens_after, trigger));
        }
        // (#3013) A compaction that succeeded is still a loop
        // when the turn after each one re-reads the turn
        // before it. The counter is fed at the top of every
        // tool turn; this is the only place it is read.
        self.compaction_repeat.compacted();
        if reread_loop_escalates(self.compaction_repeat.consecutive()) {
            announce_reread_loop(
                self.trajectory,
                self.state.turns,
                self.compaction_repeat.consecutive(),
                self.model,
                self.state.latest_prompt_tokens,
            );
            return Some(self.finish(None, TerminalReason::EscalationTriggered(EscalationReason::CompactionRereadLoop)));
        }
        // (#854) The thread just shrank, so the next report should
        // move again — restart staleness tracking so a fresh freeze
        // is detected cleanly and this episode isn't re-flagged.
        self.state.frozen_prompt_turns = 0;
        self.state.prev_prompt_tokens = None;
        // (#457 Step 3) Post-compaction feedback nudge.
        // The model's working state was just compressed
        // (compactions of 26+ messages → ~1500-char
        // summary); orient it toward the smallest concrete
        // next step rather than re-reading everything
        // (Beat 45's retrace pattern). Fires once per
        // compaction event; drains at the top of the next
        // loop iteration alongside any cycle/cascade
        // signals from this turn.
        self.feedback_injector.queue_post_compaction(self.state.turns);
        None
    }

    fn escalate_unproductive_compaction(&mut self, consecutive_unproductive_compactions: u32, tokens_after: u32, trigger: u32) -> LoopOutcome {
        eprintln!(
            "darkmux-runtime: {consecutive_unproductive_compactions} \
             compactions in a row have left the thread at \
             ~{tokens_after} tokens, still at or above the \
             {trigger}-token compaction trigger — the compactor \
             cannot reduce this thread below its own trigger, so \
             every remaining turn would pay a compactor dispatch \
             and none would buy a turn without one. ESCALATING to \
             the frontier rather than burning the difference. Raise \
             the profile's n_ctx or reduce per-turn tool output to \
             let this workload run locally. (#2805)"
        );
        self.trajectory.append_compaction_unproductive(
            self.state.turns,
            consecutive_unproductive_compactions,
            tokens_after,
            trigger,
        );
        // (#2805) ESCALATE, do not just report.
        //
        // #2793 added this detection and stopped at saying
        // it. Measured twice on the same workload with only
        // the report in place: 50 turns / 1.05M prompt
        // tokens, then 124 turns / 2.25M, neither
        // converging, both stopped by hand. Naming a
        // runaway is not ending one — and the signal went
        // to stderr and the trajectory, so an operator who
        // started a long dispatch and walked away (the
        // whole point of the local tier) came back to an
        // unbounded burn and a diagnostic they never saw.
        //
        // This is a GRACEFUL terminal, not a kill: the same
        // `EscalationTriggered` shape `bail_after_compactions`
        // uses, carrying every banked turn and the partial
        // answer out through `LoopOutcome` for the
        // `darkmux-escalation-handler` hand-off. The
        // difference from that bound is that it does not
        // require the operator to have predicted this
        // failure and set a number in advance.
        self.escalate(EscalationReason::CompactionUnproductive, None)
    }

    // ─── the length arm ──────────────────────────────────────────────────

    fn handle_length(&mut self, call: &Call) -> Result<Option<LoopOutcome>> {
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
        let message = &call.assistant_message;
        let is_useless_stall = has_no_text(message) && !has_tool_calls(message);
        // (#1221) The cap-cliff: length-finish WITH partial content
        // (or malformed tool calls) at exactly the per-call cap.
        // Pre-fix this was a hard error that killed the WHOLE
        // dispatch (#1222 shakedown): a prosecutor
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
        // `result: "error"` with no deliverable. Every banked checkpoint goes with it.
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
        // (#2836) `is_ours_or_unknown` is this reading, named. The
        // `unwrap_or(true)` it replaces is load-bearing and stays:
        // "cannot tell" must not route into the hard `Err` below,
        // which kills the dispatch and every banked checkpoint with
        // it, and an overflow diagnosis needs a measured count below
        // the cap to diagnose FROM.
        let cap_cliff = call.cut.is_ours_or_unknown();
        let content_slice = message.content.as_deref().unwrap_or("");
        // Emptiness is a property of THIS call, not of the turn. Testing
        // the accumulation meant that once a turn had checkpointed once
        // it could never be empty again, so the classic null-emission
        // runaway could never reach the drop-and-nudge branch a second
        // time. Measured: 41,383 checkpoints in 20s with
        // `Recovery budget 0/2` on every line, escalation unreachable.
        let this_call_produced_nothing =
            call.per_turn_reasoning.trim().is_empty() && content_slice.trim().is_empty();
        match length_effect(&self.state, &self.limits, is_useless_stall, cap_cliff, this_call_produced_nothing) {
            LengthEffect::Overflow => return Err(context_overflow(call)),
            // Budget check FIRST so an exhausted-budget escalation
            // doesn't have to also account for the unproductive
            // turn that just landed.
            LengthEffect::Escalate => return Ok(Some(self.escalate_stall())),
            LengthEffect::RecoverStall | LengthEffect::Checkpoint => {}
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
        // Route the slice into the region it belongs to. Nothing is
        // inserted between slices: the model was cut mid-sentence and
        // resumes at exactly that character, so any separator would
        // land inside a word. See `TurnAccum::absorb` for why the
        // ordering of the three shapes matters.
        self.turn.absorb(&call.per_turn_reasoning, content_slice);
        if this_call_produced_nothing {
            self.recover_stall(call);
        } else if let Some(outcome) = self.checkpoint(call) {
            return Ok(Some(outcome));
        }
        let tokens_str = completion_tokens_label(call.completion_tokens, call.cut_estimate);
        let shape = if is_useless_stall {
            "reasoning-only up to the cap"
        } else {
            "partial content truncated at the cap"
        };
        // The per-branch detail (extended / concluded / no-reasoning)
        // is printed above where the decision is made; this line is the
        // one-per-hit summary the operator scans for.
        let (turns, stall_recoveries_used, stall_recovery_budget) =
            (self.state.turns, self.state.stall_recoveries_used, self.limits.stall_recovery_budget);
        eprintln!(
            "darkmux-runtime: ⏸ per-call budget reached — turn {turns} \
             emitted {tokens_str} ({shape}); the turn's \
             reasoning was NOT discarded. Recovery budget \
             {stall_recoveries_used}/{stall_recovery_budget}. (#1221)"
        );
        Ok(None)
    }

    fn escalate_stall(&mut self) -> LoopOutcome {
        let (stall_recoveries_used, stall_recovery_budget) =
            (self.state.stall_recoveries_used, self.limits.stall_recovery_budget);
        // (#2229) Same wording change as the empty-tool-calls
        // arm above: the budget is a decaying RATE, so the line
        // says so rather than reading as a lifetime total.
        eprintln!(
            "darkmux-runtime: escalation_triggered — intra-turn \
             stall recovery budget ({stall_recoveries_used}/\
             {stall_recovery_budget}, decays by one per productive turn) \
             is exhausted; the nudge isn't breaking the pattern. \
             Emitting EscalationTriggered for frontier handoff. (#2229)"
        );
        let final_answer = self.turn.pending_answer();
        self.escalate(EscalationReason::IntraTurnStallExhausted, final_answer)
    }

    fn recover_stall(&mut self, call: &Call) {
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
        let bound = call.bound.cut_bound(call.cut);
        recover_intra_turn_stall(
            &mut self.messages,
            self.trajectory,
            self.state.turns,
            call.completion_tokens,
            &mut self.state.stall_recoveries_used,
            self.limits.stall_recovery_budget,
            STALL_NUDGE_MESSAGE,
            bound,
        );
        // Only a turn with nothing banked is a fresh start; one
        // mid-accumulation keeps going, or the empty call would
        // cost every checkpoint before it.
        self.state.resuming_after_checkpoint = self.turn.has_prefill();
        let (turns, stall_recoveries_used, stall_recovery_budget) =
            (self.state.turns, self.state.stall_recoveries_used, self.limits.stall_recovery_budget);
        eprintln!(
            "darkmux-runtime: ⏸ intra-turn stall recovered — turn {turns} hit                          the boundary with an EMPTY completion, so there is nothing to                          resume. Dropped the useless turn, injected a nudge; budget                          {stall_recoveries_used}/{stall_recovery_budget} used, hit {}. (#1123/#1221)",
            bound.describe()
        );
    }

    /// (#1221) CHECKPOINT, not a cap. `Some` ends the dispatch.
    fn checkpoint(&mut self, call: &Call) -> Option<LoopOutcome> {
        // What gets handed back is the WHOLE accumulation, never one
        // slice. That is also the scope the degeneracy gate needs: a
        // model re-treading ground from three checkpoints ago produces
        // slices that each look locally novel, so judging one slice in
        // isolation cannot see the cycle it exists to catch.
        let writing_thought = self.turn.writing_thought();
        let carried = self.turn.carried().to_string();
        self.state.checkpoints_used = self.state.checkpoints_used.saturating_add(1);
        // (#2171) The generation check-in's continuation budget —
        // deliberately NOT the same open-ended shape the reasoning
        // check-in gets (see the comment on `EscalationReason::
        // GenerationCheckpointBudgetExhausted`). Only a call that
        // was ITSELF generation-bound (`sent_generation_bound`)
        // draws from it; a reasoning-bound continuation of the
        // same turn (a thinking model that later starts writing
        // its answer) never does.
        //
        // (#2633) The budget is DRAWN here — the counter moves on
        // the same call it always did — but it is ACTED ON below,
        // AFTER the degeneracy gate has judged this call's slice.
        // Acting on it here, ahead of the gate, made the gate
        // UNREACHABLE on this arm at the shipped defaults, because
        // the two independently-tuned numbers collide at the same
        // checkpoint by construction:
        //
        //   - The gate's metric is `distinct windows / total
        //     windows` over a tail of `TAIL_SAMPLE_INTERVALS`
        //     intervals. A verbatim loop whose every call exactly
        //     fills the interval accumulates `k` identical copies
        //     by checkpoint `k`, so the ratio is ~`1/k` — it first
        //     dips under the 0.25 threshold at `k = 5`, and that
        //     crossing is a fixed `k` regardless of the interval's
        //     SIZE (both numerator and denominator scale with it).
        //   - `max_generation_continuations` floors at 4 whenever
        //     `answer_max_tokens / generation_interval <= 4`. The
        //     shipped defaults are 10000/4000 = 2.5, so the budget
        //     escalates on continuation 5 — the same call.
        //
        // Measured on the merged code at the literal shipped
        // defaults, against a 4000-token verbatim block: tail
        // ratios 1.0000 / 0.5007 / 0.3336 / 0.2502 across
        // checkpoints 1-4 (all `continue`), then the 5th call
        // returned EscalationTriggered(GenerationCheckpointBudget
        // Exhausted) with only four `dispatch.checkpoint` records
        // written — the call whose slice first reads 0.2001,
        // DEGENERATE, never got judged, and never got a record.
        //
        // Deferring the ACTION (rather than raising the floor)
        // is what removes the race instead of re-tuning it: the
        // gate now runs on EVERY checkpoint including the one that
        // exhausts the budget, so its reachability no longer
        // depends on any relationship between these two constants
        // at any operator config. It also costs nothing — the
        // budget's allowance is unchanged, so a turn that is NOT
        // repeating still stops on exactly the same call it
        // stopped on before.
        let (drawn, generation_budget_exhausted) = draw_generation_budget(&self.state, &call.bound, &self.limits);
        self.state.generation_continuations_this_turn = drawn;
        // (#3074) The continuation bound that does not depend on the
        // kind of check-in: once this logical turn has generated as
        // many tokens as the context window holds, no further
        // continuation can fit the prefill it would resend.
        let window_filled = turn_fills_window(self.cfg.context_window, self.state.turn_completion_tokens);
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
        //
        // (#2258) The tail window has to be sized by whichever
        // bound actually GOVERNED this call, not by
        // `reasoning_interval` unconditionally. #2171 widened this
        // arm to also handle generation-bound checkpoints (default
        // interval 4000, vs. the reasoning check-in's 1000) but
        // the sizing here kept reading the reasoning constant, so
        // a generation-bound call got an 8000-token tail
        // (`TAIL_SAMPLE_INTERVALS * 1000`) instead of the 32000
        // its own interval implies — a 4x-narrower sample than
        // the detector was tuned for on that pathway. `per_call_cap`
        // IS the governing interval: the cap selection (`call_bound`)
        // already resolved it to whichever
        // bound this call carries (`reasoning_interval` /
        // `generation_interval` / `answer_max_tokens`), so no
        // lookup is needed to name it here — `active_bound` exists
        // to attach PROVENANCE (which `BoundKind` this was, for the
        // trajectory record below), not to compute the value; all
        // three of its branches return `per_call_cap` verbatim.
        // (#2836 stage 1) Sized by whichever bound actually ended
        // this call. A ceiling cut hands over a 10,000-token slice;
        // sampling it with a tail sized for the 1,000-token check-in
        // judges a fraction of what is there. Same reasoning as
        // #2258's original fix, applied now that the two numbers can
        // differ.
        let governing_interval = governing_interval(call.cut, &call.bound);
        let degenerate = self.judge_slice(call, &carried, governing_interval);
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
        // (#2633) The generation check-in's continuation budget,
        // acted on HERE rather than ahead of the gate — see the
        // long note at the draw site above for the collision this
        // ordering removes. The counter already moved; this is
        // only the decision to stop on it.
        //
        // Deliberately AFTER the degeneracy escalation above and
        // BEFORE the remedy branches below. Both orderings are
        // load-bearing:
        //
        //   - After the gate, because on a call where BOTH are
        //     true the repetition is the more specific account of
        //     what was observed. "The budget ran out" is an
        //     accounting fact that would be true of any turn this
        //     long; "the output is repeating verbatim" is the
        //     reason it never converged. darkmux reports what it
        //     saw, and it saw a loop.
        //   - Before the remedy branches, because closing a
        //     repeating thought so the model can answer from what
        //     it has is only a remedy if there is a continuation
        //     left to spend on it. There is not — so a degenerate
        //     THOUGHT on the exhausting call stops here, with the
        //     checkpoint record above already carrying the
        //     `conclude` verdict and the tail ratio that say why.
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
        let limit = ContinuationLimit::reached(generation_budget_exhausted, window_filled);
        match checkpoint_remedy(degenerate, writing_thought, limit, self.turn.think_closed) {
            Remedy::HandOff => return Some(self.hand_off_repeating_answer()),
            Remedy::StopAt(limit) => return Some(self.stop_at_continuation_limit(limit)),
            Remedy::CloseThought => {
                let checkpoints_used = self.state.checkpoints_used;
                eprintln!(
                    "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — the reasoning is \
                     repeating (degeneracy gate); closing the thought so the model \
                     answers from what it has. (#1221)"
                );
                // The close is written INTO the accumulation, so every
                // later checkpoint keeps handing back a thought that is
                // already closed followed by the answer so far, and
                // everything after it is the ANSWER region.
                self.turn.close_thought();
            }
            Remedy::ContinueClosed => {
                let checkpoints_used = self.state.checkpoints_used;
                eprintln!(
                    "darkmux-runtime: ⏵ checkpoint {checkpoints_used} — thought already \
                     closed; handing back the answer so far so the model finishes it. \
                     (#1221)"
                );
            }
            Remedy::ContinueOpen => {
                let checkpoints_used = self.state.checkpoints_used;
                eprintln!(
                    "darkmux-runtime: ⏵ checkpoint {checkpoints_used} — reasoning is not \
                     repeating; handing it back OPEN so the model continues. (#1221)"
                );
            }
        }
        self.record_discarded_calls(call.cut);
        // A reasoning turn resumes INSIDE its think block; a
        // plain-answer turn resumes as itself, with no delimiters
        // invented around it. Either way the truncated raw response
        // goes and the prefill REPLACES its predecessor, so the
        // thread carries ONE growing assistant message rather than
        // a chain of restarts.
        self.messages.pop();
        self.turn.hand_back(&mut self.messages);
        None
    }

    /// Run the degeneracy gate over the carried region and record the
    /// checkpoint. Returns whether the verdict is to conclude.
    fn judge_slice(&mut self, call: &Call, carried: &str, governing_interval: u32) -> bool {
        let tail_ratio = crate::reasoning_loop::tail_repetition_ratio(
            carried,
            crate::reasoning_loop::TAIL_WINDOW_TOKENS,
            crate::reasoning_loop::tail_sample_tokens(governing_interval),
        );
        // Degeneracy DETECTION applies to any output; only the
        // remedy differs. An earlier cut gated the detection itself
        // on `turn_is_reasoning`, which left repeating plain content
        // with no gate at all — it checkpointed forever, and the
        // pre-existing intra-turn stall escalation that used to
        // bound exactly that shape became unreachable.
        // (#2846) Measure, then decide separately whether the
        // measurement is allowed to change anything. Splitting
        // these two is the whole feature: `record` keeps every
        // other variable identical (check-in cadence, per-call
        // cap, and therefore the usable prompt budget) and
        // changes only whether the verdict is obeyed.
        let policy = crate::detection::degeneracy_policy();
        let would_conclude = policy
            .measures()
            .then(|| crate::reasoning_loop::slice_is_degenerate(carried, governing_interval));
        let degenerate = would_conclude.unwrap_or(false) && policy.acts();
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
        self.state.resuming_after_checkpoint = true;
        // (#2165, revised #2171) Pre-#2171 a checkpoint
        // continuation only ever fired on the reasoning
        // check-in interval — "checkpoint" and "reasoning
        // check-in" were the same event. #2171 routes
        // GENERATION-bound cuts through this SAME continuation
        // machinery (a non-reasoning turn's prose/tool-call
        // batch that hits `generation_checkpoint_interval_tokens`
        // checkpoints exactly like a reasoning turn does), so a
        // checkpoint can now name either bound — `active_bound`
        // reads back whichever one THIS iteration's request
        // actually carried, same as the salvage site above.
        let bound = call.bound.cut_bound(call.cut);
        self.trajectory.append_checkpoint(
            self.state.turns,
            self.state.checkpoints_used,
            crate::trajectory::CheckpointVerdict {
                slice_tokens: call.completion_tokens,
                tail_ratio,
                verdict: if degenerate { darkmux_trajectory::Verdict::Conclude } else { darkmux_trajectory::Verdict::Continue },
                judged_chars: carried.chars().count(),
                policy: policy.as_str(),
                would_conclude,
            },
            bound,
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
        //   the inactivity deadline does NOT bound it: streaming is the
        //       production default, and every streamed chunk resets
        //       the host's deadline (`on_stream_tick`), so a turn
        //       that keeps producing slices is never idle.
        //   the context window DOES (#3074): once the turn has
        //       generated as many tokens as the window holds, it
        //       escalates (`TurnContinuationsExhausted`), because the
        //       next continuation would resend a prefill that cannot
        //       fit. That is the bound when no `max_tokens` is set,
        //       and it needs a configured window to derive from.
        //
        // A degenerate turn that never opened a thought has no
        // delimiter to close, so it does not get a prefill at all —
        // it goes back to the recovery path that already owns this
        // shape.
        degenerate
    }

    fn hand_off_repeating_answer(&mut self) -> LoopOutcome {
        let checkpoints_used = self.state.checkpoints_used;
        eprintln!(
            "darkmux-runtime: ⏹ checkpoint {checkpoints_used} — the ANSWER \
             region is repeating and there is no thought left to close. \
             Escalating for handoff with everything banked so far ATTACHED. \
             (#1221)"
        );
        let final_answer = self.turn.pending_answer();
        self.escalate(EscalationReason::IntraTurnStallExhausted, final_answer)
    }

    fn stop_at_continuation_limit(&mut self, limit: ContinuationLimit) -> LoopOutcome {
        let facts = ContinuationFacts {
            turns: self.state.turns,
            turn_tokens: self.state.turn_completion_tokens,
            checkpoints: self.state.checkpoints_used,
            model: self.model,
            latest_prompt_tokens: self.state.latest_prompt_tokens,
            turn_delay_ms: self.limits.turn_delay_ms,
            generation_interval: self.limits.generation_interval,
            generation_continuations: self.state.generation_continuations_this_turn,
            max_generation_continuations: self.limits.max_generation_continuations,
            answer_max_tokens: self.limits.answer_max_tokens,
        };
        let final_answer = self.turn.pending_answer();
        continuation_limit_outcome(
            self.trajectory,
            limit,
            facts,
            final_answer,
            std::mem::take(&mut self.messages),
            self.failed_to_run.clone(),
        )
    }

    fn record_discarded_calls(&mut self, cut: CutSource) {
        // (#2836) Before the pop: say what it destroys.
        //
        // Reaching here with tool calls attached means every one
        // of them failed #479's JSON check — a well-formed call
        // would have taken the salvage branch and dispatched.
        // They were cut mid-`arguments`, they cannot be sent
        // back (malformed arguments 400 the next request), and
        // the pop below is where they cease to exist. The model
        // meanwhile keeps its own reasoning announcing the work,
        // reads a thread where the call never happened, and
        // concludes it already answered — measured: a turn whose
        // reasoning said "let me rewrite the entire test file"
        // spent its next 54 tokens stopping.
        //
        // This is the ONLY place a tool call is discarded
        // silently. The two sibling pops in the stall-recovery
        // arms both guard on the message having no tool calls
        // before dropping it, so neither can lose one.
        //
        // Stage 0 records the loss; it does not prevent it.
        // Preventing it is Stage 1, which stops placing the cut
        // here at all.
        let Some(dropped) = self.messages.last() else {
            return;
        };
        for tc in dropped.tool_calls.iter().flatten() {
            eprintln!(
                "darkmux-runtime: ✖ discarded tool call `{}` — the \
                 check-in cut it after {} characters of arguments, \
                 which do not parse. The call is NOT dispatched and \
                 NOT sent back. (#2836)",
                tc.function.name,
                tc.function.arguments.chars().count()
            );
            self.trajectory.append_tool_call_discarded(
                self.state.turns,
                &sanitize_sample_name_prefix(&tc.function.name),
                tc.function.arguments.chars().count(),
                cut.wire_label(),
            );
        }
    }
}

/// (#406, #1050, #2164) Strip `reasoning_content` from the response message
/// (promoting it into content on a terminal turn with none), returning what
/// the field held BEFORE the strip.
fn strip_reasoning(response: &mut crate::lmstudio::ChatResponse) -> Option<String> {
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
    let choice = response.choices.first_mut()?;
    let finish = choice.finish_reason.clone();
    promote_terminal_reasoning(&mut choice.message, &finish)
}

/// (#2114 finding 2) The calls of a batch still to run after the one at
/// `idx`, and the `tool_seq` the first of them gets. `None` and `0` once the
/// last one has landed, matching a clean boundary.
fn pending_after(batch: &[ToolCall], idx: usize, next_seq: u32) -> (Option<Vec<ToolCall>>, u32) {
    let remaining: Vec<ToolCall> = batch[idx + 1..].to_vec();
    if remaining.is_empty() {
        (None, 0)
    } else {
        (Some(remaining), next_seq)
    }
}

/// The hard error for a `length` finish below our own cap.
fn context_overflow(call: &Call) -> anyhow::Error {
    let wire_max_tokens = call.bound.wire_max_tokens;
    anyhow!(
        "model returned finish_reason=length with partial content \
         BELOW the per-call cap (completion_tokens {} < \
         max_tokens_per_call {wire_max_tokens}) — context overflow: \
         prompt_tokens crossed the model's loaded context window. \
         Compaction may need a smaller threshold or a larger n_ctx.",
        call.completion_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    )
}
