//! (#2836) Who ended this model call.
//!
//! **The defect this is the seam for.** The reasoning check-in
//! (`REASONING_CHECKPOINT_INTERVAL`, whose own doc calls it "a CHECKPOINT
//! INTERVAL, not a ceiling") is enforced SERVER-side, by riding out on the
//! wire as `max_tokens`. `tool_calls` and `content` are separate response
//! channels but ONE generation stream, so a cut lands wherever the model
//! happens to be — and when that is mid-`arguments`, the JSON is
//! unparseable, the call is discarded (correctly: sending malformed
//! arguments back 400s the next request), and the turn proceeds as though
//! nothing was lost. Measured single-variable: 18 cuts in 8 turns, zero
//! edits, sandbox untouched; the same run with the check-in lifted made 12
//! edits and left the suite green.
//!
//! **Stage 0's scope, and what it deliberately does NOT do.** The wire is
//! unchanged here — the check-in still rides out as `max_tokens`. What this
//! module adds is the vocabulary the rest of the fix needs: an explicit
//! answer to "did WE cut this turn, or did the model's context window?",
//! replacing two hand-rolled token comparisons that inferred it. Stage 1
//! takes the intervals off the wire and grows a real `StreamGate` here that
//! observes the stream and intervenes only on detection.
//!
//! **Why the two predicates below are not one.** `finish_reason: "length"`
//! is ambiguous on the wire: it is what the server says both when our own
//! `max_tokens` stopped the generation AND when the prompt crossed the
//! model's loaded context window. The runtime distinguishes them by
//! comparing the reported `completion_tokens` against the cap it sent — and
//! when `usage` is absent there is nothing to compare, so the answer is
//! genuinely unknown. The two call sites resolve that unknown in OPPOSITE
//! directions, on purpose:
//!
//! - The #479 salvage asks [`CutSource::is_ours_confirmed`]. Unknown reads
//!   as NOT ours, because salvaging means dispatching tool calls that may
//!   have been truncated; the conservative answer is to leave them alone.
//! - The #1221 cap-cliff asks [`CutSource::is_ours_or_unknown`]. Unknown
//!   reads as ours, because the alternative branch is a hard `Err` that
//!   kills the whole dispatch — every banked checkpoint with it — and a
//!   context-overflow diagnosis needs a measured count below the cap to
//!   diagnose FROM. There is nothing to diagnose from.
//!
//! Before this module those two readings lived as an `is_some_and` in one
//! place and an `unwrap_or(true)` in another, four hundred lines apart,
//! with no name on the distinction.

/// Why the runtime itself ended a stream, when it did.
///
/// Stage 0 constructs none of these — the runtime does not abort streams
/// yet. They are the vocabulary Stage 1's gate emits, declared here so the
/// predicates below are written once against the final shape rather than
/// widened later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AbortReason {
    /// The degeneracy gate fired at an observation boundary: the reasoning
    /// is repeating and continuing would burn budget on it.
    Degenerate,
    /// The stream went silent — no chunk for longer than the read timeout.
    /// Fires BEFORE the host's hard kill, so the dispatch still produces an
    /// envelope instead of dying as a transport error.
    Silent,
    /// The real per-call ceiling (`answer_max_tokens`), enforced
    /// client-side so it can land at a safe boundary.
    Ceiling,
}

/// Who ended this model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutSource {
    /// Nobody cut it — the model stopped on its own (`stop`, `tool_calls`).
    None,
    /// The server stopped generating and said `finish_reason: "length"`.
    ///
    /// `measured_at_cap` is the runtime's reading of WHOSE length that was:
    /// `Some(true)` the reported completion tokens reached the cap we sent,
    /// `Some(false)` they stopped short of it (context overflow),
    /// `None` no `usage` arrived and the question cannot be answered.
    ServerLength { measured_at_cap: Option<bool> },
    /// The runtime ended the stream deliberately. Stage 1 onward.
    #[allow(dead_code)]
    RuntimeAbort(AbortReason),
}

/// What one streamed call produced, and whether the RUNTIME ended it.
///
/// Stage 0 always reports [`CutSource::None`] here — the runtime does not
/// abort streams yet, so the only cutter is the server. The field exists
/// now so Stage 1's gate changes what it puts in this struct rather than
/// changing the signature of the stream driver and every caller of it.
pub struct StreamOutcome {
    pub response: crate::lmstudio::ChatResponse,
    pub cut: CutSource,
}

impl CutSource {
    /// Classify a completed call from what the wire reported.
    ///
    /// `cap` is what THIS request actually carried as `max_tokens` — the
    /// region value, not the raw ceiling. Comparing against anything else
    /// stops recognizing a check-in cut as ours, which drops well-formed
    /// tool calls that should have been dispatched (a real regression, once
    /// shipped and reverted).
    ///
    /// The comparison is tolerance-matched rather than equality-matched:
    /// LMStudio reports `cap - 1` live (9999 at cap 10000, 29999 at cap
    /// 30000 — it stops before the token that would exceed). An exact
    /// `== cap` never matches in production.
    pub fn classify(finish_reason: &str, completion_tokens: Option<u32>, cap: u32) -> Self {
        if finish_reason != "length" {
            return CutSource::None;
        }
        CutSource::ServerLength {
            measured_at_cap: completion_tokens.map(|t| t.saturating_add(1) >= cap),
        }
    }

    /// The stable string a trajectory record names this cut by.
    ///
    /// Read by anything reconstructing WHY work was lost, so it is a wire
    /// contract, not a log line: the values only ever gain members. Stage 1
    /// starts emitting the `runtime_abort:*` forms, and a consumer that
    /// already distinguishes `server_length` from them needs no change when
    /// it does.
    pub fn wire_label(&self) -> &'static str {
        match self {
            CutSource::None => "none",
            CutSource::ServerLength { .. } => "server_length",
            CutSource::RuntimeAbort(AbortReason::Degenerate) => "runtime_abort:degenerate",
            CutSource::RuntimeAbort(AbortReason::Silent) => "runtime_abort:silent",
            CutSource::RuntimeAbort(AbortReason::Ceiling) => "runtime_abort:ceiling",
        }
    }

    /// Did the runtime's own bound end this call, as far as the evidence
    /// actually shows? An unmeasurable call answers NO.
    ///
    /// This is the salvage predicate: a `true` here dispatches tool calls
    /// that were in flight when the cut landed, so an unproven `true` costs
    /// a truncated call reaching a real tool.
    pub fn is_ours_confirmed(&self) -> bool {
        match self {
            CutSource::None => false,
            CutSource::ServerLength { measured_at_cap } => measured_at_cap.unwrap_or(false),
            CutSource::RuntimeAbort(_) => true,
        }
    }

    /// Did the runtime's own bound end this call, or can we not tell? An
    /// unmeasurable call answers YES.
    ///
    /// This is the cap-cliff predicate: a `false` here routes to a hard
    /// `Err` that kills the dispatch, so an unproven `false` costs the
    /// whole run.
    pub fn is_ours_or_unknown(&self) -> bool {
        match self {
            CutSource::None => false,
            CutSource::ServerLength { measured_at_cap } => measured_at_cap.unwrap_or(true),
            CutSource::RuntimeAbort(_) => true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Stage 1 — the observer
// ─────────────────────────────────────────────────────────────────────────

/// How much generated text separates two observations, expressed in the
/// token interval the operator configured.
///
/// **Not chunks.** The staged plan proposed counting the cadence in SSE
/// chunks, on a measurement that chunks track tokens about 1:1 on Splash.
/// Measured across 61 real calls on 2026-09-20 that ratio ran **0.02 to
/// 1.00, median 0.61 — a 43x spread**, because Splash is a speculative
/// engine: one chunk carries however many drafted tokens the verify step
/// accepted, and acceptance is content dependent (0.45-0.51 measured, with
/// 76-78% of emitted tokens coming from the draft model). A chunk is not a
/// token on any speculative engine, and the 1:1 reading came from a single
/// run that did not generalize.
///
/// Characters are counted directly off the deltas, so they need no proxy at
/// all. `CHARS_PER_TOKEN` converts the operator's token-denominated interval
/// into one, and the real ratio for each completed call is stamped into the
/// trajectory so the constant is checkable rather than assumed. It matches
/// the ruler the rest of the runtime already uses for generated prose.
///
/// Being wrong here costs CADENCE, never correctness: the verdict function
/// judges the text it is handed, and a boundary landing early or late only
/// changes how often a clean stream is looked at for free.
pub const CHARS_PER_TOKEN: usize = 4;

/// What the driver should do with the chunk it just fed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Keep reading. No observation boundary, or one was reached and
    /// deliberately skipped. **This is the common case and it costs nothing
    /// the model can see** — no truncation, no round trip, no prefill.
    Continue,
    /// A boundary was reached and the slice judged clean. Keep reading;
    /// record the observation.
    Observed { slice_chars: usize },
    /// A boundary was reached and the slice is degenerate. Stop reading.
    Degenerate { slice_chars: usize },
}

/// Watches one streamed call and decides, without ever truncating a healthy
/// one, whether it has started repeating.
///
/// **The inversion this is.** The loop used to intervene unconditionally and
/// then decide: send `max_tokens` at the check-in interval, let the server
/// truncate whatever was in flight, inspect what came back, hand it back
/// open or closed. The cut was never the point of the feature; it was how a
/// decision point got extracted from a stateless request/response. Here the
/// interval stops riding the wire and becomes an OBSERVATION CADENCE. A
/// clean turn is never interrupted at all.
///
/// The runtime is already positioned for it: it streams, and it already sees
/// every delta including tool-call fragments. Degeneracy detection never
/// needed truncation, only visibility.
pub struct StreamGate {
    interval_chars: usize,
    /// Passed to the verdict function for tail SIZING, which is
    /// token-denominated (`tail_sample_tokens`). Kept separate from
    /// `interval_chars` so the conversion above cannot silently change how
    /// wide a tail the detector samples.
    interval_tokens: u32,
    judge: fn(&str, u32) -> bool,
    /// Everything this call has generated, reasoning and content alike. The
    /// pre-Stage-1 gate judged the same union (`carried`), so the verdict
    /// sees exactly what it used to.
    slice: String,
    /// Length of the seeded prefix, so `generated_chars` can subtract it.
    carried_chars: usize,
    since_boundary: usize,
    observations: u32,
    tool_call_seen: bool,
}

impl StreamGate {
    /// `carried` is the turn's accumulation SO FAR — everything earlier
    /// continuations of this same turn produced.
    ///
    /// **The verdict must see the whole turn, not this call.** The post-hoc
    /// gate this replaces judges `turn.carried()` and says why in its own
    /// comment: *"a model re-treading ground from three checkpoints ago
    /// produces slices that each look locally novel, so judging one slice in
    /// isolation cannot see the cycle it exists to catch."*
    ///
    /// A first cut of this gate judged only the current call and a live run
    /// showed the other half of the same coin: one turn aborted SIX times in
    /// a row, each after a single observation, while the post-hoc judge
    /// looking at the full accumulation returned `continue` every time. A
    /// short slice falls into a different regime of the detector (the token
    /// metric returns "too short to judge" and the char fallback takes over),
    /// so the two judges disagreed on the same text. Seeding the prefix makes
    /// them one judge looking at one thing.
    ///
    /// The cadence still counts only NEW characters — `since_boundary` starts
    /// at zero — so a long carried prefix does not immediately trip a
    /// boundary.
    pub fn new(interval_tokens: u32, judge: fn(&str, u32) -> bool, carried: &str) -> Self {
        Self {
            interval_chars: (interval_tokens as usize).saturating_mul(CHARS_PER_TOKEN),
            interval_tokens,
            judge,
            slice: carried.to_string(),
            carried_chars: carried.chars().count(),
            since_boundary: 0,
            observations: 0,
            tool_call_seen: false,
        }
    }

    /// Characters this CALL generated, excluding the carried prefix — the
    /// numerator for the cadence calibration figure.
    pub fn generated_chars(&self) -> usize {
        self.slice.chars().count().saturating_sub(self.carried_chars)
    }

    pub fn observations(&self) -> u32 {
        self.observations
    }

    #[cfg(test)]
    pub fn slice_chars(&self) -> usize {
        self.slice.chars().count()
    }

    /// Feed one chunk. Call this for every chunk, in order.
    pub fn ingest(&mut self, chunk: &crate::lmstudio::ChatChunk) -> GateAction {
        for choice in &chunk.choices {
            let d = &choice.delta;
            if d.tool_calls.as_ref().is_some_and(|t| !t.is_empty()) {
                self.tool_call_seen = true;
            }
            for text in [d.reasoning_content.as_deref(), d.content.as_deref()]
                .into_iter()
                .flatten()
            {
                self.slice.push_str(text);
                self.since_boundary += text.chars().count();
            }
        }

        if self.since_boundary < self.interval_chars {
            return GateAction::Continue;
        }
        // Carry the remainder rather than zeroing. A chunk can deliver far
        // more than one interval's worth of text (on a speculative engine a
        // single chunk carries every draft token the verify step accepted),
        // and discarding the overflow would let the cadence drift later and
        // later behind the configured interval. At most one observation per
        // chunk either way: judging the same accumulated slice twice in a
        // row would return the same verdict.
        self.since_boundary -= self.interval_chars;

        // **Never judge a call that has begun emitting a tool call.**
        //
        // Two independent reasons, either sufficient. The metric is wrong on
        // JSON: measured at the shipped threshold, an enum-valued JSON array
        // scores 0.003 and a block of identical match arms 0.003 — both
        // "degenerate", neither pathological. And a call that is emitting a
        // tool call is productive by definition; the entire point of #2836 is
        // that cutting one destroys work that cannot be recovered, because
        // the model reads a thread where the action never happened and
        // concludes it has already answered.
        //
        // Deliberately wider than "while the arguments are still open": once
        // a tool call has STARTED, judging is suspended for the rest of the
        // call. A model that emits a call and then degenerates in a long
        // answer is left to the ceiling rather than risked here. Suspending
        // is cheap; a false positive costs real work.
        if self.tool_call_seen {
            return GateAction::Continue;
        }

        self.observations += 1;
        let chars = self.slice.chars().count();
        if (self.judge)(&self.slice, self.interval_tokens) {
            GateAction::Degenerate { slice_chars: chars }
        } else {
            GateAction::Observed { slice_chars: chars }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Stage 1: the observer ───────────────────────────────────────

    use crate::lmstudio::{ChatChunk, ChoiceDelta, Delta, ToolCallDelta};

    fn chunk(reasoning: Option<&str>, content: Option<&str>, tool: bool) -> ChatChunk {
        ChatChunk {
            id: "c".into(),
            choices: vec![ChoiceDelta {
                index: 0,
                delta: Delta {
                    role: None,
                    content: content.map(str::to_string),
                    reasoning_content: reasoning.map(str::to_string),
                    tool_calls: tool.then(|| {
                        vec![ToolCallDelta {
                            index: Some(0),
                            id: Some("call_1".into()),
                            kind: Some("function".into()),
                            function: None,
                            extra_content: None,
                        }]
                    }),
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    const NEVER: fn(&str, u32) -> bool = |_, _| false;
    const ALWAYS: fn(&str, u32) -> bool = |_, _| true;

    /// The headline claim, and the one the whole redesign exists to make:
    /// **a turn that is behaving well is never interrupted.** Before Stage 1
    /// this stream would have been truncated four times by the server, each
    /// cut costing a round trip and a re-sent prefill, and any one of them
    /// could have landed mid-tool-call.
    #[test]
    fn a_clean_stream_is_never_interrupted_however_long_it_runs() {
        let mut g = StreamGate::new(10, NEVER, ""); // 40-char cadence
        let mut observed = 0;
        for _ in 0..40 {
            match g.ingest(&chunk(Some("some ordinary reasoning text "), None, false)) {
                GateAction::Continue => {}
                GateAction::Observed { .. } => observed += 1,
                GateAction::Degenerate { .. } => panic!("a clean stream must never be cut"),
            }
        }
        assert!(observed > 0, "the gate must actually be looking, not just passing");
    }

    /// Below the cadence the gate does nothing at all — not "looks and says
    /// fine", literally nothing. That is what makes observation free.
    #[test]
    fn nothing_happens_before_the_first_boundary() {
        let mut g = StreamGate::new(100, ALWAYS, ""); // 400-char cadence
        for _ in 0..3 {
            assert_eq!(g.ingest(&chunk(None, Some("short"), false)), GateAction::Continue);
        }
        assert_eq!(g.observations(), 0, "no boundary reached, so nothing was judged");
    }

    /// Reasoning delivered on the separate field is the bulk of what a
    /// thinking model emits, so a cadence that only counted `content` would
    /// never fire on exactly the models this feature is for.
    #[test]
    fn reasoning_channel_text_advances_the_cadence() {
        let mut g = StreamGate::new(5, NEVER, ""); // 20-char cadence
        let a = g.ingest(&chunk(Some("0123456789012345678901234"), None, false));
        assert!(matches!(a, GateAction::Observed { .. }), "got {a:?}");
        assert_eq!(g.observations(), 1);
    }

    #[test]
    fn a_degenerate_slice_at_a_boundary_stops_the_stream() {
        let mut g = StreamGate::new(5, ALWAYS, "");
        let a = g.ingest(&chunk(None, Some("0123456789012345678901234"), false));
        assert!(matches!(a, GateAction::Degenerate { .. }), "got {a:?}");
    }

    /// **The guard #2836 is about.** A call that has begun emitting a tool
    /// call is never judged, so it can never be cut by this gate — even with
    /// a verdict function that calls everything degenerate.
    ///
    /// Two reasons the real detector needs this: JSON scores 0.003 against a
    /// 0.25 threshold (the documented false-degenerate class), and a call
    /// emitting a tool call is productive by definition. Measured cost of
    /// getting it wrong: 9 destroyed `edit` calls across 4 runs, each one
    /// leaving the model reading a thread where the action never happened.
    #[test]
    fn a_call_that_started_a_tool_call_is_never_judged_again() {
        let mut g = StreamGate::new(5, ALWAYS, ""); // would cut at every boundary
        assert_eq!(g.ingest(&chunk(None, None, true)), GateAction::Continue);
        for _ in 0..20 {
            assert_eq!(
                g.ingest(&chunk(None, Some("0123456789012345678901234"), false)),
                GateAction::Continue,
                "the gate must stay silent for the rest of a call that is emitting a tool call"
            );
        }
        assert_eq!(
            g.observations(),
            0,
            "not merely 'judged and continued' — never judged at all"
        );
    }

    /// The suspension is not retroactive: boundaries BEFORE the tool call
    /// are judged normally. Otherwise a model that reasons degenerately for
    /// a long time and then emits one call would be exempt from the gate
    /// entirely.
    #[test]
    fn boundaries_before_the_tool_call_are_still_judged() {
        let mut g = StreamGate::new(5, ALWAYS, "");
        let a = g.ingest(&chunk(None, Some("0123456789012345678901234"), false));
        assert!(matches!(a, GateAction::Degenerate { .. }), "got {a:?}");
        assert_eq!(g.observations(), 1);
    }

    /// (#2836, found by a live run) The verdict judges the whole TURN, not
    /// this call. A gate seeded with the turn's accumulation hands the
    /// detector the same text the post-hoc gate sees.
    ///
    /// Without the seed, one real turn aborted six times in a row — each
    /// after a single observation — while the post-hoc judge on the full
    /// accumulation returned `continue` every time. Two judges, one turn,
    /// opposite answers, because a short slice falls into the detector's
    /// char-fallback regime that a long one never reaches.
    #[test]
    fn the_verdict_sees_the_carried_turn_not_just_this_call() {
        let carried = "earlier reasoning from a previous continuation ".repeat(10);
        let mut g = StreamGate::new(5, NEVER, &carried);
        assert_eq!(
            g.observations(),
            0,
            "a long carried prefix must not itself trip a boundary — the cadence \
             measures NEW text"
        );
        assert!(g.slice_chars() >= carried.chars().count());
        g.ingest(&chunk(None, Some("0123456789012345678901234"), false));
        assert_eq!(g.observations(), 1, "and the new text still advances it");
        assert!(
            g.slice_chars() > carried.chars().count(),
            "the judged slice is prefix PLUS this call"
        );
        assert_eq!(
            g.generated_chars(),
            25,
            "but the calibration numerator counts only what this call generated"
        );
    }

    /// A chunk can deliver several intervals' worth of text at once — on a
    /// speculative engine one chunk carries every accepted draft token. The
    /// overflow has to carry, or the cadence drifts further behind the
    /// configured interval with every oversized chunk.
    #[test]
    fn an_oversized_chunk_carries_its_remainder_into_the_next_boundary() {
        let mut g = StreamGate::new(5, NEVER, ""); // 20-char cadence
        // 38 chars: one boundary now, 18 left over.
        let a = g.ingest(&chunk(None, Some(&"x".repeat(38)), false));
        assert!(matches!(a, GateAction::Observed { .. }), "got {a:?}");
        // 2 more chars reaches 20 again only if the 18 carried.
        let b = g.ingest(&chunk(None, Some("yy"), false));
        assert!(
            matches!(b, GateAction::Observed { .. }),
            "the remainder must carry; zeroing it would need 20 fresh chars here. got {b:?}"
        );
        assert_eq!(g.observations(), 2);
    }

    /// The verdict sees the WHOLE call, not just the slice since the last
    /// boundary — same scope the pre-Stage-1 gate judged (`carried`), so a
    /// repetition spanning two boundaries is still visible.
    #[test]
    fn the_judged_slice_accumulates_across_boundaries() {
        let seen: std::cell::Cell<usize> = std::cell::Cell::new(0);
        // fn-pointer judges cannot capture, so assert via the gate's own view.
        let mut g = StreamGate::new(5, NEVER, "");
        g.ingest(&chunk(None, Some("aaaaaaaaaaaaaaaaaaaaa"), false));
        let after_first = g.slice_chars();
        g.ingest(&chunk(None, Some("bbbbbbbbbbbbbbbbbbbbb"), false));
        assert!(
            g.slice_chars() > after_first,
            "the slice must grow, not reset, between boundaries"
        );
        seen.set(g.slice_chars());
        assert_eq!(seen.get(), 42);
    }

    #[test]
    fn a_turn_the_model_ended_itself_is_nobody_s_cut() {
        for reason in ["stop", "tool_calls"] {
            let cut = CutSource::classify(reason, Some(4_000), 1_000);
            assert_eq!(
                cut,
                CutSource::None,
                "{reason} means the model stopped on its own — a token count \
                 at or past the cap is not evidence of a cut when the server \
                 did not say length"
            );
            assert!(!cut.is_ours_confirmed());
            assert!(!cut.is_ours_or_unknown());
        }
    }

    #[test]
    fn a_length_finish_at_the_cap_is_ours_on_both_readings() {
        let cut = CutSource::classify("length", Some(1_000), 1_000);
        assert!(cut.is_ours_confirmed(), "salvage must engage");
        assert!(cut.is_ours_or_unknown(), "the cliff must not hard-error");
    }

    #[test]
    fn the_cap_minus_one_that_lmstudio_actually_reports_still_reads_as_the_cap() {
        // Not a rounding nicety: an exact `== cap` never matches in
        // production, which silently killed both the salvage and the cliff
        // recovery on real dispatches.
        let cut = CutSource::classify("length", Some(999), 1_000);
        assert!(cut.is_ours_confirmed(), "999 at cap 1000 is a cap hit");
        assert!(cut.is_ours_or_unknown());
    }

    #[test]
    fn a_length_finish_well_below_the_cap_is_the_context_window_not_us() {
        let cut = CutSource::classify("length", Some(120), 10_000);
        assert!(
            !cut.is_ours_confirmed(),
            "nothing of ours stopped this — salvaging would dispatch calls \
             truncated by an overflow"
        );
        assert!(
            !cut.is_ours_or_unknown(),
            "this is the one shape that SHOULD reach the hard error: a \
             measured count below our own cap is the overflow diagnosis"
        );
    }

    /// The asymmetry is the whole reason these are two methods. An absent
    /// `usage` is not a value to default; it is a question the two callers
    /// answer differently because they are risking different things.
    #[test]
    fn an_unmeasurable_length_finish_splits_the_two_readings() {
        let cut = CutSource::classify("length", None, 1_000);
        assert!(
            !cut.is_ours_confirmed(),
            "without a token count there is no evidence our cap cut it, and \
             salvage dispatches real tool calls on the strength of that claim"
        );
        assert!(
            cut.is_ours_or_unknown(),
            "the alternative is a hard Err that kills the dispatch and every \
             banked checkpoint with it — 'cannot tell' must not read as \
             'context overflow'"
        );
    }

    /// The label is what a later reader keys on, so it must not vary with
    /// the measurement detail folded into the same variant.
    #[test]
    fn the_wire_label_names_the_source_not_the_measurement() {
        assert_eq!(CutSource::None.wire_label(), "none");
        for measured in [Some(true), Some(false), None] {
            assert_eq!(
                CutSource::ServerLength {
                    measured_at_cap: measured
                }
                .wire_label(),
                "server_length",
                "whether we could measure the cut is a separate question from \
                 who made it"
            );
        }
        assert_eq!(
            CutSource::RuntimeAbort(AbortReason::Degenerate).wire_label(),
            "runtime_abort:degenerate"
        );
        assert_eq!(
            CutSource::RuntimeAbort(AbortReason::Silent).wire_label(),
            "runtime_abort:silent"
        );
        assert_eq!(
            CutSource::RuntimeAbort(AbortReason::Ceiling).wire_label(),
            "runtime_abort:ceiling"
        );
    }

    #[test]
    fn a_runtime_abort_is_ours_on_both_readings_without_needing_usage() {
        // Stage 1's gate synthesizes a terminal state with no `usage` at
        // all, so a token comparison cannot answer for it. That is exactly
        // why the predicates key on the source rather than on the count.
        for reason in [
            AbortReason::Degenerate,
            AbortReason::Silent,
            AbortReason::Ceiling,
        ] {
            let cut = CutSource::RuntimeAbort(reason);
            assert!(cut.is_ours_confirmed(), "{reason:?}");
            assert!(cut.is_ours_or_unknown(), "{reason:?}");
        }
    }
}
