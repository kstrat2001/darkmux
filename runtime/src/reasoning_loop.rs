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
