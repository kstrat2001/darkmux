//! Tool-result head/tail elision for the hard bounds (#2792).
//!
//! When a request or a compaction excerpt must fit a window, the largest
//! tool-result bodies lose their middle behind an explicit marker (keep the
//! head and tail, state how many bytes went and how to get them back). This
//! module holds that cut and the last-resort [`hard_trim_to_fit`].
//!
//! ## No soft trim (#3202)
//!
//! #1391 also soft-trimmed every result over 4,000 bytes once it was more
//! than six messages back, as each request was built. That rewrote a message
//! the engine had already cached, so every prefix-caching engine (the
//! Messages API, LM Studio, lithos-metal, Splash) fell back to the head of
//! its cache and prefilled the rest again. Measured on lithos-metal: 526 s of
//! a 908 s run was prefill on the eight requests that missed. Trimming once
//! at insertion instead would have kept the cache but meant the model never
//! saw a large result whole, even on the turn that asked for it. So tool
//! results enter the conversation whole and darkmux never rewrites them;
//! compaction bounds the conversation, and [`hard_trim_to_fit`] is the last
//! resort before a request that would not fit.
//!
//! ## Prior art
//!
//! Design convergence with xai-org/grok-build (Apache-2.0), `xai-chat-state`
//! `PruningConfig` (head/tail elision around a marker). Independently
//! implemented here; no code copied.
//!
//! ## Safety
//!
//! - Head/tail cuts land on UTF-8 char boundaries (the #329 class of
//!   slice-mid-codepoint panics is guarded by clamping to the nearest
//!   boundary), so multibyte tool output (CJK / emoji / accents) is safe.

use crate::lmstudio::Message;

/// Sentinel that marks an already-elided body: the self-identifying prefix
/// of the model-facing elision marker, so the marker doubles as the guard
/// against re-eliding a body for no gain. (#1391)
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

/// (#2792) Elide a body's middle down toward `floor_bytes`, keeping a head
/// and tail around the elision marker.
///
/// Split out so the HARD path could choose its own floor. The #1391 soft
/// trim (removed in #3202) hard-coded constants tuned for "never disturb an
/// ordinary short result" — the exact reason the hard pass reclaimed nothing
/// on a thread made entirely of medium-sized results.
///
/// Returns `None` only when the body is already elided (idempotent) or is too
/// small for head + marker + tail to be shorter than what it replaces.
pub fn trim_body_to(body: &str, floor_bytes: usize) -> Option<String> {
    // AN ALREADY-ELIDED BODY MAY BE ELIDED FURTHER, and refusing to was what
    // made #2808's bound inert on the modal thread.
    //
    // The soft trim (#1391, removed in #3202) stamped the sentinel on every
    // result over 4,000 bytes outside a recent window — which was most of
    // the compaction middle by the time a threshold tripped. A flat "contains the
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
/// context-window bound. Since #3202 it is the only thing that ever elides a
/// tool result in the conversation; nothing else rewrites one.
///
/// Measured on a real run, ONE tool result landing after the last compaction
/// carried the next request to 38,439 tokens against a declared 32,000, four
/// times in 27 turns. Compaction cannot help — it runs BETWEEN turns, and
/// this growth happens WITHIN one, after the last compaction and before the
/// send.
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
        // (#2792 round-2) Do NOT borrow the soft trim's floor (#1391, since
        // removed). Its 4,000-byte threshold was chosen for the SOFT path's purpose —
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elided_body_keeps_its_head_and_tail_around_the_marker() {
        let body = format!("HEAD_{}_TAIL", "m".repeat(9_000));
        let trimmed = trim_body_to(&body, 3_200).expect("a large body elides");
        assert!(trimmed.starts_with("HEAD_"));
        assert!(trimmed.ends_with("_TAIL"));
        assert!(trimmed.len() < body.len());
        // The model-facing note, unchanged (contract 6): literal counts, a
        // directive, and the self-identifying provenance prefix.
        assert!(trimmed.contains(&elision_marker(body.len() - 3_000, 1_500, 1_500)), "{trimmed}");
        assert!(trimmed.contains("bytes from the middle of this tool result; kept the first 1500 and last 1500 bytes. Re-run the tool to retrieve the omitted section if you need it.]"));
        assert!(trimmed.contains(TOOL_RESULT_TRIM_MARKER_SENTINEL));
    }

    #[test]
    fn eliding_again_at_the_same_floor_is_a_no_op() {
        let once = trim_body_to(&"q".repeat(7_000), 3_200).expect("first elision applies");
        assert!(trim_body_to(&once, once.len()).is_none(), "no gain, no churn");
    }

    #[test]
    fn utf8_multibyte_head_and_tail_do_not_panic_and_land_on_boundaries() {
        // 4-byte emoji everywhere, so the raw cut indices land MID-codepoint
        // and must be clamped to char boundaries.
        let body = "🎉".repeat(3000); // 12000 bytes
        let trimmed = trim_body_to(&body, 3_200).expect("trims");
        let head_region = trimmed.split(TOOL_RESULT_TRIM_MARKER_SENTINEL).next().unwrap();
        assert!(head_region.chars().all(|c| c == '🎉' || c == '\n'), "head must contain only whole codepoints");
        assert!(head_region.len() <= 1_500 + 1);
    }
}
