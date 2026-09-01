//! Reasoning-loop detection — flag turns whose normalized reasoning
//! content matches a recent turn's reasoning content within a sliding
//! window.
//!
//! Part of [#461](https://github.com/kstrat2001/darkmux/issues/461),
//! sibling of [`crate::cycle_detector`] (#418). The cycle detector
//! catches **tool-side** repetition (same tool + same canonical args).
//! This module catches **reasoning-side** repetition — the model
//! cycling through the same line of thought across turns while every
//! tool call looks unique because the model is searching for
//! different angles on the same blocked situation.
//!
//! Run-5-shaped case (Beat 54 N=5): the model produced 14K reasoning
//! chars + a 24KB write in one turn, after heavy upstream reasoning.
//! Zero existing detectors fired because every tool call was unique.
//! The reasoning was visibly stuck to the operator; nothing surfaced
//! it to the model.
//!
//! ## Scope (MVP)
//!
//! - **Inter-turn exact-hash detection only.** Hash each turn's
//!   normalized reasoning content (lowercase, whitespace-collapsed);
//!   fire when the same hash appears N+ times in a window.
//! - **Intra-turn substring detection** (a single turn whose
//!   reasoning self-repeats) deferred to a follow-up.
//! - **Content-field repetition** (same shape but on `content`
//!   instead of `reasoning_content`) deferred to a follow-up — the
//!   `content` field is usually the final answer, so its inter-turn
//!   repetition is much rarer in practice.
//! - **Fuzzy-match detection** (Levenshtein, cosine, embedding)
//!   deferred — exact hash is the cheap shape that catches the
//!   high-value cases first.
//!
//! ## Design
//!
//! - Sliding window of the most recent N reasoning hashes (default N=10).
//! - Per turn, compute hash from `normalize(reasoning_content)`. Empty
//!   or pure-whitespace reasoning is skipped (no signal possible).
//! - When the same hash appears K times within the window (default K=3),
//!   emit a [`ReasoningLoopSignal::Suspected`].
//! - Edge-triggered: the same hash continuing to recur at or above
//!   threshold returns no further signal until **any** non-fired hash
//!   lands. The suppression is intentionally permissive — a returning
//!   stuck pattern (e.g., model briefly diverges then re-enters the
//!   loop) can fire again on its next threshold crossing, which is
//!   the right direction for an observability signal (false positives
//!   are mild; false negatives miss the case the detector exists for).
//!   See [#476] for the tighter "must move on with multiple distinct
//!   hashes" alternative — deferred per pre-N=5 reviewer guidance.
//! - One instance per dispatch.
//!
//! ## Non-cryptographic hash
//!
//! Uses [`std::collections::hash_map::DefaultHasher`]. This is identity
//! comparison — two strings hashing the same means they really are the
//! same (after normalization). The detector explicitly does NOT need
//! collision resistance against adversarial inputs; the model isn't
//! adversarial in that direction. Keeping runtime dep-set lean (the
//! "don't add dependencies casually" doctrine in CLAUDE.md) is the
//! reason for not pulling in `blake3` for this layer.

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Default window size — how many recent reasoning hashes to track.
/// Matches the cycle detector default (#418).
pub const DEFAULT_WINDOW_SIZE: usize = 10;

/// Default threshold — how many appearances of the same hash within
/// the window before firing the suspected signal. Matches the cycle
/// detector default.
pub const DEFAULT_WARN_THRESHOLD: usize = 3;

/// Minimum normalized-reasoning length to count toward detection.
/// Below this, the reasoning is too short for repetition to be a
/// meaningful signal (e.g., a 20-char internal note like "Now let
/// me check this" is naturally repeated and not a stuck-pattern
/// indicator).
pub const DEFAULT_MIN_REASONING_LEN: usize = 80;

/// (#461) Result of recording a turn's reasoning into the detector.
/// A `Suspected` outcome is observability + feedback-injection in
/// the MVP — no bail. Bail-on-threshold can layer on later if the
/// model-facing nudge isn't enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningLoopSignal {
    /// First time we've seen this many of this hash in the window.
    /// Edge-triggered: same-hash continuation returns None until a
    /// different hash lands.
    Suspected {
        /// Number of times the matching hash appeared in the window
        /// at trigger time.
        count: usize,
        /// Total window size at trigger time — for the feedback
        /// template's "N times in M turns" framing.
        window_size: usize,
    },
}

/// Sliding-window reasoning-loop detector. One instance per dispatch.
pub struct ReasoningLoopDetector {
    window: VecDeque<u64>,
    window_size: usize,
    warn_threshold: usize,
    min_reasoning_len: usize,
    /// Hash that most recently fired the signal. Until a different
    /// hash lands, continued recurrence is suppressed so the model
    /// doesn't get nudged on every turn after the first firing.
    last_fired_hash: Option<u64>,
}

impl ReasoningLoopDetector {
    pub fn new() -> Self {
        Self::with_params(
            DEFAULT_WINDOW_SIZE,
            DEFAULT_WARN_THRESHOLD,
            DEFAULT_MIN_REASONING_LEN,
        )
    }

    pub fn with_params(
        window_size: usize,
        warn_threshold: usize,
        min_reasoning_len: usize,
    ) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            window_size,
            warn_threshold,
            min_reasoning_len,
            last_fired_hash: None,
        }
    }

    /// Record one turn's reasoning. Returns `Some(Suspected)` when
    /// the same hash crosses the threshold within the sliding window
    /// (and that hash hasn't already fired a signal). Returns `None`
    /// otherwise — including when the reasoning is empty / too short
    /// (no signal possible) or when the same hash continues to
    /// recur after an initial firing (edge-trigger suppression).
    pub fn record(&mut self, reasoning: &str) -> Option<ReasoningLoopSignal> {
        let normalized = normalize(reasoning);
        if normalized.len() < self.min_reasoning_len {
            return None;
        }
        let hash = hash_text(&normalized);

        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back(hash);

        let count = self.window.iter().filter(|h| **h == hash).count();
        if count >= self.warn_threshold && Some(hash) != self.last_fired_hash {
            self.last_fired_hash = Some(hash);
            return Some(ReasoningLoopSignal::Suspected {
                count,
                window_size: self.window_size,
            });
        }

        // Edge-trigger reset: if the most recent hash isn't the one
        // that fired (i.e., the model has moved to a different line
        // of reasoning), clear the suppression flag so a future
        // repeat of the original or a new pattern can fire again.
        if Some(hash) != self.last_fired_hash {
            self.last_fired_hash = None;
        }

        None
    }
}

impl Default for ReasoningLoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a reasoning string for repetition detection.
///
/// - Lowercase (case-only variations shouldn't break the match).
/// - Collapse all whitespace runs to a single space (formatting drift
///   between thinking-block emissions shouldn't break the match).
/// - Trim leading/trailing whitespace.
fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_ws = true;
    for c in lower.chars() {
        if c.is_whitespace() {
            if !last_was_ws {
                out.push(' ');
                last_was_ws = true;
            }
        } else {
            out.push(c);
            last_was_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn hash_text(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_reasoning(seed: &str) -> String {
        // Pad to clear the min-length floor so the detector engages.
        format!(
            "{} - reasoning about the auth flow before making the edit, \
             want to make sure I understand the existing pattern first",
            seed
        )
    }

    #[test]
    fn empty_reasoning_does_not_record() {
        let mut d = ReasoningLoopDetector::new();
        for _ in 0..5 {
            assert_eq!(d.record(""), None);
        }
    }

    #[test]
    fn whitespace_only_reasoning_does_not_record() {
        let mut d = ReasoningLoopDetector::new();
        for _ in 0..5 {
            assert_eq!(d.record("   \n\t   "), None);
        }
    }

    #[test]
    fn too_short_reasoning_does_not_record() {
        // Below the min-length floor — short notes aren't a stuck
        // pattern even if they repeat.
        let mut d = ReasoningLoopDetector::new();
        for _ in 0..5 {
            assert_eq!(d.record("Now let me check this."), None);
        }
    }

    #[test]
    fn three_identical_reasonings_fires_suspected() {
        let mut d = ReasoningLoopDetector::new();
        let r = long_reasoning("turn one");
        assert_eq!(d.record(&r), None, "1st call: count=1, no signal");
        assert_eq!(d.record(&r), None, "2nd call: count=2, still no signal");
        let sig = d.record(&r);
        match sig {
            Some(ReasoningLoopSignal::Suspected { count, window_size }) => {
                assert_eq!(count, 3, "3rd call crosses threshold");
                assert_eq!(window_size, DEFAULT_WINDOW_SIZE);
            }
            None => panic!("expected Suspected on 3rd identical reasoning"),
        }
    }

    #[test]
    fn whitespace_drift_collapses_to_same_hash() {
        // Same content with different whitespace + casing must collide.
        // Each input is padded above the min-length floor so the
        // detector engages on every record() call.
        let mut d = ReasoningLoopDetector::new();
        let base = "Reasoning about the auth flow before making the edit. The pattern is X — \
                    same line of thought as before, want to make sure the existing approach holds.";
        let upper = "REASONING ABOUT THE AUTH FLOW BEFORE MAKING THE EDIT. THE PATTERN IS X — \
                     SAME LINE OF THOUGHT AS BEFORE, WANT TO MAKE SURE THE EXISTING APPROACH HOLDS.";
        let weird_ws = "reasoning  about the auth  flow   before making the edit.\n\nThe pattern \
                        is X — same line of thought  as before, want to   make sure the existing \
                        approach holds.";
        d.record(base);
        d.record(upper);
        let sig = d.record(weird_ws);
        assert!(
            matches!(sig, Some(ReasoningLoopSignal::Suspected { count: 3, .. })),
            "case + whitespace normalization should collapse the three reasonings to identical \
             hashes: {sig:?}"
        );
    }

    #[test]
    fn three_distinct_reasonings_do_not_fire() {
        let mut d = ReasoningLoopDetector::new();
        for seed in &["alpha", "beta", "gamma"] {
            let r = long_reasoning(seed);
            assert_eq!(d.record(&r), None);
        }
    }

    #[test]
    fn edge_triggered_suppresses_continued_recurrence() {
        let mut d = ReasoningLoopDetector::new();
        let r = long_reasoning("stuck");
        d.record(&r);
        d.record(&r);
        let fired = d.record(&r);
        assert!(fired.is_some(), "first crossing fires");
        // Continuing to record the same reasoning should NOT fire
        // again — the model is being nudged once per stuck pattern,
        // not on every subsequent turn.
        assert_eq!(d.record(&r), None, "edge-trigger should suppress");
        assert_eq!(d.record(&r), None, "edge-trigger should still suppress");
    }

    #[test]
    fn different_pattern_after_firing_can_re_arm_detector() {
        // After firing, a different reasoning lands → suppression
        // clears. Then if THAT pattern repeats 3 times, the detector
        // fires again for the new pattern.
        let mut d = ReasoningLoopDetector::new();
        let stuck = long_reasoning("stuck");
        d.record(&stuck);
        d.record(&stuck);
        d.record(&stuck); // fires
        let different = long_reasoning("different");
        d.record(&different); // count=1 for new pattern, clears suppression
        d.record(&different); // count=2
        let resig = d.record(&different);
        assert!(
            matches!(resig, Some(ReasoningLoopSignal::Suspected { count: 3, .. })),
            "new pattern hitting threshold should fire after suppression clears: {resig:?}"
        );
    }

    #[test]
    fn sliding_window_drops_oldest_entries() {
        // With a window of 3, after 4 entries the first should be
        // gone — only the most recent 3 are in scope.
        let mut d = ReasoningLoopDetector::with_params(3, 3, DEFAULT_MIN_REASONING_LEN);
        let a = long_reasoning("a");
        let b = long_reasoning("b");
        d.record(&a); // window: [a]
        d.record(&b); // window: [a, b]
        d.record(&b); // window: [a, b, b]
        d.record(&b); // window: [b, b, b]  ← `a` dropped; threshold crossed for b
        // We expect the LAST recording to have fired — but the
        // detector saw the b-count rise across multiple calls; the
        // suspected signal must be on the FIRST crossing (when count
        // first hit threshold).
        //
        // Rebuild the scenario to assert the firing position cleanly.
        let mut d = ReasoningLoopDetector::with_params(3, 3, DEFAULT_MIN_REASONING_LEN);
        d.record(&a);
        d.record(&b);
        d.record(&b);
        let sig = d.record(&b);
        assert!(
            matches!(sig, Some(ReasoningLoopSignal::Suspected { count: 3, .. })),
            "fourth recording (3rd b in window) should fire: {sig:?}"
        );
    }

    #[test]
    fn returning_pattern_after_full_window_rotation_re_fires() {
        // Pins current (intentionally permissive) re-arm semantics:
        // after a fire, if a SINGLE different reasoning lands,
        // suppression clears. A subsequent recurrence of the original
        // pattern that crosses threshold again WILL fire.
        //
        // Captured behavior, not aspirational. The tighter "must see
        // multiple distinct hashes" alternative is tracked as #476;
        // see the docstring at the module-level for the rationale
        // behind keeping the permissive shape pre-1.0.
        let mut d = ReasoningLoopDetector::new();
        let stuck = long_reasoning("stuck");
        d.record(&stuck);
        d.record(&stuck);
        let first_fire = d.record(&stuck);
        assert!(first_fire.is_some(), "first crossing fires");

        // Single divergence clears suppression (permissive shape).
        let divergent = long_reasoning("brief sidebar");
        d.record(&divergent);

        // The stuck pattern re-enters. The 3 prior X-hashes are
        // still in the 10-slot window, so count crosses threshold on
        // the very next record. With permissive suppression
        // (last_fired_hash was cleared by the single divergence), this
        // re-fires immediately — pins the behavior.
        let refire = d.record(&stuck);
        assert!(
            refire.is_some(),
            "returning stuck pattern re-fires after a single divergence; \
             see #476 for the tighter alternative: {refire:?}"
        );
        // The very next record of the same stuck pattern is suppressed
        // again until another divergence lands. Documents the
        // once-per-fire shape on the new firing window.
        assert_eq!(
            d.record(&stuck),
            None,
            "suppression re-engages immediately after the re-fire"
        );
    }

    #[test]
    fn min_reasoning_len_is_per_normalized_form() {
        // Whitespace-heavy short reasoning normalizes below floor →
        // still skipped. The floor compares normalized length, not
        // raw input length.
        let mut d = ReasoningLoopDetector::with_params(10, 3, 50);
        let s = format!("{:>40}{:<40}", "  ", "short note.");
        for _ in 0..5 {
            assert_eq!(d.record(&s), None);
        }
    }
}

// ── intra-turn degeneracy (#1221) ─────────────────────────────────────────

/// Below this, a reasoning slice is treated as looping rather than working.
///
/// Deliberately far from BOTH measured clusters. Real productive reasoning —
/// the 40,608-char pepper-grinder turn, its body alone, a resumed
/// continuation, and a checkpoint accumulation — all scored **1.000**.
/// Synthetic loops scored **0.013-0.015**. A threshold anywhere in 0.1-0.8
/// separates them, so 0.25 sits with enormous margin on both sides.
///
/// The asymmetry is the reason to keep it low: a false CLEAN costs one more
/// checkpoint, while a false DEGENERATE destroys an analysis pass. When in
/// doubt this must let the model keep working.
pub const DEGENERATE_TAIL_RATIO: f32 = 0.25;

/// Distinct-window ratio over the TAIL of one reasoning slice.
///
/// Tail, not the whole body, because that is where the signature lives: E19
/// measured a 51K-char turn in which only the final ~200 chars tread water
/// while everything before was substance. Averaged over the whole slice that
/// signal vanishes.
///
/// Returns `None` for a slice too short to judge — which the caller must treat
/// as CLEAN, never as degenerate.
///
/// **What this does and does not catch.** It detects repetition, not lack of
/// progress. A model that paraphrases itself in a circle scores well here and
/// will pass. That is a knowing trade: it catches the cheap pathological case
/// with a ~70x margin, and what it misses is bounded by the checkpoint ceiling
/// rather than running forever. Do not read a clean score as "the model is
/// making progress"; read it as "the model is not repeating itself".
pub fn tail_repetition_ratio(slice: &str, window: usize, tail: usize) -> Option<f32> {
    let toks: Vec<&str> = slice.split_whitespace().collect();
    let start = toks.len().saturating_sub(tail);
    let t = &toks[start..];
    if t.len() < window.saturating_mul(3) {
        return None;
    }
    let windows: Vec<String> = t.windows(window).map(|w| w.join(" ")).collect();
    let distinct: std::collections::HashSet<&String> = windows.iter().collect();
    Some(distinct.len() as f32 / windows.len() as f32)
}

/// How many checkpoint intervals of reasoning the gate looks back over.
///
/// The sample is DERIVED from the checkpoint interval rather than set beside
/// it, because the two silently determine the gate's sensitivity together and
/// setting them independently put it on a knife edge.
///
/// A model looping verbatim emits the same slice each interval, so a tail
/// holding `k` copies scores `1/k` — the floor is `interval / sample`. With the
/// sample pinned at 4000 and the interval at 1000, that floor was **0.2507**
/// against a **0.25** threshold: pure verbatim looping asymptotes just ABOVE
/// the line and never fires. The one live run that did conclude (0.2088) only
/// got under because its content was repetitive WITHIN each slice as well.
///
/// Measured across the grid (verbatim floor / real-source-and-prose ceiling):
///
/// | sample | floor | prose |
/// |--------|-------|-------|
/// | 4x interval  | 0.2507 | 0.910 |
/// | 8x interval  | 0.1252 | 0.953 |
/// | 16x interval | 0.0625 | 0.960 |
///
/// 8x puts the threshold at 2x margin above the degenerate floor and ~3.8x
/// below genuine prose, and — the point of deriving it — those margins hold
/// whatever the operator sets the interval to.
pub const TAIL_SAMPLE_INTERVALS: usize = 8;

/// 12-token windows: the size the 1.000-vs-0.015 separation was measured at.
pub const TAIL_WINDOW_TOKENS: usize = 12;

/// The tail the gate judges, for a given checkpoint interval.
pub fn tail_sample_tokens(checkpoint_interval: u32) -> usize {
    (checkpoint_interval as usize).saturating_mul(TAIL_SAMPLE_INTERVALS)
}

// ── the tokenizer's blind spot (#2228) ────────────────────────────────────
//
// `tail_repetition_ratio` above splits on WHITESPACE. A slice containing none
// tokenizes to exactly ONE token, fails the `t.len() < window * 3` guard
// (36 tokens at the shipped window), and returns `None` — which
// `slice_is_degenerate` maps to CLEAN. The canonical degenerate-decoding
// shapes are precisely the whitespace-free ones: `"-"` x N, `"!"` x N,
// no-space CJK (`"。"` x N), and base64/hex blobs. **The gate could never
// fire on the exact failure it exists to catch.**
//
// What that cost, at the call site in `loop_runner`: a model emitting a
// whitespace-free blob to the per-call cap hits `finish_reason=length` every
// call, the checkpoint machinery engages, the verdict comes back `false`, and
// the "hand it back OPEN, continue" branch runs `messages.pop()` +
// `turn.hand_back(..)` — an identical next iteration. Continuations are the
// same logical turn BY DESIGN, so `turns` never increments and
// `runtime.max_turns` cannot bound it (the comment at the call site already
// says so for a different miss). The only backstop left is the host's 600s
// SIGKILL, which produces NO envelope and discards every banked checkpoint.
//
// The fix is a second metric of the SAME SHAPE — distinct windows over total
// windows, sliding by one — at CHAR granularity, engaged only where the
// tokenizer is blind. It is purely ADDITIVE: the token metric runs first and
// unchanged, and the fallback can only turn a CLEAN into a DEGENERATE, never
// the reverse. Every pre-#2228 verdict is still reached for its old reason.
//
// ## Measured separation
//
// Every corpus below is generated by `degeneracy_charlevel_tests`, so the
// table is reproducible rather than remembered. Measured at the shipped
// constants — `TAIL_WINDOW_CHARS` 24, tail 96,000 chars (interval 1000),
// `DEGENERATE_CHAR_RATIO` 0.09, blind at >= 32 chars/token:
//
// | corpus                          | chars/tok | blind | ratio  | verdict    |
// |---------------------------------|-----------|-------|--------|------------|
// | `"-"` x 5,000                   |     5000  | yes   | 0.0002 | DEGENERATE |
// | `"-"` x 64,000                  |    64000  | yes   | 0.0000 | DEGENERATE |
// | `"abc"` cycle x 2,000           |     6000  | yes   | 0.0005 | DEGENERATE |
// | `"。"` x 3,000 (no-space CJK)    |     3000  | yes   | 0.0003 | DEGENERATE |
// | 400-char b64 unit x 20          |     8000  | yes   | 0.0501 | DEGENERATE |
// | 4,000-char b64 unit x 30        |    96000  | yes   | 0.0417 | DEGENERATE |
// | 400-char dash run + index       |      203  | yes   | 0.0559 | DEGENERATE |
// | --- threshold 0.09 ---          |           |       |        |            |
// | 105-char-prefix URL list        |      109  | yes   | 0.2043 | clean      |
// | near-identical JSON records     |    91291  | yes   | 0.3357 | clean      |
// | minified JSON (~110 KB)         |    96000  | yes   | 0.6345 | clean      |
// | URL list (~65 KB)               |       72  | yes   | 0.6211 | clean      |
// | base64 of random bytes (64 KB)  |    64000  | yes   | 1.0000 | clean      |
// | base64 wrapped at 76 cols       |       77  | yes   | 1.0000 | clean      |
// | hex digests, concatenated       |    96000  | yes   | 1.0000 | clean      |
// | hex digests, one per line       |      129  | yes   | 1.0000 | clean      |
// | real CJK prose (729 chars)      |      729  | yes   | 1.0000 | clean      |
// | English prose (~56 K chars)     |      6.3  | NO    | 0.8924 | clean      |
// | Rust source (`cycle_detector`)  |      9.3  | NO    | 0.8303 | clean      |
//
// Margins on both sides of 0.09: the worst DEGENERATE score is 0.0559
// (**1.61x** below) and the worst clean score is 0.2043 (**2.27x** above).
// The two clusters are 3.7x apart with the threshold near their geometric
// midpoint. Both margins are asserted, not remembered — see
// `the_char_threshold_sits_between_both_measured_clusters`. The last two rows
// never reach the fallback at all; they are the standing requirement that
// behavior is unchanged wherever the tokenizer can see.

/// Mean chars per whitespace-token at or above which `split_whitespace()` is
/// not resolving the content, so `tail_repetition_ratio` cannot judge it.
///
/// The trigger is DENSITY, not "contains no whitespace". A 5,000-char blob
/// broken up by forty distinct index tokens has 41 tokens — enough to clear
/// the token metric's length guard — and every 12-token window is distinct
/// because the indices differ, so the token metric scores **1.000** and calls
/// it clean while the content between the indices is a dash run. A few spaces
/// in 5,000 chars is still tokenization-blind, and this constant is what says
/// so. (`a_blob_with_a_few_stray_spaces_is_still_judged` pins it, asserting
/// the token metric's 1.000 as a PRECONDITION so the test cannot silently
/// stop exercising the trigger.)
///
/// Measured chars-per-whitespace-token, the same corpora as the table above:
///
/// | corpus                       | chars/token |
/// |------------------------------|-------------|
/// | English prose                |     **6.3** |
/// | Rust source                  |     **9.3** |
/// | --- threshold 32 ---         |             |
/// | dash run + index tokens      |        62.9 |
/// | URL list, one per line       |        70.9 |
/// | base64 wrapped at 76 cols    |        76.9 |
/// | URL list, 105-char prefixes  |       109.2 |
/// | hex digests, one per line    |       128.9 |
/// | anything with NO whitespace  |    >= 729.0 |
///
/// 32 sits **5.1x** above the highest measured whitespace-bearing corpus and
/// **1.97x** below the lowest blind one. Nothing measured lands between 9.3
/// and 62.9, which is why the exact value is not delicate.
///
/// **This guard is LOAD-BEARING — deleting it changes verdicts.** An earlier
/// version of this paragraph claimed the opposite ("structural, not active"),
/// reasoning that condemnation needs an invariant run of ~250 chars, "i.e."
/// ~44 chars/token. That `i.e.` is false: a long invariant unit that carries
/// its OWN whitespace is long and LOW-density at the same time. Markup and
/// minified CSS are exactly that shape, and this trigger is the only thing
/// keeping them clean. Measured (tail 96,000, W=24) — an HTML row whose
/// invariant prefix carries a space every ~26 chars:
///
/// | invariant chars | chars/token | char ratio | token ratio | without the guard |
/// |-----------------|-------------|------------|-------------|-------------------|
/// | 120             |         7.9 |     0.1573 |      0.6672 | clean             |
/// | 200             |         7.4 |     0.1013 |      0.4003 | clean             |
/// | **240**         |         7.3 | **0.0861** |      0.3337 | **false positive**|
/// | **260**         |         7.2 | **0.0801** |      0.3078 | **false positive**|
/// | **300**         |         7.2 | **0.0704** |      0.2666 | **false positive**|
/// | 360             |         7.1 |     0.0596 |  **0.2226** | degenerate anyway |
///
/// The zone is bounded on BOTH sides. Below ~240 the char metric does not
/// condemn, so there is nothing to save. Above ~340 the TOKEN metric
/// condemns on its own (0.2226 against its own 0.25 at 360), so the verdict
/// is the same with or without the guard — a test written at 360 would pass
/// with the guard deleted, for the wrong reason. The guard decides only the
/// middle band.
///
/// Every row is SIGHTED by a wide margin (7.x against a threshold of 32), so
/// the token metric judges them and resolves them correctly; the char metric
/// alone would escalate a legitimate markup answer and lose the work.
/// `the_density_trigger_is_load_bearing_on_whitespace_carrying_units` pins
/// this and fails if the guard is removed.
///
/// Why a sweep did not find it: the conformance test's corpus is a dash run
/// plus four short tokens, which pins chars/token to about `run/6`, so long
/// unit and low density cannot co-occur in it. Absence of a counterexample
/// from one generator is not absence of the class.
pub const TOKENIZER_BLIND_CHARS_PER_TOKEN: f32 = 32.0;

/// Below this, a whitespace-free tail is treated as looping rather than
/// working. The char-metric twin of [`DEGENERATE_TAIL_RATIO`].
///
/// **Why not simply reuse 0.25.** The shape is the same, so the reasoning
/// carries over — but the CLUSTERS do not sit where the token metric's do.
/// Whitespace-free content that is legitimately repetitive at char scale is
/// real and common: a URL list whose entries share a 105-char prefix scores
/// 0.2043 and near-identical JSON records score 0.3357. At 0.25 the first is
/// a FALSE POSITIVE outright and the second clears by 1.34x, which is not a
/// margin. The token metric never meets these shapes because whitespace
/// tokenization dissolves them (the same URL list scores 1.000 by token).
///
/// 0.09 is placed near the geometric midpoint of the two measured clusters
/// (0.0559 and 0.2043 — see the table above), giving 1.61x below and 2.27x
/// above **for that measured set**. Read those margins as a property of the
/// CORPORA, not of the metric: sweeping machine-generated shapes drives the
/// legitimate floor much lower and some cross it. Recorded here so the next
/// person tuning this constant starts from the counterexamples rather than
/// rediscovering them — minified CSS with a ~185-char invariant body scores
/// 0.1070 (a 1.19x margin, not 2.27x); at ~300 chars it scores 0.0715, and a
/// 290-char flow-record JSONL line scores 0.0789. Both are FALSE POSITIVES.
/// For constructed shapes the legitimate cluster extends down to ~0.04.
///
/// No REAL artifact in this repo reaches it — the served viewer bundle and
/// the demo page score 0.7263, `Cargo.lock` 0.6274, `bun.lock` 0.8846, and
/// darkmux's own flow JSONL 0.12-0.74 — which is why 0.09 ships. But the
/// headroom is narrower than the table alone suggests, and the trade that
/// justifies it is that a false DEGENERATE returns `EscalationTriggered`
/// WITH the work attached, where the bug it replaces was a 600s SIGKILL with
/// no envelope at all. The asymmetry in [`DEGENERATE_TAIL_RATIO`]'s doc still governs the
/// direction to err: a false CLEAN costs one more checkpoint, while a false
/// DEGENERATE either force-closes a working model's thought or returns
/// `EscalationTriggered` on a legitimate answer.
pub const DEGENERATE_CHAR_RATIO: f32 = 0.09;

/// 24-char windows.
///
/// `W` trades the two error directions against each other, and the trade is
/// one-directional: verbatim repetition scores `unit / tail` regardless of
/// `W`, while every shape with SOME variation — legitimate boilerplate and
/// near-miss loops alike — rises with `W`. So growing `W` widens the gap
/// between the verbatim cluster and legitimate structured content, and pays
/// for it by conceding loops built from a long invariant unit plus a small
/// varying element.
///
/// Measured (tail 96,000 chars, interval 1000). `DEG` = must fire,
/// `LEG` = must not:
///
/// | corpus                          | W=12   | W=16   | W=20   | W=24   | W=32   |
/// |---------------------------------|--------|--------|--------|--------|--------|
/// | DEG 1,000-char b64 unit x 60    | 0.0167 | 0.0167 | 0.0167 | 0.0167 | 0.0167 |
/// | DEG 4,000-char b64 unit x 60    | 0.0417 | 0.0417 | 0.0417 | 0.0417 | 0.0417 |
/// | DEG 400-char dash run + index   | 0.0260 | 0.0358 | 0.0456 | 0.0554 | 0.0750 |
/// | DEG 200-char dash run + index   | 0.0500 | 0.0693 | 0.0886 | 0.1079 | 0.1465 |
/// | DEG 120-char dash run + index   | 0.0808 | 0.1122 | 0.1437 | 0.1752 | 0.2381 |
/// | LEG 105-char-prefix URL list    | 0.0947 | 0.1312 | 0.1678 | 0.2043 | 0.2775 |
/// | LEG near-identical JSON records | 0.1519 | 0.2132 | 0.2745 | 0.3357 | 0.4583 |
///
/// The binding pair is the last DEG row against the first LEG row: they are
/// the SAME statistical shape (a long invariant unit plus a tiny varying
/// element) and differ only in whether the invariant part means anything, so
/// no `W` and no threshold separates them — see the gap list on
/// [`slice_is_degenerate`]. Taking that concession as given, the question
/// becomes how far the realistic verbatim ceiling (0.0417) sits below the
/// legitimate floor: 2.3x at W=12, 3.1x at W=16, 4.9x at W=24, 6.7x at W=32.
/// 24 is where both margins around 0.09 clear 2x (1.80x / 2.27x) while the
/// window still spans less than the ~58-char JSON boilerplate and ~105-char
/// URL prefixes it has to see through. W=32 buys a slightly wider legit
/// margin and concedes the 200-char dash row as well; W=12 is rejected
/// outright — its legit floor of 0.0947 is a 1.05x margin, i.e. none.
pub const TAIL_WINDOW_CHARS: usize = 24;

/// How many chars of whitespace-free output one checkpoint interval's worth
/// of tokens is assumed to be worth, when sizing the char tail.
///
/// [`TAIL_SAMPLE_INTERVALS`] fixes the tail at 8 checkpoint intervals so a
/// verbatim loop scores `1/8`. That count is in TOKENS, and this fallback
/// runs precisely where the token count is meaningless, so the tail has to be
/// converted to chars — `tail = TAIL_SAMPLE_INTERVALS * this * interval`.
///
/// The conversion has a derivable requirement rather than a guessed value.
/// With a unit of `C` chars per real token, `tail / unit` copies fit in the
/// tail and the ratio bottoms out at `C / (TAIL_SAMPLE_INTERVALS * M)`. To
/// stay under the threshold that needs `M > C / (8 * 0.09)`, i.e.
/// **`M > 1.39 * C`** — INDEPENDENT of the interval, which is the same
/// property [`TAIL_SAMPLE_INTERVALS`] was derived for. Measured
/// chars-per-real-token by content class: base64 ~4 (3 bytes to 4 chars,
/// and BPE rarely splits finer), hex ~2, minified JSON ~3, CJK ~1-1.5. At
/// `C = 4` the requirement is `M > 5.6`; **12 covers `C` up to 8.6**, a 2.1x
/// overestimate of the worst measured class.
///
/// Overestimating is the safe direction, and this is measured rather than
/// assumed: legitimate ratios are tail-INVARIANT while the verbatim floor
/// scales as `1/tail`, so a longer tail only lowers the degenerate side.
/// At W=16, sweeping the multiplier:
///
/// | corpus                          | 4x     | 8x     | 12x    | 16x    | 24x    |
/// |---------------------------------|--------|--------|--------|--------|--------|
/// | 4,000-char b64 unit x 30 (DEG)  | 0.1251 | 0.0625 | 0.0417 | 0.0333 | 0.0333 |
/// | 105-char-prefix URL list (LEG)  | 0.1349 | 0.1322 | 0.1312 | 0.1293 | 0.1292 |
/// | near-identical JSON (LEG)       | 0.2192 | 0.2182 | 0.2132 | 0.2132 | 0.2132 |
/// | base64 wrapped 76 cols (LEG)    | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 |
///
/// 4x is where a verbatim base64 loop (0.1251) crosses the legitimate floor
/// (0.1349) and the metric stops working at all; 16x and beyond buy nothing
/// the 30-copy corpus can show. The cost of the larger tail is bounded: at
/// the default 1000-token interval it is 96,000 chars, and the `HashSet` of
/// `&[char]` window slices allocates no window contents.
pub const TAIL_CHARS_PER_TOKEN: usize = 12;

/// The char tail the fallback judges, for a given checkpoint interval.
pub fn tail_sample_chars(checkpoint_interval: u32) -> usize {
    tail_sample_tokens(checkpoint_interval).saturating_mul(TAIL_CHARS_PER_TOKEN)
}

/// The last `tail` CHARS of `slice`, as chars — never bytes.
fn tail_chars(slice: &str, tail: usize) -> Vec<char> {
    let mut v: Vec<char> = slice.chars().rev().take(tail).collect();
    v.reverse();
    v
}

/// Mean chars per whitespace-token over a char tail.
///
/// This is the quantity that decides whether the token metric has anything to
/// work with: a 12-token window over 6-char words looks at ~70 chars of
/// content, while the same window over 500-char "words" either does not exist
/// (too few tokens to clear the length guard) or steps over the repetition
/// entirely because each token swallows a whole period of it.
fn chars_per_whitespace_token(tail: &[char]) -> f32 {
    let mut tokens = 0usize;
    let mut in_tok = false;
    for c in tail {
        if c.is_whitespace() {
            in_tok = false;
        } else {
            if !in_tok {
                tokens += 1;
            }
            in_tok = true;
        }
    }
    if tokens == 0 {
        // All whitespace: there is no content to judge, and calling that
        // "blind" would hand an empty slice to the fallback. Not blind.
        return 0.0;
    }
    tail.len() as f32 / tokens as f32
}

/// Distinct-window ratio over the TAIL of one slice, at CHAR granularity.
///
/// The same shape as [`tail_repetition_ratio`] — distinct windows over total
/// windows, sliding by one — so the two ratios are directly comparable and
/// that function's whole line of reasoning carries over. The only difference
/// is the unit: chars instead of whitespace tokens.
///
/// Windows are over `char`s, never bytes. A byte window would straddle code
/// points and make the ratio meaningless for exactly the content this exists
/// to judge — no-space CJK is 3 bytes per char.
///
/// Returns `None` for a tail too short to judge, which the caller must treat
/// as CLEAN.
pub fn tail_char_repetition_ratio(slice: &str, window: usize, tail: usize) -> Option<f32> {
    let t = tail_chars(slice, tail);
    if t.len() < window.saturating_mul(3) {
        return None;
    }
    // Borrowed slices, so the set holds no window CONTENTS — the whole
    // structure is one fat pointer per window.
    let windows: std::collections::HashSet<&[char]> = t.windows(window).collect();
    Some(windows.len() as f32 / (t.len() - window + 1) as f32)
}

/// True when whitespace tokenization cannot resolve this slice's tail, so
/// [`tail_repetition_ratio`]'s verdict on it means nothing either way.
///
/// Measured over the SAME tail region the fallback then judges, not the whole
/// slice: a turn that reasoned in prose and then began emitting a blob is
/// blind where it matters even though its body is not.
pub fn tokenization_is_blind(slice: &str, checkpoint_interval: u32) -> bool {
    let t = tail_chars(slice, tail_sample_chars(checkpoint_interval));
    chars_per_whitespace_token(&t) >= TOKENIZER_BLIND_CHARS_PER_TOKEN
}

/// Is this slice's tail repeating rather than working?
///
/// Two metrics, in order. The whitespace-token metric runs first and
/// UNCHANGED; only if it does not already call the slice degenerate does the
/// (#2228) char fallback get a say, and only where the tokenizer is blind.
/// The composition is deliberately one-way — the fallback can turn a CLEAN
/// into a DEGENERATE and never the reverse — so no verdict this function
/// reached before #2228 has changed, or changed its reason.
///
/// **What this does NOT catch.** Every metric has a gap; these are this
/// one's, measured rather than supposed:
///
/// - **A long invariant unit plus a small varying element.** A 120-char dash
///   run followed by an incrementing index scores 0.1752 and passes. It is
///   statistically indistinguishable from a legitimate URL list whose entries
///   share a 105-char prefix (0.2043) — same shape, and the difference is
///   whether the invariant part carries meaning, which no window ratio can
///   see. Conceded deliberately, in the direction [`DEGENERATE_TAIL_RATIO`]'s
///   asymmetry names: a false CLEAN costs a checkpoint, a false DEGENERATE
///   costs the work. The monotonicity runs the OTHER way from what an
///   earlier version of this line said: SHORTER invariant units score HIGHER
///   and escape MORE. Measured (dash run of length L + incrementing index,
///   tail 96,000, W=24) — L=40 0.4954, L=100 0.2150, L=160 0.1363, L=200
///   0.1097, all clean; the fix reaches this shape only at L>=250 (0.0881,
///   0.0558 at 400). So the conceded gap is not a narrow band around 120
///   chars — it is EVERYTHING below ~250. What keeps the concession sound is
///   that the #2228 shapes are VERBATIM, and verbatim fires at every unit
///   size measured (0.0004 from 43 to 215 chars).
/// - **Legitimate output that really is char-repetitive** — a minified JSON
///   array of genuinely identical records, base64 of a sparse or zero-filled
///   buffer — is called DEGENERATE, correctly by the metric and wrongly by
///   intent. The measured corpora put realistic versions of both well clear
///   (0.6345 and 1.0000), but a pathological one exists and this is where it
///   lands.
/// - **A unit larger than `tail / (1 / DEGENERATE_CHAR_RATIO)`.** The tail
///   holds `tail / unit` copies and the ratio bottoms out at their
///   reciprocal, so a whitespace-free unit above ~8,600 chars per checkpoint
///   cannot be driven under the threshold however many times it repeats.
///   [`TAIL_CHARS_PER_TOKEN`] is sized so this stays ~2x beyond the largest
///   realistic per-interval unit.
/// - **Everything [`tail_repetition_ratio`] already misses**, unchanged:
///   repetition is not lack of progress, and a model paraphrasing itself in a
///   circle scores near 1.000 by either metric.
pub fn slice_is_degenerate(slice: &str, checkpoint_interval: u32) -> bool {
    // The token metric first, unchanged. Any verdict it could reach before
    // #2228 it still reaches, for the same reason.
    if let Some(r) =
        tail_repetition_ratio(slice, TAIL_WINDOW_TOKENS, tail_sample_tokens(checkpoint_interval))
    {
        if r < DEGENERATE_TAIL_RATIO {
            return true;
        }
    }
    // (#2228) The char fallback, engaged ONLY where the tokenizer is blind —
    // so no slice the token metric can actually resolve changes hands.
    if !tokenization_is_blind(slice, checkpoint_interval) {
        return false;
    }
    match tail_char_repetition_ratio(
        slice,
        TAIL_WINDOW_CHARS,
        tail_sample_chars(checkpoint_interval),
    ) {
        // Too short to judge — the asymmetry says keep working.
        None => false,
        Some(r) => r < DEGENERATE_CHAR_RATIO,
    }
}

#[cfg(test)]
mod degeneracy_tests {
    use super::*;

    #[test]
    fn an_exact_loop_is_degenerate() {
        let looping = "I should check the guard. ".repeat(200);
        assert!(slice_is_degenerate(&looping, 1000));
    }

    #[test]
    fn a_two_phrase_cycle_is_degenerate() {
        let looping = "Check the guard. Verify the path. ".repeat(120);
        assert!(slice_is_degenerate(&looping, 1000));
    }

    #[test]
    fn ordinary_varied_reasoning_is_clean() {
        // Distinct sentences — the shape all four REAL samples had.
        let productive: String = (0..300)
            .map(|i| format!("Step {i}: inspect symbol_{i} in module_{i} and note its guard. "))
            .collect();
        assert!(!slice_is_degenerate(&productive, 1000));
    }

    #[test]
    fn a_slice_too_short_to_judge_is_treated_as_clean() {
        // The asymmetry, pinned: uncertainty must never stop a working model.
        assert_eq!(tail_repetition_ratio("only a few words here", 12, 400), None);
        assert!(!slice_is_degenerate("only a few words here", 1000));
    }

    #[test]
    fn the_threshold_sits_far_from_both_measured_clusters() {
        let looping = "same phrase again and again ".repeat(150);
        let productive: String = (0..300)
            .map(|i| format!("Distinct observation number {i} about a different symbol. "))
            .collect();
        let lr = tail_repetition_ratio(&looping, TAIL_WINDOW_TOKENS, tail_sample_tokens(1000)).unwrap();
        let pr = tail_repetition_ratio(&productive, TAIL_WINDOW_TOKENS, tail_sample_tokens(1000)).unwrap();
        assert!(lr < DEGENERATE_TAIL_RATIO, "loop {lr} must be below {DEGENERATE_TAIL_RATIO}");
        assert!(pr > 0.9, "productive {pr} must be near 1.0");
    }
}

#[cfg(test)]
mod checkpoint_gate_sensitivity {
    use super::*;

    /// (#1221) The gate must actually FIRE on verbatim looping — at every
    /// checkpoint interval, not just the one it was tuned at.
    ///
    /// This is the test that was missing. The sample used to be a constant
    /// (4000) beside a 1000-token interval, which put the verbatim floor at
    /// 0.2507 against a 0.25 threshold: a model looping perfectly would
    /// asymptote just ABOVE the line and never trip. Nothing said so, because
    /// nothing measured the floor. Deriving the sample from the interval is
    /// what makes the margin hold at any setting; this pins that it does.
    #[test]
    fn verbatim_looping_fires_at_every_interval() {
        for interval in [250u32, 500, 1000, 2000, 4000] {
            let slice: String = (0..interval).map(|j| format!("v{j} ")).collect();
            // Twenty repeats: far more than the tail can hold, so this is the
            // asymptotic floor, not a transient early value.
            let looping = slice.repeat(20);
            let r = tail_repetition_ratio(
                &looping,
                TAIL_WINDOW_TOKENS,
                tail_sample_tokens(interval),
            )
            .expect("a 20x repeat is long enough to judge");
            assert!(
                r < DEGENERATE_TAIL_RATIO,
                "verbatim looping at interval={interval} scored {r:.4}, which is NOT \
                 below the {DEGENERATE_TAIL_RATIO} threshold — the gate cannot fire on \
                 the exact failure it exists to catch"
            );
            assert!(
                slice_is_degenerate(&looping, interval),
                "interval={interval}: the gate must call verbatim looping degenerate"
            );
        }
    }

    /// The other side: genuinely varied reasoning must NOT be called degenerate,
    /// however long it gets. A gate that stops good work is worse than none.
    #[test]
    fn novel_reasoning_never_fires_however_long() {
        for interval in [250u32, 1000, 4000] {
            // Ten intervals' worth of wholly distinct tokens.
            let novel: String = (0..interval * 10).map(|j| format!("w{j} ")).collect();
            assert!(
                !slice_is_degenerate(&novel, interval),
                "interval={interval}: novel reasoning must never be called degenerate"
            );
        }
    }
}

#[cfg(test)]
mod degeneracy_charlevel_tests {
    use super::*;

    // ── deterministic corpora ─────────────────────────────────────────────
    //
    // Every generator here is seeded and allocation-free of external deps, so
    // the measured table in the docs above is reproducible by running these
    // tests, not by trusting a number someone typed once.

    /// SplitMix64 — a deterministic stand-in for "random bytes a model might
    /// base64 or hex-encode into its answer".
    fn prng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    fn prng_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut next = prng(seed);
        (0..n).map(|_| (next() & 0xFF) as u8).collect()
    }

    fn base64(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
        }
        out
    }

    /// Concatenated 64-char hex digests, the shape a model produces when it
    /// echoes a list of content hashes.
    fn hex_digests(count: usize, seed: u64) -> String {
        let mut next = prng(seed);
        let mut out = String::new();
        for _ in 0..count {
            for _ in 0..8 {
                out.push_str(&format!("{:016x}", next()));
            }
        }
        out
    }

    /// Minified JSON with varying field values — no whitespace anywhere, the
    /// exact shape a model emits when asked for a compact machine-readable
    /// answer.
    fn minified_json(records: usize, seed: u64) -> String {
        let mut next = prng(seed);
        let kinds = ["file", "symbol", "module", "guard", "route", "record"];
        let verbs = ["added", "removed", "renamed", "moved", "inlined", "split"];
        let mut out = String::from("{\"findings\":[");
        for i in 0..records {
            if i > 0 {
                out.push(',');
            }
            let k = kinds[(next() % kinds.len() as u64) as usize];
            let v = verbs[(next() % verbs.len() as u64) as usize];
            out.push_str(&format!(
                "{{\"id\":{i},\"kind\":\"{k}\",\"action\":\"{v}\",\"path\":\"crates/dm-{}/src/{}_{}.rs\",\"line\":{},\"score\":{}}}",
                next() % 97,
                k,
                next() % 9973,
                next() % 4001,
                next() % 1000
            ));
        }
        out.push_str("]}");
        out
    }

    /// A newline-separated URL list: whitespace EXISTS, but every token is a
    /// long string sharing a ~45-char prefix with every other one. The
    /// adversarial case for the char fallback — legitimate content that is
    /// genuinely repetitive at a 16-char scale.
    fn url_list(count: usize) -> String {
        let mut next = prng(0x51_u64);
        let mut out = String::new();
        for i in 0..count {
            out.push_str(&format!(
                "https://github.com/kstrat2001/darkmux/issues/{}#issuecomment-{}\n",
                i,
                next() % 1_000_000_000
            ));
        }
        out
    }

    /// A URL list whose entries share a 105-char prefix and differ only in a
    /// short index — the BINDING false-positive constraint. Legitimate output
    /// that is genuinely repetitive at the char window's own scale.
    fn long_prefix_url_list(count: usize) -> String {
        let mut out = String::new();
        for i in 0..count {
            out.push_str(&format!(
                "https://raw.githubusercontent.com/kstrat2001/darkmux/main/crates/darkmux-crew/src/step_kinds/patterns/{i}.rs\n"
            ));
        }
        out
    }

    /// Minified JSON records that differ ONLY by an index — ~58 chars of
    /// identical boilerplate per record. The second-worst legitimate case.
    fn near_identical_json(count: usize) -> String {
        let mut out = String::from("[");
        for i in 0..count {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"kind\":\"finding\",\"severity\":\"low\",\"confirmed\":true,\"index\":{i}}}"
            ));
        }
        out.push(']');
        out
    }

    /// Genuine Chinese prose — whitespace-free by the language's own
    /// convention, and the reason a naive "no spaces means degenerate" rule
    /// would be a disaster. Seven topically distinct paragraphs so the
    /// vocabulary does not collapse.
    const CJK_PROSE: &str = concat!(
        "秋天的清晨，山谷里升起薄薄的雾气，远处的松林在微光中显出深浅不一的轮廓。溪水沿着石缝流下，声音清脆，像是有人在低声数着日子。牧羊人赶着羊群走过木桥，脚步声惊起一只灰色的水鸟，它掠过水面，向着更高的坡地飞去。",
        "城市另一端的工厂正在换班，机器的轰鸣与广播里的通知混在一起。年轻的工程师站在控制台前，盯着屏幕上跳动的曲线，思考着昨夜那次异常究竟来自传感器的漂移，还是散热系统的设计缺陷。他打开笔记本，把几个可能的原因逐条写下，准备等会儿和同事讨论。",
        "南方的古镇里，老人们仍旧保留着用陶罐煨汤的习惯。他们说，火候要慢，材料要新鲜，急躁的人做不出好味道。集市上卖鱼的摊贩把最肥的一条留给熟客，旁边的孩子踮着脚，望着糖画师傅手里流动的金黄色糖丝，久久不肯离开。",
        "图书馆的顶层很安静，只有翻书的声音。一个学生把三本关于气候变迁的著作摊在桌上，试图理解海洋环流与季风之间的关系。窗外的梧桐落下第一片黄叶，他抬头看了一眼，又低下头继续写笔记，铅笔在纸上留下细细的痕迹。",
        "夜里下起小雨，屋檐滴水，邻居家的猫躲在花盆后面。远处传来火车的汽笛，长长的一声，像是把整条街的睡意都拉长了。第二天醒来，路面湿润，空气里有泥土和桂花混合的气味，孩子们踩着水洼跑向学校，笑声散在巷子的尽头。",
        "他决定在冬天之前去一趟北方，看看结冰的湖面和被雪覆盖的草原。行李很简单：一件厚外套、两本旧书、一台已经用了很多年的相机。列车穿过隧道时，车厢里忽然安静下来，只剩下轮轨规律的节奏，像是某种古老的计时器。",
        "海边的小镇上，造船的师傅正在给一艘木船上漆。他的手掌粗糙，动作却很轻，仿佛在对待一件易碎的乐器。海风带来咸味，也带来远处渔船归航的消息。傍晚时分，霞光把整片海面染成温暖的橙红，几只海鸥落在桅杆上，久久没有飞走。",
    );

    // ── the bug (#2228): the positive cases the gate could never fire on ──

    #[test]
    fn a_whitespace_free_single_char_run_is_degenerate() {
        // The canonical degenerate-decoding shape. split_whitespace() yields
        // ONE token, the len<window*3 guard returns None, and None maps to
        // CLEAN — so before #2228 this ran to the host's 600s SIGKILL.
        let looping = "-".repeat(5000);
        assert!(
            slice_is_degenerate(&looping, 1000),
            "a 5000-char run of a single repeated character must be degenerate"
        );
    }

    #[test]
    fn a_whitespace_free_short_cycle_is_degenerate() {
        let looping = "abc".repeat(2000);
        assert!(
            slice_is_degenerate(&looping, 1000),
            "a whitespace-free 3-char cycle must be degenerate"
        );
    }

    #[test]
    fn a_no_space_cjk_run_is_degenerate() {
        // Multi-byte: the metric must count CHARS, not bytes, or the window
        // straddles code points and the ratio is meaningless.
        let looping = "。".repeat(3000);
        assert!(
            slice_is_degenerate(&looping, 1000),
            "a whitespace-free CJK single-character run must be degenerate"
        );
    }

    #[test]
    fn a_long_whitespace_free_unit_repeated_is_degenerate() {
        // Not a single char and not a short cycle: a whole 400-char
        // whitespace-free unit re-emitted. This is what a model re-generating
        // the same blob every checkpoint actually looks like.
        let unit = base64(&prng_bytes(300, 7));
        let looping = unit.repeat(20);
        assert!(
            slice_is_degenerate(&looping, 1000),
            "a whitespace-free unit repeated 20x must be degenerate"
        );
    }

    #[test]
    fn a_blob_with_a_few_stray_spaces_is_still_judged() {
        // The trigger has to be DENSITY, not "no whitespace at all". Here the
        // whitespace tokens are all DISTINCT (an incrementing index), so the
        // token metric scores 1.000 and calls it CLEAN — while the content
        // between them is a dash run. A few spaces in 5000 chars is still
        // tokenization-blind.
        let mut s = String::new();
        for i in 0..150 {
            s.push_str(&"-".repeat(400));
            s.push_str(&format!(" [{i}] "));
        }
        let token_ratio =
            tail_repetition_ratio(&s, TAIL_WINDOW_TOKENS, tail_sample_tokens(1000)).unwrap();
        assert!(
            token_ratio > 0.9,
            "precondition: the token metric must score this CLEAN ({token_ratio:.4}) — \
             otherwise this test is not exercising the density trigger, and a `None`-only \
             trigger would have been enough"
        );
        assert!(
            tokenization_is_blind(&s, 1000),
            "precondition: the density trigger must call this blind"
        );
        assert!(
            slice_is_degenerate(&s, 1000),
            "a dash run broken up by 150 distinct index tokens is still degenerate"
        );
    }

    // ── the false-positive guards: legitimate whitespace-free content ─────
    //
    // A `true` here is worse than the bug. In the answer region it returns
    // EscalationTriggered(IntraTurnStallExhausted) and hands the dispatch to
    // the frontier; inside a thought it force-closes the model's reasoning.

    #[test]
    fn base64_of_random_bytes_is_clean() {
        let blob = base64(&prng_bytes(6000, 11));
        assert!(
            !slice_is_degenerate(&blob, 1000),
            "an 8000-char base64 payload is legitimate whitespace-free output"
        );
    }

    #[test]
    fn minified_json_is_clean() {
        let doc = minified_json(120, 23);
        assert!(doc.len() > 4000, "precondition: long enough to judge");
        assert!(
            !slice_is_degenerate(&doc, 1000),
            "minified JSON is legitimate whitespace-free output"
        );
    }

    #[test]
    fn concatenated_hex_digests_are_clean() {
        let digests = hex_digests(80, 37);
        assert!(
            !slice_is_degenerate(&digests, 1000),
            "a 5120-char run of hex digests is legitimate whitespace-free output"
        );
    }

    #[test]
    fn real_cjk_prose_is_clean() {
        assert!(
            !slice_is_degenerate(CJK_PROSE, 1000),
            "genuine CJK prose is whitespace-free BY THE LANGUAGE and must stay CLEAN"
        );
    }

    #[test]
    fn a_url_list_is_clean() {
        // Long shared prefixes at exactly the scale the char window looks at.
        let urls = url_list(120);
        assert!(
            !slice_is_degenerate(&urls, 1000),
            "a list of distinct URLs sharing a long prefix must stay CLEAN"
        );
    }

    #[test]
    fn a_long_prefix_url_list_is_clean() {
        // The binding constraint: this is the LOWEST-scoring legitimate
        // corpus measured, and the reason DEGENERATE_CHAR_RATIO is 0.09 and
        // not DEGENERATE_TAIL_RATIO's 0.25 — at 0.25 this escalates.
        let urls = long_prefix_url_list(1200);
        let r = tail_char_repetition_ratio(&urls, TAIL_WINDOW_CHARS, tail_sample_chars(1000))
            .expect("long enough to judge");
        assert!(
            r > DEGENERATE_CHAR_RATIO,
            "the worst legitimate corpus scored {r:.4}, at or below the \
             {DEGENERATE_CHAR_RATIO} threshold — the gate would escalate a real dispatch"
        );
        assert!(!slice_is_degenerate(&urls, 1000));
    }

    #[test]
    fn near_identical_json_records_are_clean() {
        let doc = near_identical_json(1400);
        assert!(!slice_is_degenerate(&doc, 1000));
    }

    /// (#2228) The density trigger, and the honest limit of it.
    ///
    /// Requirement 1 says the fallback engages only where whitespace
    /// tokenization is uninformative. `tokenization_is_blind` is what
    /// enforces that — but this test also pins the measured fact that it
    /// never has to OVERRULE the char metric: on the one family where the two
    /// guards could disagree (a long invariant run broken up by distinct
    /// tokens), every slice the trigger calls sighted is one the char metric
    /// would have passed anyway.
    ///
    /// Swept over dash-run lengths, at the shipped constants:
    ///
    /// | run | chars/token | blind | token ratio | char ratio |
    /// |-----|-------------|-------|-------------|------------|
    /// |  30 |         7.6 | no    |      1.0000 |     0.4717 |
    /// |  60 |        12.6 | no    |      1.0000 |     0.2911 |
    /// | 120 |        22.5 | no    |      1.0000 |     0.1650 |
    /// | 200 |        35.8 | YES   |      1.0000 |     0.1041 |
    /// | 250 |        44.1 | YES   |      1.0000 |     0.0847 |
    /// | 400 |        69.0 | YES   |      1.0000 |     0.0546 |
    ///
    /// The char metric first crosses 0.09 between runs of 200 and 250 — by
    /// which point the trigger has long since said "blind". So an
    /// always-blind mutation would change NO verdict here, with ~1.3x of
    /// headroom at the crossing. That is reported rather than hidden: the
    /// trigger is a structural conformance guard, not an active filter, and
    /// this test is what will notice if that ever stops being true.
    #[test]
    fn the_density_trigger_never_has_to_overrule_the_char_metric() {
        for run in [30usize, 60, 120, 200, 250, 400] {
            let mut v = String::new();
            for i in 0..(120000 / (run + 12)) {
                v.push_str(&format!("| {} | row {i} |\n", "-".repeat(run)));
            }
            let ch = tail_char_repetition_ratio(&v, TAIL_WINDOW_CHARS, tail_sample_chars(1000))
                .expect("long enough to judge");
            if !tokenization_is_blind(&v, 1000) {
                assert!(
                    ch > DEGENERATE_CHAR_RATIO,
                    "run={run}: the trigger called this sighted while the char metric \
                     scored {ch:.4}, below the {DEGENERATE_CHAR_RATIO} threshold — the two \
                     guards now disagree, so the trigger has become load-bearing on its \
                     own and its value needs re-deriving"
                );
            }
        }
    }

    /// (#2228) The density trigger is LOAD-BEARING, not decorative — this is
    /// the test the constant's doc points at, and it exists because an
    /// earlier version of that doc claimed deleting the trigger would be
    /// "behaviorally silent".
    ///
    /// The class it saves: a long invariant unit that carries its OWN
    /// whitespace, so the slice is char-repetitive AND low-density at once.
    /// Markup and minified CSS are exactly that. Each corpus here is SIGHTED
    /// by a wide margin, so the token metric judges it and calls it clean —
    /// while the char metric ALONE would score it under the threshold and
    /// escalate a legitimate answer.
    ///
    /// Note what this asserts and why: `slice_is_degenerate` must be false
    /// (the guard holds), AND the raw char ratio must be BELOW the threshold
    /// (so the guard is the only reason it holds). Without that second
    /// assertion the test would still pass with the guard removed, for the
    /// wrong reason.
    #[test]
    fn the_density_trigger_is_load_bearing_on_whitespace_carrying_units() {
        // A space every ~26 chars keeps density well under the trigger while
        // the repeated prefix drives char-window repetition down.
        // The protection zone is bounded on BOTH sides, which is why these
        // three values and not a wider sweep. Below ~240 the char metric does
        // not condemn (0.1013 at 200), so there is nothing to save. Above
        // ~340 the TOKEN metric condemns on its own (0.2226 at 360, under its
        // own 0.25) and the verdict is unchanged with or without the guard.
        // A test using 360 would pass with the guard removed — for the wrong
        // reason.
        for invariant in [240usize, 260, 300] {
            let mut prefix = String::new();
            while prefix.len() < invariant {
                prefix.push_str("<td class=\"cell pad wide\"> ");
            }
            prefix.truncate(invariant);
            let mut v = String::new();
            for i in 0..(140_000 / (invariant + 16)) {
                v.push_str(&format!("<tr>{prefix}<td>{i}</td></tr>\n"));
            }

            assert!(
                !tokenization_is_blind(&v, 1000),
                "invariant={invariant}: this corpus must be SIGHTED — if the trigger \
                 ever starts calling markup blind, the guard stops protecting it and \
                 this test is no longer testing anything"
            );

            let ch = tail_char_repetition_ratio(&v, TAIL_WINDOW_CHARS, tail_sample_chars(1000))
                .expect("long enough to judge");
            assert!(
                ch < DEGENERATE_CHAR_RATIO,
                "invariant={invariant}: scored {ch:.4}, at or above the \
                 {DEGENERATE_CHAR_RATIO} threshold — the char metric no longer \
                 condemns this shape, so the guard is no longer what saves it and \
                 this test has gone vacuous"
            );

            assert!(
                !slice_is_degenerate(&v, 1000),
                "invariant={invariant}: legitimate markup (char ratio {ch:.4}) was \
                 called DEGENERATE — the density trigger is what keeps this clean, \
                 and removing it escalates a working model's answer"
            );
        }
    }

    /// The trigger's two sides, pinned where they are far apart: content the
    /// whitespace tokenizer resolves must never reach the fallback, and
    /// whitespace-free content must always reach it.
    #[test]
    fn the_density_trigger_splits_prose_from_blobs() {
        let prose: String = (0..900)
            .map(|i| format!("Step {i}: inspect symbol_{i} in module_{i} and note its guard. "))
            .collect();
        let code: String = (0..400)
            .map(|i| {
                format!("    fn check_{i}(&self) -> bool {{\n        self.slot_{i}.is_some()\n    }}\n")
            })
            .collect();
        for (name, s) in [("prose", &prose), ("source-shaped", &code)] {
            assert!(
                !tokenization_is_blind(s, 1000),
                "{name} must stay on the unchanged token path"
            );
        }
        for (name, s) in [
            ("dash run", "-".repeat(5000)),
            ("base64", base64(&prng_bytes(6000, 11))),
            ("minified JSON", minified_json(120, 23)),
            ("CJK prose", CJK_PROSE.to_string()),
        ] {
            assert!(
                tokenization_is_blind(&s, 1000),
                "{name} is whitespace-free and must reach the fallback"
            );
        }
    }

    #[test]
    fn ordinary_english_prose_takes_the_unchanged_token_path() {
        // Requirement 1 pinned: the fallback must not engage on anything the
        // whitespace tokenizer can already resolve.
        let productive: String = (0..300)
            .map(|i| format!("Step {i}: inspect symbol_{i} in module_{i} and note its guard. "))
            .collect();
        assert!(!slice_is_degenerate(&productive, 1000));
    }

    /// (#2228) The measured separation, pinned. The doc tables above are only
    /// as good as this test: it re-derives both clusters from the same
    /// corpora and asserts the threshold still sits between them with margin.
    ///
    /// A change that narrows either margin fails here rather than in a
    /// dispatch, which is the whole point of writing the numbers down.
    #[test]
    fn the_char_threshold_sits_between_both_measured_clusters() {
        let tail = tail_sample_chars(1000);
        let r = |s: &str| {
            tail_char_repetition_ratio(s, TAIL_WINDOW_CHARS, tail).expect("long enough to judge")
        };
        let unit = base64(&prng_bytes(300, 7));
        let mut stray = String::new();
        for i in 0..150 {
            stray.push_str(&"-".repeat(400));
            stray.push_str(&format!(" [{i}] "));
        }
        let degenerate = [
            ("single-char run", r(&"-".repeat(64000))),
            ("short cycle", r(&"abc".repeat(2000))),
            ("no-space CJK run", r(&"。".repeat(3000))),
            ("400-char b64 unit x20", r(&unit.repeat(20))),
            ("400-char dash run + index", r(&stray)),
        ];
        let clean = [
            ("105-char-prefix URL list", r(&long_prefix_url_list(1200))),
            ("near-identical JSON", r(&near_identical_json(1400))),
            ("minified JSON", r(&minified_json(1500, 107))),
            ("URL list", r(&url_list(1000))),
            ("base64 of random bytes", r(&base64(&prng_bytes(48000, 101)))),
            ("hex digests", r(&hex_digests(1000, 103))),
            ("real CJK prose", r(CJK_PROSE)),
        ];
        let worst_degenerate = degenerate.iter().map(|(_, v)| *v).fold(0.0f32, f32::max);
        let worst_clean = clean.iter().map(|(_, v)| *v).fold(f32::MAX, f32::min);
        for (name, v) in &degenerate {
            assert!(
                *v < DEGENERATE_CHAR_RATIO,
                "{name} scored {v:.4}, NOT below the {DEGENERATE_CHAR_RATIO} threshold — \
                 the gate cannot fire on a shape it exists to catch"
            );
        }
        for (name, v) in &clean {
            assert!(
                *v > DEGENERATE_CHAR_RATIO,
                "{name} scored {v:.4}, at or below the {DEGENERATE_CHAR_RATIO} threshold — \
                 the gate would escalate legitimate whitespace-free output"
            );
        }
        // The margins the doc tables claim, asserted rather than remembered.
        assert!(
            DEGENERATE_CHAR_RATIO / worst_degenerate > 1.5,
            "degenerate-side margin collapsed: worst degenerate {worst_degenerate:.4}"
        );
        assert!(
            worst_clean / DEGENERATE_CHAR_RATIO > 2.0,
            "clean-side margin collapsed: worst clean {worst_clean:.4}"
        );
    }

    /// The gap named on `slice_is_degenerate`, pinned so it is a DECISION and
    /// not an accident someone later "fixes" by moving the threshold.
    ///
    /// A 120-char invariant run plus a varying index (0.1752) and a
    /// legitimate 105-char-prefix URL list (0.2043) are the same statistical
    /// shape. Both are CLEAN, on purpose: no threshold separates them, and
    /// the asymmetry says err toward letting the model work.
    #[test]
    fn the_conceded_overlap_stays_clean_on_both_sides() {
        let mut stray = String::new();
        for i in 0..500 {
            stray.push_str(&"-".repeat(120));
            stray.push_str(&format!(" [{i}] "));
        }
        let urls = long_prefix_url_list(1200);
        let tail = tail_sample_chars(1000);
        let deg = tail_char_repetition_ratio(&stray, TAIL_WINDOW_CHARS, tail).unwrap();
        let leg = tail_char_repetition_ratio(&urls, TAIL_WINDOW_CHARS, tail).unwrap();
        assert!(
            deg < leg,
            "the conceded pair must stay ordered as measured: degenerate-shaped \
             {deg:.4} below legitimate {leg:.4}"
        );
        assert!(
            leg / deg < 1.5,
            "these two are supposed to be INSEPARABLE ({deg:.4} vs {leg:.4}); if they \
             have pulled apart, the concession can be revisited"
        );
        assert!(!slice_is_degenerate(&stray, 1000));
        assert!(!slice_is_degenerate(&urls, 1000));
    }
}
