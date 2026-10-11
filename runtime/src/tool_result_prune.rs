//! Tool-result head/tail soft-trim (#1391).
//!
//! Oversized tool results sit in the transcript verbatim, and on small
//! context windows they are the single biggest driver of the FIRST
//! compaction. Middle-eliding a large tool result (keep the head + tail,
//! drop the middle behind a marker) is a cheap, lossy-but-bounded context
//! reclaim that DELAYS the first compaction — on a tight window that can
//! mean one fewer compaction per dispatch, which is measurable wall-clock.
//!
//! ## When: once, as the result enters the conversation (#3202)
//!
//! The loop trims a result as it appends it ([`trim_at_insertion`]), to its
//! final form, and never changes it afterwards. This used to run as each
//! request was built, on results more than six messages back, which sent a
//! large result whole for a few turns and then shortened it. That rewrote a
//! message the engine had already cached, so every prefix-caching engine
//! (the Messages API, LM Studio, lithos-metal, Splash) fell back to the
//! head of its cache and prefilled the rest again. Measured on lithos-metal:
//! 526 s of a 908 s run was prefill on the eight requests that missed.
//!
//! The price is that the model never reads a result over the threshold
//! whole, not even on the turn that asked for it: it reads the head and
//! tail around the marker, which tells it to re-run the tool for the
//! omitted section (a narrower `read` range, a filtered command).
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
//! - Only results over [`TOOL_RESULT_TRIM_THRESHOLD_BYTES`] are touched;
//!   an ordinary result enters the conversation whole.
//! - The trim is idempotent: an already-trimmed body carries the marker
//!   sentinel and is skipped.
//! - Head/tail cuts land on UTF-8 char boundaries (the #329 class of
//!   slice-mid-codepoint panics is guarded by clamping to the nearest
//!   boundary), so multibyte tool output (CJK / emoji / accents) is safe.

use crate::lmstudio::Message;

/// Byte length above which a tool-result body is soft-trimmed.
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

/// Sentinel that marks an already-trimmed body. Presence of this
/// substring makes the pass idempotent (a trimmed body is skipped on
/// subsequent turns). It is the self-identifying prefix of the
/// model-facing elision marker, so the marker doubles as the guard. (#1391)
pub const TOOL_RESULT_TRIM_MARKER_SENTINEL: &str = "[darkmux-runtime: elided";

/// Outcome of one trim pass over the transcript.
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
/// and literal, with a self-identifying `[darkmux-runtime: ...]` prefix
/// as provenance. It states exactly how many bytes were removed and how
/// to recover them, so a fresh-context model reads it as an instruction,
/// not as prose to imitate. (#1391)
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

/// (#2792) Elide a body's middle down toward `floor_bytes`, keeping a head
/// and tail around the same marker [`soft_trim_body`] uses.
///
/// Split out so the HARD path can choose its own floor. `soft_trim_body`
/// hard-codes the #1391 constants, which are tuned for "never disturb an
/// ordinary short result" — a correct goal for the soft pass and the exact
/// reason the hard pass reclaimed nothing on a thread made entirely of
/// medium-sized results.
///
/// Returns `None` only when the body is already elided (idempotent) or is too
/// small for head + marker + tail to be shorter than what it replaces.
pub fn trim_body_to(body: &str, floor_bytes: usize) -> Option<String> {
    // AN ALREADY-ELIDED BODY MAY BE ELIDED FURTHER, and refusing to was what
    // made #2808's bound inert on the modal thread.
    //
    // The soft trim (#1391) stamps the sentinel on every result over 4,000
    // bytes (since #3202, as each one enters the conversation) — which is
    // most of the compaction middle by the time a threshold trips. A flat "contains the
    // sentinel, refuse" guard therefore made every one of those bodies
    // untouchable to the hard bound, so a middle that was ~100% tool-result
    // bytes reported "the weight is not in tool-result bodies" and refused
    // the compaction. Measured: 12 reads of 6,000 bytes each, soft-trimmed
    // to ~3,180, left a 36,502-character excerpt against a ~32,000 budget
    // with `results_trimmed = 1`. Without the prior soft trim, the identical
    // thread fit.
    //
    // The guard's PURPOSE is idempotency — the soft pass asking for 4,000 on
    // a body already at 3,180 must not churn it. That purpose is served by
    // comparing the ask to the body, not by the sentinel's mere presence: a
    // floor at or above the current size has nothing to gain and still
    // refuses. A floor BELOW it is a genuine request for a smaller body, and
    // the re-elision is honest — the head and tail shrink around a fresh
    // marker, and the previous marker falls inside the newly elided middle.
    if body.contains(TOOL_RESULT_TRIM_MARKER_SENTINEL) && floor_bytes >= body.len() {
        return None;
    }
    // Split the floor between head and tail, leaving room for the marker.
    let side = floor_bytes.saturating_sub(200) / 2;
    if side == 0 {
        return None;
    }
    let head_end = floor_char_boundary(body, side);
    let tail_start = ceil_char_boundary(body, body.len().saturating_sub(side));
    if head_end >= tail_start {
        return None;
    }
    let elided = tail_start - head_end;
    let head = &body[..head_end];
    let tail = &body[tail_start..];
    let marker = elision_marker(elided, head_end, body.len() - tail_start);
    let out = format!("{head}{marker}{tail}");
    // Only worth it if it actually shrinks.
    if out.len() >= body.len() {
        return None;
    }
    Some(out)
}

/// (#2792) LAST-RESORT trim to bring an assembled prompt under a hard
/// context-window bound, cutting deeper than the soft trim's threshold and
/// head + tail allow.
///
/// The soft trim exists to reclaim transcript without disturbing an ordinary
/// result, so it leaves every body at or under 4,000 bytes alone, and until
/// #3202 it also spared the last six messages. That restraint is correct for
/// its purpose and is what left this case unhandled: measured on a real run,
/// ONE tool result landing in that recent window carried the next request to
/// 38,439 tokens against a declared 32,000, four times in 27 turns. Since
/// #3202 such a result enters already trimmed, but a thread of many results
/// at or under 4,000 bytes (or a seeded one) can still overflow. Compaction cannot help — it runs BETWEEN turns, and this growth
/// happens WITHIN one, after the last compaction and before the send.
///
/// On a model loaded at the window its profile declares, that request is
/// refused outright (HTTP 400, "the number of tokens to keep from the initial
/// prompt is greater than the context length") and the dispatch dies. So the
/// choice here is not "trim or keep" — it is "trim, or send something already
/// known to fail". Trimming wins, and it is not silent: every trimmed body
/// keeps its head and tail around an explicit elision marker, so the model can
/// see that it is reading an excerpt.
///
/// Largest first, so the fewest bodies are touched to get under the bound, and
/// it stops the moment it fits. Returns the bodies trimmed and bytes
/// reclaimed; a caller that gets `results_trimmed == 0` on an over-window
/// thread has a thread whose weight is NOT in tool results and must say so
/// rather than pretend it acted.
pub fn hard_trim_to_fit(
    messages: &mut [Message],
    target_bytes: usize,
    min_body_bytes: usize,
) -> ToolTrimStats {
    let mut stats = ToolTrimStats::default();
    let mut current: usize = messages
        .iter()
        .map(|m| {
            m.content.as_ref().map(|s| s.len()).unwrap_or(0)
                + m.tool_calls
                    .as_ref()
                    .map(|tcs| {
                        tcs.iter()
                            .map(|tc| tc.function.name.len() + tc.function.arguments.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum();
    if current <= target_bytes {
        return stats;
    }

    // Index the trimmable tool results by body size, largest first.
    let mut candidates: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .filter_map(|(i, m)| m.content.as_ref().map(|b| (i, b.len())))
        .collect();
    candidates.sort_by_key(|&(_, len)| std::cmp::Reverse(len));

    for (idx, _) in candidates {
        if current <= target_bytes {
            break;
        }
        // `candidates` was built by `filter_map` on `content.as_ref()` and
        // nothing clears a body in between, so this is always `Some`.
        let Some(body) = messages[idx].content.as_deref() else {
            continue;
        };
        // (#2792 round-2) Do NOT delegate the floor to `soft_trim_body`. Its
        // 4,000-byte threshold was chosen for the SOFT path's purpose —
        // "ordinary short results are never disturbed" — which is right for
        // delaying a compaction and wrong for a last-resort bound. Measured:
        // 40 results of 3,000 bytes each, 120,000 bytes against a 32,000
        // budget, reclaimed ZERO and sent the lot, because no single body
        // cleared 4,000. That is the modal long-agentic shape (many grep /
        // read / bash outputs), not a corner.
        if body.len() <= min_body_bytes {
            continue;
        }
        // (#2808 round-2) TAKE ONLY WHAT IS NEEDED. This cut every candidate
        // straight to `min_body_bytes`, checking the target only BETWEEN
        // bodies, so a middle whose weight is ONE large tool result
        // overshot catastrophically: measured 45,121 characters against a
        // 31,328-character budget reduced to 612 — 2% of the budget used,
        // 98% of the middle discarded when ~30% would have done.
        //
        // That is not merely wasteful. The compaction excerpt is built from
        // this, so the summary was asked to stand in for a middle it had
        // barely seen, and the #1389 min-reduction guard then refused the
        // compaction outright. Same thread, same mock: no compactor window
        // installed 6,014 chars; a 16,000-token window refused. The fix for
        // #2808 made compaction FAIL on a shape it had handled, which is the
        // pathology #2808 is about, reached through its own fix.
        //
        // This was landed once before as a #2792 follow-up and reverted,
        // because precision here holds the thread closer to the window and
        // that starved a compactor whose excerpt was bounded by nothing. The
        // two changes fix each other and belong together: bound the excerpt
        // (this PR) and stop over-eliding it (this hunk). Do not split them
        // again.
        let overshoot = current.saturating_sub(target_bytes);
        let allowed = body.len().saturating_sub(overshoot).max(min_body_bytes);
        let Some(trimmed) = trim_body_to(body, allowed) else {
            continue; // already trimmed
        };
        let reclaimed = body.len().saturating_sub(trimmed.len());
        messages[idx].content = Some(trimmed);
        stats.results_trimmed += 1;
        stats.bytes_reclaimed += reclaimed;
        current = current.saturating_sub(reclaimed);
    }
    stats
}

/// (#3202) The tool-result message the loop appends to the conversation:
/// the body in its FINAL form. A body over the threshold is soft-trimmed
/// here, once, and nothing changes it afterwards, so every later request
/// extends the one before it and the engine's prompt cache stays valid. A
/// body at or under the threshold enters whole. The second value is the
/// bytes the trim removed (0 when it left the body whole).
pub fn trim_at_insertion(tool_call_id: impl Into<String>, name: impl Into<String>, body: String) -> (Message, usize) {
    let (body, reclaimed) = match soft_trim_body(&body) {
        Some(trimmed) => {
            let reclaimed = body.len() - trimmed.len();
            (trimmed, reclaimed)
        }
        None => (body, 0),
    };
    (Message::tool_result(tool_call_id, name, body), reclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_large_result_enters_the_conversation_trimmed_and_a_small_one_whole() {
        let big = "y".repeat(6000);
        let (message, reclaimed) = trim_at_insertion("call_1", "bash", big.clone());
        let body = message.content.as_deref().unwrap();
        assert_eq!(body, soft_trim_body(&big).unwrap(), "the soft trim's own final form");
        assert_eq!(reclaimed, big.len() - body.len());
        assert_eq!((message.role.as_str(), message.tool_call_id.as_deref()), ("tool", Some("call_1")));

        let exactly = "z".repeat(TOOL_RESULT_TRIM_THRESHOLD_BYTES);
        let (message, reclaimed) = trim_at_insertion("call_2", "bash", exactly.clone());
        assert_eq!(message.content.as_deref(), Some(exactly.as_str()), "at the threshold it enters whole");
        assert_eq!(reclaimed, 0);
    }
}
