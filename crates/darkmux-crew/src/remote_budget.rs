//! `RemoteBudget`: one step's hosted-token bucket, metering the per-step
//! cap on hosted tokens (`remote.max_tokens_per_step`; `dispatch.map` steps
//! naming the same `bucket_group` share one bucket, and a bare hosted
//! `dispatch` is one step). Local calls never touch it.
//!
//! **(#2902 step 5) The cap is the operator's, and reaching it never stops
//! the step.** There is no built-in number: `None` is no cap, and nothing is
//! counted. When the operator sets one, `remote.step_budget_policy` decides
//! what reaching it does, and it has two values (operator, 2026-09-27):
//!
//! - `off`: nothing is counted.
//! - `warn` (absent = `warn`): every call is admitted, and the first time
//!   the step's spend reaches its cap [`RemoteBudget::take_breach`] hands
//!   the caller one breach to surface (`crate::budget::settle_step` prints
//!   and records it).
//!
//! There is no `wait` for a step: a step has no rolling window, so nothing
//! would ever free room. `wait` is an ENDPOINT budget's value only, and
//! `remote.step_budget_policy: wait` is refused at preflight.
//!
//! Before 4.0 this bucket had a 500000 default, SKIPPED the calls a spent
//! step would have made (a named envelope reason), and clamped each call's
//! `max_tokens` to what was left, denying a grant below a per-caller floor
//! (#1610). All three are gone: skipping and clamping both stopped work the
//! operator never asked to stop, and with no clamp there is no starved grant
//! for a floor to deny.
//!
//! **The ceiling is SOFT, by construction.** A call's cost is known only
//! AFTER it, so a step can overshoot by whatever the calls in flight at the
//! crossing spend. A breach is decided, and reported, on SETTLED spend only
//! (review C-c): a sibling's call still in flight has spent nothing yet. The
//! in-flight reservations are tracked beside it ([`RemoteBudget::used`]).

use darkmux_types::config::StepBudgetPolicy;
use serde::{Deserialize, Serialize};

/// (#1260) One bucket's outcome row, as it lands in a mission's envelope
/// (e.g. `darkmux-lab`'s `ReviewEnvelope::remote_budgets`). Its `stage`
/// field is the envelope's wire name for the bucket's label (the retired
/// review pipeline labeled its buckets `probe`, `judge-pass1`, ...), kept
/// so recorded envelopes still read; the bucket is a per-step cap. The same
/// rule (CLAUDE.md contract 8: the wire keeps its historical spelling, the
/// vocabulary says "step") keeps the hosted `dispatch.single_shot` step
/// result's `remote_max_tokens_per_execution` key.
// (#2310 P2) `PartialEq` is new — every field is a plain String/u64/bool/
// u32, so this rides inside a typed `Output<T>` body (which derives
// `PartialEq` throughout — see `darkmux_crew::step_output`'s module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteBudgetRecord {
    /// The bucket's label (wire name kept), e.g. a step id.
    pub stage: String,
    pub max_tokens: u64,
    /// What the bucket's settled calls spent.
    pub used_tokens: u64,
    pub exhausted: bool,
    /// Remote calls NOT made because the bucket had already exhausted. A
    /// pre-4.0 envelope can carry a non-zero count; since #2902 step 5 no
    /// call is ever skipped, so a new row always says 0. Kept for the shape.
    pub skipped_calls: u32,
}

/// A step whose spend has reached its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepBreach {
    pub used: u64,
    pub budget: u64,
}

/// One step's bucket. See the module doc.
#[derive(Debug)]
pub struct RemoteBudget {
    label: Option<&'static str>,
    budget: Option<u64>,
    policy: StepBudgetPolicy,
    used: u64,
    /// What settled calls actually spent (no reservations).
    settled: u64,
    calls: u32,
    /// The breach has been handed out ([`Self::take_breach`] fires once).
    surfaced: bool,
}

impl RemoteBudget {
    /// A bucket with no label ([`Self::record`] returns `None`). A cap of
    /// `0` is no cap (CLAUDE.md: a `0` on a darkmux bound means unbounded,
    /// never "instantly").
    pub fn new(budget: Option<u64>, policy: StepBudgetPolicy) -> Self {
        let budget = budget.filter(|n| *n > 0);
        Self { label: None, budget, policy, used: 0, settled: 0, calls: 0, surfaced: false }
    }

    /// A bucket that reports its label on [`Self::record`].
    pub fn labeled(label: &'static str, budget: Option<u64>, policy: StepBudgetPolicy) -> Self {
        Self { label: Some(label), ..Self::new(budget, policy) }
    }

    /// The bucket the configuration describes: `explicit` (a launcher's
    /// `bucket_budget` stamped into the step's own config) else
    /// `remote.max_tokens_per_step` (no default), under
    /// `remote.step_budget_policy`. An unregistered policy is an error,
    /// never a fallback (preflight refuses it before any run starts).
    pub fn from_config(explicit: Option<u64>) -> Result<Self, darkmux_types::config_enum::BadEnumValue> {
        let policy = darkmux_types::config_access::remote_step_budget_policy()?;
        Ok(Self::new(explicit.or_else(darkmux_types::config_access::remote_max_tokens_per_step), policy))
    }

    /// True when this bucket counts anything: a budget is set and the
    /// policy is not `off`.
    pub fn counts(&self) -> bool {
        self.budget.is_some() && self.policy == StepBudgetPolicy::Warn
    }

    /// The per-step cap, when one is set.
    pub fn budget(&self) -> Option<u64> {
        self.budget
    }

    pub fn policy(&self) -> StepBudgetPolicy {
        self.policy
    }

    /// Tokens spent AND reserved for calls in flight.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Tokens settled calls actually spent.
    pub fn settled(&self) -> u64 {
        self.settled
    }

    /// True when a counting bucket's SETTLED spend has reached its cap.
    pub fn exhausted(&self) -> bool {
        self.counts() && self.budget.is_some_and(|b| self.settled >= b)
    }

    /// Admit one call and reserve `requested` (the call's completion cap).
    /// Never clamps, never refuses, never holds.
    pub fn admit_reserve(&mut self, requested: u32) {
        self.used = self.used.saturating_add(u64::from(requested));
    }

    /// Replace a reservation made by [`Self::admit_reserve`] with the call's
    /// ACTUAL spend (its reported usage, or the reserved cap when the
    /// endpoint reported none). `calls` is the dispatch attempts it
    /// represents.
    pub fn settle(&mut self, reserved: u32, actual: u64, calls: u32) {
        self.used = self.used.saturating_sub(u64::from(reserved)).saturating_add(actual);
        self.settled = self.settled.saturating_add(actual);
        self.calls += calls;
    }

    /// The breach to surface, ONCE per bucket: the first time a counting
    /// bucket's spend has reached its cap. `None` otherwise.
    pub fn take_breach(&mut self) -> Option<StepBreach> {
        if self.surfaced || !self.exhausted() {
            return None;
        }
        self.surfaced = true;
        Some(StepBreach { used: self.settled, budget: self.budget.unwrap_or(0) })
    }

    /// This bucket's outcome row: `None` without a label, without a cap, or
    /// when no call was made. `used_tokens` is the SETTLED spend;
    /// `skipped_calls` is always 0 now (a step never skips a call).
    pub fn record(&self) -> Option<RemoteBudgetRecord> {
        let label = self.label?;
        let budget = self.budget?;
        if self.calls == 0 {
            return None;
        }
        Some(RemoteBudgetRecord {
            stage: label.to_string(),
            max_tokens: budget,
            used_tokens: self.settled,
            exhausted: self.settled >= budget,
            skipped_calls: 0,
        })
    }
}

#[cfg(test)]
impl RemoteBudget {
    fn with_stage_label_for_test(budget: Option<u64>) -> Self {
        Self::labeled("s", budget, StepBudgetPolicy::Warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No budget: nothing counts, nothing is surfaced, and no row is
    /// emitted, however much is spent.
    #[test]
    fn no_budget_counts_nothing() {
        for policy in [StepBudgetPolicy::Off, StepBudgetPolicy::Warn] {
            let mut b = RemoteBudget::labeled("s", None, policy);
            for _ in 0..5 {
                b.admit_reserve(1_000_000);
                b.settle(1_000_000, 9_000_000, 1);
            }
            assert!(!b.counts() && !b.exhausted());
            assert_eq!(b.take_breach(), None);
            assert!(b.record().is_none());
        }
    }

    /// `off` with a budget set: still nothing counts.
    #[test]
    fn off_never_breaches_even_past_the_budget() {
        let mut b = RemoteBudget::new(Some(100), StepBudgetPolicy::Off);
        b.admit_reserve(500);
        b.settle(500, 1_000, 1);
        assert_eq!(b.take_breach(), None);
    }

    /// `warn`: the breach is surfaced exactly once, and no call is ever
    /// clamped (the reservation is the full request).
    #[test]
    fn warn_surfaces_the_breach_once_and_never_clamps() {
        let mut b = RemoteBudget::labeled("probe", Some(1_000), StepBudgetPolicy::Warn);
        b.admit_reserve(4_096);
        assert_eq!(b.used(), 4_096, "reserved in full: never clamped to the budget");
        b.settle(4_096, 1_200, 1);
        assert_eq!(b.take_breach(), Some(StepBreach { used: 1_200, budget: 1_000 }));
        assert_eq!(b.take_breach(), None, "surfaced once");
        for _ in 0..3 {
            b.admit_reserve(4_096);
            b.settle(4_096, 10, 1);
        }
        let rec = b.record().unwrap();
        assert_eq!((rec.max_tokens, rec.used_tokens, rec.exhausted, rec.skipped_calls), (1_000, 1_230, true, 0));
    }

    /// (review C-c) A breach is decided on SETTLED spend: siblings' calls
    /// in flight (reserved, not settled) never count, so the message never
    /// reports "12788 of 10000" when only 500 was spent.
    #[test]
    fn a_breach_counts_settled_spend_not_siblings_in_flight() {
        let mut b = RemoteBudget::new(Some(10_000), StepBudgetPolicy::Warn);
        for _ in 0..4 {
            b.admit_reserve(4_096); // four siblings in flight: 16384 reserved
        }
        b.settle(4_096, 500, 1);
        assert_eq!((b.settled(), b.used()), (500, 12_788), "the reviewer's numbers");
        assert!(!b.exhausted(), "12788 in use but only 500 spent");
        assert_eq!(b.take_breach(), None, "500 settled of 10000: no breach");
        b.settle(4_096, 9_600, 1);
        assert_eq!(b.take_breach(), Some(StepBreach { used: 10_100, budget: 10_000 }));
    }

    /// (zero doctrine) A cap of 0 is NO cap: nothing is counted, nothing is
    /// surfaced. (Pre-4.0, 0 was a hard refusal.)
    #[test]
    fn a_zero_cap_is_no_cap() {
        let mut n = RemoteBudget::with_stage_label_for_test(Some(0));
        n.admit_reserve(10);
        n.settle(10, 5_000, 1);
        assert!(!n.counts() && !n.exhausted());
        assert_eq!(n.take_breach(), None);
        assert_eq!(n.budget(), None);
        assert!(n.record().is_none(), "no cap, no row");
    }

    /// Concurrent siblings on one shared bucket: every call's reservation
    /// and settlement is accounted.
    #[test]
    fn concurrent_admit_reserve_settle_accounts_every_call() {
        use std::sync::{Arc, Mutex};
        const REQ: u32 = 100;
        const THREADS: u32 = 16;
        const ITERS: u32 = 20;
        let bucket = Arc::new(Mutex::new(RemoteBudget::labeled("stress", Some(1_000), StepBudgetPolicy::Warn)));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let bucket = Arc::clone(&bucket);
                std::thread::spawn(move || {
                    for _ in 0..ITERS {
                        bucket.lock().unwrap().admit_reserve(REQ);
                        bucket.lock().unwrap().settle(REQ, 7, 1);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(bucket.lock().unwrap().used(), u64::from(THREADS * ITERS) * 7);
    }
}
