//! `DispatchBudget`: one dispatch's token cap, `endpoints.<id>.limits.
//! tokens_per_dispatch` (#3035). A dispatch is one role execution; the cap
//! applies at ANY endpoint, managed or not, and is metered on the calls that
//! dispatch makes there. (Before 5.0 this was a per-STEP cap,
//! `remote.max_tokens_per_step`, shared across a `dispatch.map` step's items
//! by `bucket_group`; both are gone. A whole-run budget is the endpoint's
//! rolling `limits.window`.)
//!
//! **The cap is the operator's, and reaching it never stops the dispatch.**
//! There is no built-in number: no cap is no counting. When the operator
//! sets one, the endpoint's `limits.policy` decides what reaching it does:
//!
//! - `off`: nothing is counted.
//! - `warn` (absent = `warn` once a cap is set) and `wait`: every call is
//!   admitted, and the first time the dispatch's spend reaches its cap
//!   [`DispatchBudget::take_breach`] hands the caller one breach to surface
//!   (`crate::budget::settle_dispatch` prints and records it). `wait` has no
//!   meaning for a dispatch's own spend (it never expires, so nothing would
//!   free room); it governs the endpoint's rolling `window` only, and the
//!   cap beside it warns.
//!
//! Before 4.0 this bucket had a 500000 default, SKIPPED the calls a spent
//! step would have made (a named envelope reason), and clamped each call's
//! `max_tokens` to what was left, denying a grant below a per-caller floor
//! (#1610). All three are gone: skipping and clamping both stopped work the
//! operator never asked to stop, and with no clamp there is no starved grant
//! for a floor to deny.
//!
//! **The ceiling is SOFT, by construction.** A call's cost is known only
//! AFTER it, so a dispatch can overshoot by whatever the calls in flight at
//! the crossing spend. A breach is decided, and reported, on SETTLED spend
//! only (review C-c): a sibling's call still in flight has spent nothing
//! yet.

use darkmux_types::{BudgetPolicy, ModelEndpoint};

/// A dispatch whose spend has reached its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchBreach {
    pub used: u64,
    pub budget: u64,
}

/// One dispatch's bucket. See the module doc.
#[derive(Debug)]
pub struct DispatchBudget {
    budget: Option<u64>,
    policy: BudgetPolicy,
    /// What settled calls actually spent (no reservations).
    settled: u64,
    /// The breach has been handed out ([`Self::take_breach`] fires once).
    surfaced: bool,
}

impl DispatchBudget {
    /// A bucket with this cap and policy. A cap of `0` is no cap (CLAUDE.md:
    /// a `0` on a darkmux bound means unbounded, never "instantly").
    pub fn new(budget: Option<u64>, policy: BudgetPolicy) -> Self {
        Self { budget: budget.filter(|n| *n > 0), policy, settled: 0, surfaced: false }
    }

    /// The bucket `ep`'s `limits` describe: its `tokens_per_dispatch` under
    /// its `limits.policy`, managed endpoint or not. No limits, no cap, or
    /// policy `off` is a bucket that counts nothing. `Err` when the limits
    /// cannot be used as written (a typo must not silently disarm a cap;
    /// preflight refuses it first, this keeps a caller that skipped
    /// preflight from running a typo'd cap as no cap).
    pub fn for_endpoint(ep: &ModelEndpoint) -> Result<Self, String> {
        let Some(limits) = crate::budget::checked_limits(ep)? else { return Ok(Self::new(None, BudgetPolicy::Off)) };
        let policy = limits.resolved_policy().map_err(|raw| format!("budget policy `{raw}` is not valid"))?;
        Ok(Self::new(limits.dispatch_cap(), policy))
    }

    /// True when this bucket counts anything: a cap is set and the policy is
    /// not `off`.
    pub fn counts(&self) -> bool {
        self.budget.is_some() && self.policy.counts()
    }

    /// The per-dispatch cap, when one is set.
    pub fn budget(&self) -> Option<u64> {
        self.budget
    }

    /// Tokens settled calls actually spent.
    pub fn settled(&self) -> u64 {
        self.settled
    }

    /// True when a counting bucket's SETTLED spend has reached its cap.
    pub fn exhausted(&self) -> bool {
        self.counts() && self.budget.is_some_and(|b| self.settled >= b)
    }

    /// Settle one call's spend (see `crate::budget::conservative_spend` for
    /// what a call that reports no usage is charged).
    pub fn settle(&mut self, actual: u64) {
        self.settled = self.settled.saturating_add(actual);
    }

    /// The breach to surface, ONCE per bucket: the first time a counting
    /// bucket's spend has reached its cap. `None` otherwise.
    pub fn take_breach(&mut self) -> Option<DispatchBreach> {
        if self.surfaced || !self.exhausted() {
            return None;
        }
        self.surfaced = true;
        Some(DispatchBreach { used: self.settled, budget: self.budget.unwrap_or(0) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No cap: nothing counts and nothing is surfaced, however much is spent.
    #[test]
    fn no_cap_counts_nothing() {
        for policy in [BudgetPolicy::Off, BudgetPolicy::Warn, BudgetPolicy::Wait] {
            let mut b = DispatchBudget::new(None, policy);
            for _ in 0..5 {
                b.settle(9_000_000);
            }
            assert!(!b.counts() && !b.exhausted());
            assert_eq!(b.take_breach(), None);
        }
    }

    /// `off` with a cap set: still nothing counts.
    #[test]
    fn off_never_breaches_even_past_the_cap() {
        let mut b = DispatchBudget::new(Some(100), BudgetPolicy::Off);
        b.settle(1_000);
        assert_eq!(b.take_breach(), None);
    }

    /// `warn` and `wait` (the cap has nothing to wait for, so it warns): the
    /// breach is surfaced exactly once, and no call is ever clamped.
    #[test]
    fn warn_surfaces_the_breach_once_and_never_clamps() {
        for policy in [BudgetPolicy::Warn, BudgetPolicy::Wait] {
            let mut b = DispatchBudget::new(Some(1_000), policy);
            b.settle(1_200);
            assert_eq!(b.take_breach(), Some(DispatchBreach { used: 1_200, budget: 1_000 }));
            assert_eq!(b.take_breach(), None, "surfaced once");
            for _ in 0..3 {
                b.settle(10);
            }
            assert_eq!(b.settled(), 1_230);
        }
    }

    /// (review C-c) A breach is decided on SETTLED spend: calls in flight
    /// (reserved, not settled) never count, so the message never reports
    /// "12788 of 10000" when only 500 was spent.
    #[test]
    fn a_breach_counts_settled_spend() {
        let mut b = DispatchBudget::new(Some(10_000), BudgetPolicy::Warn);
        b.settle(500);
        assert_eq!(b.settled(), 500);
        assert!(!b.exhausted(), "500 spent of 10000");
        assert_eq!(b.take_breach(), None, "500 settled of 10000: no breach");
        b.settle(9_600);
        assert_eq!(b.take_breach(), Some(DispatchBreach { used: 10_100, budget: 10_000 }));
    }

    /// (zero doctrine) A cap of 0 is NO cap: nothing is counted, nothing is
    /// surfaced. (Pre-4.0, 0 was a hard refusal.)
    #[test]
    fn a_zero_cap_is_no_cap() {
        let mut n = DispatchBudget::new(Some(0), BudgetPolicy::Warn);
        n.settle(5_000);
        assert!(!n.counts() && !n.exhausted());
        assert_eq!(n.take_breach(), None);
        assert_eq!(n.budget(), None);
    }

    /// The bucket an endpoint's limits describe, managed or not: its
    /// `tokens_per_dispatch` under its policy (absent: `warn`), and nothing
    /// counted for an endpoint with no cap.
    #[test]
    fn the_bucket_comes_from_the_endpoints_limits_on_either_kind() {
        let ep = |json: &str| -> ModelEndpoint { serde_json::from_str(json).unwrap() };
        for json in [
            r#"{"url":"https://h/v1","limits":{"tokens_per_dispatch":500}}"#,
            r#"{"managed":"lmstudio","limits":{"tokens_per_dispatch":500}}"#,
        ] {
            let b = DispatchBudget::for_endpoint(&ep(json)).unwrap();
            assert_eq!(b.budget(), Some(500), "{json}");
            assert!(b.counts(), "absent policy is warn once a cap is set: {json}");
        }
        let off = DispatchBudget::for_endpoint(&ep(r#"{"url":"https://h/v1","limits":{"tokens_per_dispatch":500,"policy":"off"}}"#)).unwrap();
        assert!(!off.counts());
        let none = DispatchBudget::for_endpoint(&ep(r#"{"url":"https://h/v1"}"#)).unwrap();
        assert_eq!(none.budget(), None);
        let typo = ep(r#"{"url":"https://h/v1","limits":{"tokens_per_dispatch":"500k"}}"#);
        assert!(DispatchBudget::for_endpoint(&typo).is_err(), "an unreadable cap is refused, never read as no cap");
    }

    /// Concurrent calls settling into one bucket: every call's spend is accounted.
    #[test]
    fn concurrent_settles_account_every_call() {
        use std::sync::{Arc, Mutex};
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(Some(1_000), BudgetPolicy::Warn)));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let bucket = Arc::clone(&bucket);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        bucket.lock().unwrap().settle(7);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(bucket.lock().unwrap().settled(), 16 * 20 * 7);
    }
}
