//! The generalized "pass 1 → conditional confirmation passes → demote on
//! disagreement" control-flow shape (#1352 Tier 2).
//!
//! Extracted from the PR-review pipeline's judge stage
//! (`darkmux-lab`'s `review.rs::judge_one_flag_with_passes`), which was
//! already generic over the *pass count* (#1266) but had its control flow
//! hand-written inline. The judge's use of this shape is unchanged in
//! result — `review.rs` now plugs its own dispatch call and ruling
//! classification into [`multi_pass_confirm`] rather than re-implementing
//! the loop — but the shape itself has no review-specific knowledge (no
//! `ProbeFlag`, no `JudgeRuling`, no token/budget accounting): it is
//! generic over the caller's own pass-result type `R`.

/// How one pass's result classifies against the confirm/demote decision —
/// the caller-supplied predicate [`multi_pass_confirm`] drives its control
/// flow with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassClass {
    /// This pass affirms the finding. Pass 1 confirming continues on to a
    /// confirmation pass (when `passes > 1`); a later pass confirming
    /// continues the unanimous-consensus loop.
    Confirm,
    /// This pass's outcome doesn't affirm, but isn't a hard reject either
    /// — only meaningful for pass 1 (a review judge's `needs_check`
    /// ruling); a non-pass-1 pass with this class is treated the same as
    /// [`PassClass::Reject`] by the confirmation loop (see the function
    /// doc — both simply aren't [`PassClass::Confirm`]).
    NeedsCheck,
    /// This pass's outcome fully rejects the finding — again only
    /// meaningful for pass 1's own tier decision (see
    /// [`MultiPassResult::tier`]'s doc).
    Reject,
}

/// The overall outcome tier — pass 1's classification when it isn't
/// [`PassClass::Confirm`], or the confirmation loop's outcome when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmTier {
    Confirmed,
    NeedsCheck,
    Rejected,
}

/// [`multi_pass_confirm`]'s full result: which tier the run landed on, every
/// pass's raw result (so the caller can fold its own metrics — tokens, wall
/// time, dispatch-error flags, whatever — however it needs to), and whether
/// a confirmation pass (as opposed to pass 1 itself) is what caused a
/// demotion.
pub struct MultiPassResult<R> {
    pub tier: ConfirmTier,
    /// `true` iff pass 1 confirmed but a LATER confirmation pass didn't —
    /// `false` when pass 1 itself decided the tier (confirmed outright at
    /// `passes: 1`, or non-confirmed at pass 1 with no confirmation pass
    /// ever attempted).
    pub demoted_by_later_pass: bool,
    pub pass1: R,
    /// Every confirmation pass (pass numbers `2..=passes`) that actually
    /// ran, in order — empty when `passes == 1` or pass 1 didn't confirm.
    /// The unanimous-consensus loop early-exits on the first non-confirm,
    /// so this is never longer than necessary to reach a verdict; the LAST
    /// entry is the decisive one (the pass that broke unanimity, or the
    /// final confirm).
    pub confirmation_passes: Vec<R>,
}

impl<R> MultiPassResult<R> {
    /// The decisive later-pass result, if any — `confirmation_passes.last()`
    /// under a friendlier name for callers that only care about the LAST
    /// confirmation pass (e.g. the review judge's `pass2` slot — see its own
    /// doc for why "pass2" names the LAST pass, not literally pass number 2,
    /// once `passes > 2`).
    pub fn decisive_later_pass(&self) -> Option<&R> {
        self.confirmation_passes.last()
    }
}

/// Run the multi-pass-confirm control flow: pass 1 ALWAYS runs
/// (`run_pass(1)`). If `classify(&pass1)` isn't [`PassClass::Confirm`], the
/// run stops there — [`ConfirmTier::NeedsCheck`] or
/// [`ConfirmTier::Rejected`] per pass 1's own classification, no
/// confirmation pass ever attempted (a non-confirmed pass 1 earns no
/// further call regardless of `passes`).
///
/// Otherwise, when `passes > 1`, confirmation passes `2..=passes` run in
/// sequence (UNANIMOUS consensus): the FIRST pass whose classification
/// isn't [`PassClass::Confirm`] demotes the result to
/// [`ConfirmTier::NeedsCheck`] and stops (early-exit — `N` configured
/// passes never costs `N×` on a disagreement; only as many passes as it
/// takes to find one land). Every pass confirming →
/// [`ConfirmTier::Confirmed`].
///
/// `run_pass(pass_no)` performs one pass's own work (a dispatch, or
/// whatever the caller's pass IS) and returns its raw result `R`;
/// `classify` maps that result to a [`PassClass`]. Aggregating
/// domain-specific bookkeeping (tokens spent, wall-clock, dispatch-error
/// flags, served-model identity, …) across passes is the CALLER's job —
/// this function returns every pass's raw result via
/// [`MultiPassResult::pass1`]/[`MultiPassResult::confirmation_passes`] so
/// the caller folds its own metrics however it needs; this function is the
/// control-flow SHAPE only, with zero opinion on what a pass actually
/// costs.
pub fn multi_pass_confirm<R>(
    passes: u32,
    mut run_pass: impl FnMut(u32) -> R,
    classify: impl Fn(&R) -> PassClass,
) -> MultiPassResult<R> {
    // A caller-constructed `0` must never skip pass 1 — same defensive
    // clamp the pre-extraction judge control flow used.
    let passes = passes.max(1);

    let pass1 = run_pass(1);
    match classify(&pass1) {
        PassClass::NeedsCheck => {
            return MultiPassResult {
                tier: ConfirmTier::NeedsCheck,
                demoted_by_later_pass: false,
                pass1,
                confirmation_passes: Vec::new(),
            };
        }
        PassClass::Reject => {
            return MultiPassResult {
                tier: ConfirmTier::Rejected,
                demoted_by_later_pass: false,
                pass1,
                confirmation_passes: Vec::new(),
            };
        }
        PassClass::Confirm => {}
    }

    if passes == 1 {
        return MultiPassResult {
            tier: ConfirmTier::Confirmed,
            demoted_by_later_pass: false,
            pass1,
            confirmation_passes: Vec::new(),
        };
    }

    let mut confirmation_passes = Vec::new();
    let mut demoted = false;
    for pass_no in 2..=passes {
        let r = run_pass(pass_no);
        let confirmed = classify(&r) == PassClass::Confirm;
        confirmation_passes.push(r);
        if !confirmed {
            demoted = true;
            break;
        }
    }
    let tier = if demoted { ConfirmTier::NeedsCheck } else { ConfirmTier::Confirmed };
    MultiPassResult { tier, demoted_by_later_pass: demoted, pass1, confirmation_passes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StubPass {
        pass_no: u32,
        class: PassClass,
    }

    fn run_scripted(script: Vec<PassClass>) -> impl FnMut(u32) -> StubPass {
        let mut script = script.into_iter();
        move |pass_no| StubPass { pass_no, class: script.next().expect("script exhausted") }
    }

    fn classify(p: &StubPass) -> PassClass {
        p.class
    }

    #[test]
    fn pass1_confirm_then_confirm_is_confirmed() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::Confirm, PassClass::Confirm]);
        let result = multi_pass_confirm(
            2,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::Confirmed);
        assert!(!result.demoted_by_later_pass);
        assert_eq!(calls, 2);
        assert_eq!(result.decisive_later_pass().unwrap().pass_no, 2);
    }

    #[test]
    fn pass1_confirm_then_reject_demotes() {
        let mut run_pass = run_scripted(vec![PassClass::Confirm, PassClass::Reject]);
        let result = multi_pass_confirm(2, &mut run_pass, classify);
        assert_eq!(result.tier, ConfirmTier::NeedsCheck);
        assert!(result.demoted_by_later_pass);
    }

    #[test]
    fn pass1_needs_check_skips_confirmation_passes() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::NeedsCheck]);
        let result = multi_pass_confirm(
            2,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::NeedsCheck);
        assert!(!result.demoted_by_later_pass);
        assert!(result.confirmation_passes.is_empty());
        assert_eq!(calls, 1, "a non-confirmed pass 1 earns no further call");
    }

    #[test]
    fn pass1_reject_archives_directly() {
        let mut run_pass = run_scripted(vec![PassClass::Reject]);
        let result = multi_pass_confirm(2, &mut run_pass, classify);
        assert_eq!(result.tier, ConfirmTier::Rejected);
        assert!(result.confirmation_passes.is_empty());
    }

    #[test]
    fn passes_one_confirm_is_confirmed_with_a_single_call() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::Confirm]);
        let result = multi_pass_confirm(
            1,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::Confirmed);
        assert!(result.confirmation_passes.is_empty());
        assert_eq!(calls, 1);
    }

    #[test]
    fn unanimous_consensus_all_confirm_runs_every_pass() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::Confirm, PassClass::Confirm, PassClass::Confirm]);
        let result = multi_pass_confirm(
            3,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::Confirmed);
        assert!(!result.demoted_by_later_pass);
        assert_eq!(calls, 3);
        assert_eq!(result.decisive_later_pass().unwrap().pass_no, 3);
    }

    #[test]
    fn unanimous_consensus_early_exit_on_first_disagreement() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::Confirm, PassClass::Reject, PassClass::Confirm]);
        let result = multi_pass_confirm(
            3,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::NeedsCheck);
        assert!(result.demoted_by_later_pass);
        assert_eq!(calls, 2, "unanimity breaks at pass 2 — pass 3 never runs");
    }

    #[test]
    fn zero_passes_is_clamped_to_at_least_one() {
        let mut calls = 0u32;
        let mut run_pass = run_scripted(vec![PassClass::Confirm]);
        let result = multi_pass_confirm(
            0,
            |n| {
                calls += 1;
                run_pass(n)
            },
            classify,
        );
        assert_eq!(result.tier, ConfirmTier::Confirmed);
        assert_eq!(calls, 1);
    }
}
