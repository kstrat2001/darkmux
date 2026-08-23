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

pub fn slice_is_degenerate(slice: &str, checkpoint_interval: u32) -> bool {
    match tail_repetition_ratio(slice, TAIL_WINDOW_TOKENS, tail_sample_tokens(checkpoint_interval)) {
        // Too short to judge — the asymmetry says keep working.
        None => false,
        Some(r) => r < DEGENERATE_TAIL_RATIO,
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
