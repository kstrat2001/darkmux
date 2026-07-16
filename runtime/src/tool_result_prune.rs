//! Tool-result head/tail soft-trim (#1391).
//!
//! Oversized tool results sit in the transcript verbatim, and on small
//! context windows they are the single biggest driver of the FIRST
//! compaction. Middle-eliding an OLD, large tool result (keep the head +
//! tail, drop the middle behind a marker) is a cheap, lossy-but-bounded
//! context reclaim that DELAYS the first compaction — on a tight window
//! that can mean one fewer compaction per dispatch, which is measurable
//! wall-clock.
//!
//! ## Prior art
//!
//! Design convergence with xai-org/grok-build (Apache-2.0), `xai-chat-state`
//! `PruningConfig` (head 1500 / tail 1500 over a 4000-char threshold).
//! Independently implemented here; no code copied.
//!
//! ## What this is NOT
//!
//! - NOT compaction. Compaction summarizes the MIDDLE of the whole
//!   conversation via a companion model call (`crate::compaction`); this
//!   is a zero-model-call, purely mechanical byte trim of a single
//!   oversized tool-result body. It runs BEFORE the compaction trigger is
//!   evaluated so the reclaimed bytes push the trigger out.
//! - NOT a hard-clear. The grok-build prior art also hard-clears results
//!   older than K turns to a placeholder; that second stage is
//!   deliberately out of scope here (this ships the soft-trim only).
//!
//! ## Safety
//!
//! - The last [`TOOL_RESULT_TRIM_PRESERVE_RECENT`] messages are NEVER
//!   trimmed — the active tool-call/tool-result thread the model is
//!   reasoning about right now stays intact.
//! - The trim is idempotent: an already-trimmed body carries the marker
//!   sentinel and is skipped, so re-running the pass every turn is a
//!   no-op on bodies already reclaimed.
//! - Head/tail cuts land on UTF-8 char boundaries (the #329 class of
//!   slice-mid-codepoint panics is guarded by clamping to the nearest
//!   boundary), so multibyte tool output (CJK / emoji / accents) is safe.

use crate::lmstudio::Message;

/// Byte length above which an OLD tool-result body is soft-trimmed.
/// Conservative: only genuinely large results (4000+ bytes) are touched,
/// so ordinary short results are never disturbed. Chosen to sit well
/// above head + tail so a fired trim always leaves a real middle to
/// elide. (#1391)
pub const TOOL_RESULT_TRIM_THRESHOLD_BYTES: usize = 4000;

/// Bytes of the head kept verbatim. The head of a tool result usually
/// carries the most load-bearing context (the command echo, the first
/// rows of output, the error header). (#1391)
pub const TOOL_RESULT_TRIM_HEAD_BYTES: usize = 1500;

/// Bytes of the tail kept verbatim. The tail usually carries the
/// terminal signal (exit status, final assertion, summary line). (#1391)
pub const TOOL_RESULT_TRIM_TAIL_BYTES: usize = 1500;

/// Number of most-recent messages that are NEVER trimmed. The model is
/// actively reasoning over the tail of the transcript; trimming a result
/// it just received would remove context it still needs. Only OLDER
/// results (before this window) are reclaimed. (#1391)
pub const TOOL_RESULT_TRIM_PRESERVE_RECENT: usize = 6;

/// Sentinel that marks an already-trimmed body. Presence of this
/// substring makes the pass idempotent (a trimmed body is skipped on
/// subsequent turns). It is the self-identifying prefix of the
/// model-facing elision marker, so the marker doubles as the guard. (#1391)
pub const TOOL_RESULT_TRIM_MARKER_SENTINEL: &str = "[darkmux-runtime: elided";

/// Outcome of one soft-trim pass over the transcript.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ToolTrimStats {
    /// How many tool-result bodies were trimmed this pass.
    pub results_trimmed: usize,
    /// Net bytes removed from the transcript (sum of `old_len - new_len`).
    pub bytes_reclaimed: usize,
}

/// Largest char boundary `<= i` (clamps a byte index DOWN so a slice
/// never splits a multibyte codepoint). (#329 class; #1391)
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i` (clamps a byte index UP so a slice
/// never splits a multibyte codepoint). (#329 class; #1391)
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// The model-facing elision marker inserted between the kept head and
/// tail. Per the repo's model-facing conventions (CLAUDE.md): directive
/// + literal, with a self-identifying `[darkmux-runtime: ...]` prefix as
/// provenance. It states exactly how many bytes were removed and how to
/// recover them, so a fresh-context model reads it as an instruction, not
/// as prose to imitate. (#1391)
fn elision_marker(elided_bytes: usize, head_bytes: usize, tail_bytes: usize) -> String {
    format!(
        "\n[darkmux-runtime: elided {elided_bytes} bytes from the middle of this tool result; \
         kept the first {head_bytes} and last {tail_bytes} bytes. Re-run the tool to retrieve \
         the omitted section if you need it.]\n"
    )
}

/// Soft-trim a single tool-result body if it is over threshold and not
/// already trimmed. Returns `Some(new_body)` when a trim was applied,
/// `None` when the body is left untouched (under threshold, or already
/// carries the marker sentinel). Pure + total: safe on any `&str`. (#1391)
pub fn soft_trim_body(body: &str) -> Option<String> {
    if body.len() <= TOOL_RESULT_TRIM_THRESHOLD_BYTES {
        return None;
    }
    // Idempotent: never re-trim a body we already reclaimed.
    if body.contains(TOOL_RESULT_TRIM_MARKER_SENTINEL) {
        return None;
    }
    let head_end = floor_char_boundary(body, TOOL_RESULT_TRIM_HEAD_BYTES);
    let tail_start =
        ceil_char_boundary(body, body.len().saturating_sub(TOOL_RESULT_TRIM_TAIL_BYTES));
    // Defensive: with THRESHOLD (4000) > HEAD (1500) + TAIL (1500) the head
    // and tail never overlap, so there is always a real middle to elide.
    // The guard keeps the function total if the constants are ever retuned
    // so head + tail could meet.
    if head_end >= tail_start {
        return None;
    }
    let elided = tail_start - head_end;
    let head = &body[..head_end];
    let tail = &body[tail_start..];
    let marker = elision_marker(elided, head_end, body.len() - tail_start);
    Some(format!("{head}{marker}{tail}"))
}

/// Walk the transcript and soft-trim every OLD oversized tool-result body.
/// The last [`TOOL_RESULT_TRIM_PRESERVE_RECENT`] messages are skipped so
/// the active thread stays intact. Mutates in place; returns what it
/// reclaimed for the caller's observability. (#1391)
pub fn soft_trim_old_tool_results(messages: &mut [Message]) -> ToolTrimStats {
    let n = messages.len();
    let protect_from = n.saturating_sub(TOOL_RESULT_TRIM_PRESERVE_RECENT);
    let mut stats = ToolTrimStats::default();
    for (i, m) in messages.iter_mut().enumerate() {
        if i >= protect_from {
            // Everything from here on is inside the protected recent window.
            break;
        }
        if m.role != "tool" {
            continue;
        }
        let Some(body) = m.content.as_deref() else {
            continue;
        };
        if let Some(trimmed) = soft_trim_body(body) {
            let reclaimed = body.len().saturating_sub(trimmed.len());
            stats.results_trimmed += 1;
            stats.bytes_reclaimed += reclaimed;
            m.content = Some(trimmed);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(body: &str) -> Message {
        Message::tool_result("call_1", "bash", body)
    }

    #[test]
    fn under_threshold_body_is_untouched() {
        let body = "x".repeat(TOOL_RESULT_TRIM_THRESHOLD_BYTES);
        assert!(
            soft_trim_body(&body).is_none(),
            "a body exactly at threshold must not trim (boundary is `>`)"
        );
        let small = "short output";
        assert!(soft_trim_body(small).is_none());
    }

    #[test]
    fn over_threshold_keeps_exact_head_and_tail() {
        // Distinct head/tail sentinels so we can assert the exact bytes kept.
        let head = "HEAD_".to_string() + &"a".repeat(TOOL_RESULT_TRIM_HEAD_BYTES);
        let tail = "b".repeat(TOOL_RESULT_TRIM_TAIL_BYTES) + "_TAIL";
        let middle = "m".repeat(5000);
        let body = format!("{head}{middle}{tail}");
        let trimmed = soft_trim_body(&body).expect("over-threshold body trims");

        // Head bytes kept verbatim (exactly HEAD_BYTES of them).
        assert!(trimmed.starts_with(&body[..TOOL_RESULT_TRIM_HEAD_BYTES]));
        // Tail bytes kept verbatim (exactly the last TAIL_BYTES).
        assert!(trimmed.ends_with(&body[body.len() - TOOL_RESULT_TRIM_TAIL_BYTES..]));
        // The marker is present and reports the elided count.
        assert!(trimmed.contains(TOOL_RESULT_TRIM_MARKER_SENTINEL));
        // The trimmed body is materially smaller than the original.
        assert!(trimmed.len() < body.len());
        // And the reclaimed body itself is now under threshold (so it won't
        // re-fire and won't dominate the next compaction decision).
        assert!(trimmed.len() < TOOL_RESULT_TRIM_THRESHOLD_BYTES);
    }

    #[test]
    fn marker_reports_directive_and_literal_counts() {
        let body = "z".repeat(6000);
        let trimmed = soft_trim_body(&body).expect("trims");
        // Literal: names a byte count.
        assert!(trimmed.contains("elided "));
        assert!(trimmed.contains("bytes from the middle"));
        // Directive: tells the model what action recovers the omitted bytes.
        assert!(trimmed.contains("Re-run the tool"));
        // Self-identifying provenance prefix.
        assert!(trimmed.contains("[darkmux-runtime:"));
    }

    #[test]
    fn trim_is_idempotent() {
        let body = "q".repeat(7000);
        let once = soft_trim_body(&body).expect("first trim applies");
        // Second pass over the already-trimmed body is a no-op.
        assert!(
            soft_trim_body(&once).is_none(),
            "an already-trimmed body carries the sentinel and must be skipped"
        );
    }

    #[test]
    fn utf8_multibyte_head_and_tail_do_not_panic_and_land_on_boundaries() {
        // Fill head, middle, and tail regions with 4-byte emoji so the raw
        // HEAD_BYTES / (len - TAIL_BYTES) indices land MID-codepoint and must
        // be clamped to char boundaries. "🎉" is 4 bytes.
        let body = "🎉".repeat(3000); // 12000 bytes, way over threshold
        let trimmed = soft_trim_body(&body).expect("trims");
        // If any slice split a codepoint this would have panicked already.
        // The kept head is only whole emoji.
        let head_region = trimmed.split(TOOL_RESULT_TRIM_MARKER_SENTINEL).next().unwrap();
        assert!(
            head_region.chars().all(|c| c == '🎉' || c == '\n'),
            "head must contain only whole codepoints"
        );
        // Round-trips as valid UTF-8 (String guarantees it, but assert the
        // kept head is <= the byte cap after clamping DOWN).
        assert!(head_region.len() <= TOOL_RESULT_TRIM_HEAD_BYTES + 1);
    }

    #[test]
    fn preserve_recent_window_is_not_trimmed() {
        let big = "y".repeat(6000);
        // 8 messages; PRESERVE_RECENT=6, so only indices [0, 1] are eligible.
        let mut messages = vec![
            tool_msg(&big), // 0 eligible -> trimmed
            tool_msg(&big), // 1 eligible -> trimmed
            tool_msg(&big), // 2 protected
            tool_msg(&big), // 3 protected
            tool_msg(&big), // 4 protected
            tool_msg(&big), // 5 protected
            tool_msg(&big), // 6 protected
            tool_msg(&big), // 7 protected
        ];
        let stats = soft_trim_old_tool_results(&mut messages);
        assert_eq!(stats.results_trimmed, 2, "only the 2 old results are trimmed");
        assert!(stats.bytes_reclaimed > 0);
        // The protected tail bodies are byte-identical to the original.
        for m in &messages[2..] {
            assert_eq!(m.content.as_deref().unwrap().len(), big.len());
        }
        // The old bodies were reclaimed.
        for m in &messages[..2] {
            assert!(m.content.as_deref().unwrap().len() < big.len());
        }
    }

    #[test]
    fn non_tool_messages_are_never_trimmed() {
        let big = "w".repeat(6000);
        let mut messages = vec![
            Message::user(big.clone()),   // 0 — user, huge, NOT a tool result
            Message::assistant(&big),     // 1 — assistant, huge, NOT a tool result
            tool_msg("small"),            // 2
            tool_msg("small"),            // 3
            tool_msg("small"),            // 4
            tool_msg("small"),            // 5
            tool_msg("small"),            // 6
            tool_msg("small"),            // 7
        ];
        let stats = soft_trim_old_tool_results(&mut messages);
        assert_eq!(stats.results_trimmed, 0, "only role=='tool' bodies are eligible");
        assert_eq!(messages[0].content.as_deref().unwrap().len(), big.len());
    }
}
