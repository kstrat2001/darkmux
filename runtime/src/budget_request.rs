//! (#1221) What happens when a turn is stopped at the per-call reasoning cap.
//!
//! # Why the old answer was wrong
//!
//! The cap was calibrated at 2x the max-useful-turn of NON-reasoning local
//! coders. Thinking models moved that ceiling far past the calibration, so the
//! cap began truncating PRODUCTIVE reasoning — measured at 43-50% of turns on
//! the review corpus, with one scraped turn tracing real code and naming a real
//! bug when it was cut. The recovery layers then DISCARDED the truncated turn
//! on the theory that a capped turn is noise. True of a genuine loop; false of
//! the measured case, and `content_empty` cannot tell them apart, because a
//! model that emits only reasoning looks identical to one that emitted nothing.
//!
//! # What replaces it
//!
//! The model's own tokens are handed back inside the think region, so it
//! RESUMES instead of restarting. Measured on the real thing: a 40,608-char
//! truncated turn prefilled back resumed mid-sentence and produced 3,999 more
//! tokens still tracking the same subject.
//!
//! **The closing delimiter is the switch**, and it is the whole design:
//!
//! * omit it -> the model keeps thinking, on a larger budget
//! * append it -> the model concludes FROM that reasoning rather than
//!   re-deriving it (the s1 budget-forcing move, reachable here because
//!   prefill bridges intra-generation forcing to an inter-call loop)
//!
//! # Why not `content`
//!
//! Promoting reasoning into `content` — the obvious alternative — puts scratch
//! work where the model was trained to read its own COMMITTED answer. It would
//! see hedging and backtracking presented as a conclusion. Prefill keeps the
//! tokens in the region they were generated in.
//!
//! # Who decides
//!
//! The RUNTIME, not the model. An earlier cut asked the model to conclude or
//! request more budget; measured on a real review it FOLDED at the first
//! checkpoint, producing a summary with zero findings where the same model
//! uninterrupted had produced a real finding. A model invited to stop will
//! stop. The gate is therefore silent — see `reasoning_loop::slice_is_degenerate`.

/// Qwen/DeepSeek-family thinking delimiters. Family-specific by nature: the
/// provider returns reasoning in a SEPARATE field with the delimiters stripped
/// (verified — a trivial request returns the substance in `reasoning_content`
/// and `"\n\n"` in `content`), so the raw generated sequence is never received
/// and the wrapper has to be rebuilt. Rebuilding it means inferring a
/// convention, which is why the caller must degrade honestly rather than guess
/// for a family it does not know: a wrong delimiter injects a stray tag into
/// the model's own output, which is worse than not trying.
pub const THINK_OPEN: &str = "<think>\n";
pub const THINK_CLOSE: &str = "\n</think>\n";

/// The prefill body for a turn that should KEEP THINKING on a larger budget.
pub fn continue_thinking_prefill(reasoning: &str) -> String {
    format!("{THINK_OPEN}{reasoning}")
}

/// The prefill body for a turn that should CONCLUDE from what it has.
pub fn conclude_now_prefill(reasoning: &str) -> String {
    format!("{THINK_OPEN}{reasoning}{THINK_CLOSE}")
}

#[cfg(test)]
#[path = "budget_request_tests.rs"]
mod tests;
