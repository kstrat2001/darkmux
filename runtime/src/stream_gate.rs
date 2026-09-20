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

#[cfg(test)]
mod tests {
    use super::*;

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
