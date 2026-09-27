//! `RemoteBudget`: one pipeline stage's hosted-token bucket, metering the
//! STAGE budget (`remote.max_tokens_per_execution`, where one execution =
//! one pipeline stage; a bare hosted `dispatch` is one). Local calls never
//! touch it.
//!
//! **(#2902 step 5) The budget is the operator's, and reaching it never
//! stops the stage.** There is no built-in number: `None` is no stage
//! budget, and nothing is counted. When the operator sets one, the stage
//! policy (`remote.stage_budget_policy`, the same `off` / `warn` / `wait`
//! words an endpoint budget uses, [`BudgetPolicy`]) decides what reaching it
//! does:
//!
//! - `off`: nothing is counted.
//! - `warn` (absent = `warn`): every call is admitted, and the first time
//!   the stage's spend reaches its budget [`RemoteBudget::take_breach`]
//!   hands the caller one breach to surface (`crate::budget` prints and
//!   records it).
//! - `wait`: once the budget is spent, [`RemoteBudget::admit_reserve`]
//!   answers [`StageAdmit::Wait`] and the caller waits
//!   (`crate::budget::admit_stage`) until the operator raises the budget or
//!   switches the policy (re-read from disk while it waits), or aborts the
//!   run. A stage has no rolling window, so nothing frees room on its own.
//!
//! Before 4.0 this bucket had a 500000 default, SKIPPED the calls a spent
//! stage would have made (a named envelope reason), and clamped each call's
//! `max_tokens` to what was left, denying a grant below a per-caller floor
//! (#1610). All three are gone: skipping and clamping both stopped work the
//! operator never asked to stop, and with no clamp there is no starved grant
//! for a floor to deny.
//!
//! **The ceiling is SOFT, by construction.** Admission is checked BEFORE a
//! call and the call's cost is settled AFTER it, so a stage can overshoot by
//! whatever the calls in flight at the crossing spend.
//!
//! **Concurrent siblings.** [`RemoteBudget::admit_reserve`] reserves the
//! requested cap in the same locked operation it admits in, and
//! [`RemoteBudget::settle`] replaces the reservation with the real spend, so
//! sibling `dispatch.map` steps sharing one `bucket_group` (#1442) see each
//! other's in-flight calls rather than all admitting against the same
//! untouched balance.

use darkmux_types::BudgetPolicy;
use serde::{Deserialize, Serialize};

/// (#1260) One pipeline stage's remote token-bucket outcome — the row that
/// lands in a mission's envelope (e.g. `darkmux-lab`'s
/// `ReviewEnvelope::remote_budgets`). An "execution" is one stage (the
/// probe pass, each judge pass, the verify pass), each drawing from its own
/// `remote.max_tokens_per_execution` allowance so a runaway stage is caught
/// at the cap without starving later stages.
// (#2310 P2) `PartialEq` is new — every field is a plain String/u64/bool/
// u32, so this rides inside a typed `Output<T>` body (which derives
// `PartialEq` throughout — see `darkmux_crew::step_output`'s module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteBudgetRecord {
    /// e.g. `probe` | `judge-pass1` | `judge-pass2` | `verify`.
    pub stage: String,
    pub max_tokens: u64,
    pub used_tokens: u64,
    pub exhausted: bool,
    /// Remote calls NOT made because the bucket had already exhausted.
    pub skipped_calls: u32,
}

/// A stage whose spend has reached its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageBreach {
    pub used: u64,
    pub budget: u64,
}

/// What [`RemoteBudget::admit_reserve`] says about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAdmit {
    /// Make the call. The requested cap is reserved; settle it after.
    Proceed,
    /// `wait` policy and the budget is spent: hold the call.
    Wait(StageBreach),
}

/// One stage's bucket. See the module doc.
#[derive(Debug)]
pub struct RemoteBudget {
    stage: Option<&'static str>,
    budget: Option<u64>,
    policy: BudgetPolicy,
    used: u64,
    calls: u32,
    /// The breach has been handed out ([`Self::take_breach`] fires once).
    surfaced: bool,
}

impl RemoteBudget {
    /// A bucket with no stage label ([`Self::record`] returns `None`).
    pub fn new(budget: Option<u64>, policy: BudgetPolicy) -> Self {
        Self { stage: None, budget, policy, used: 0, calls: 0, surfaced: false }
    }

    /// A bucket that reports its stage on [`Self::record`].
    pub fn with_stage(stage: &'static str, budget: Option<u64>, policy: BudgetPolicy) -> Self {
        Self { stage: Some(stage), ..Self::new(budget, policy) }
    }

    /// The bucket the configuration describes: `explicit` (a launcher's
    /// `bucket_budget` stamped into the step's own config) else
    /// `remote.max_tokens_per_execution` (no default), under
    /// `remote.stage_budget_policy`. An unregistered policy is an error,
    /// never a fallback (preflight refuses it before any run starts).
    pub fn from_config(explicit: Option<u64>) -> Result<Self, darkmux_types::config_enum::BadEnumValue> {
        let policy = darkmux_types::config_access::remote_stage_budget_policy()?;
        Ok(Self::new(explicit.or_else(darkmux_types::config_access::remote_max_tokens_per_execution), policy))
    }

    /// True when this bucket counts anything: a budget is set and the
    /// policy is not `off`.
    pub fn counts(&self) -> bool {
        self.budget.is_some() && self.policy.counts()
    }

    /// The stage budget, when one is set.
    pub fn budget(&self) -> Option<u64> {
        self.budget
    }

    pub fn policy(&self) -> BudgetPolicy {
        self.policy
    }

    /// Tokens spent (and reserved for calls in flight).
    pub fn used(&self) -> u64 {
        self.used
    }

    /// True when a counting bucket's spend has reached its budget.
    pub fn exhausted(&self) -> bool {
        self.counts() && self.budget.is_some_and(|b| self.used >= b)
    }

    fn breach(&self) -> StageBreach {
        StageBreach { used: self.used, budget: self.budget.unwrap_or(0) }
    }

    /// Admit one call and reserve `requested` (the call's completion cap)
    /// against the bucket, or, under `wait` with the budget spent, say to
    /// wait. Never clamps and never refuses: under `off`, `warn` or no
    /// budget the call always proceeds.
    pub fn admit_reserve(&mut self, requested: u32) -> StageAdmit {
        if self.policy == BudgetPolicy::Wait && self.exhausted() {
            return StageAdmit::Wait(self.breach());
        }
        self.used = self.used.saturating_add(u64::from(requested));
        StageAdmit::Proceed
    }

    /// Replace a reservation made by [`Self::admit_reserve`] with the call's
    /// ACTUAL spend (its reported usage, or the reserved cap when the
    /// endpoint reported none). `calls` is the dispatch attempts it
    /// represents.
    pub fn settle(&mut self, reserved: u32, actual: u64, calls: u32) {
        self.used = self.used.saturating_sub(u64::from(reserved)).saturating_add(actual);
        self.calls += calls;
    }

    /// The breach to surface, ONCE per bucket: the first time a counting
    /// bucket's spend has reached its budget. `None` otherwise.
    pub fn take_breach(&mut self) -> Option<StageBreach> {
        if self.surfaced || !self.exhausted() {
            return None;
        }
        self.surfaced = true;
        Some(self.breach())
    }

    /// Adopt a budget and policy re-read while waiting (the operator raised
    /// the budget, or changed the policy).
    pub fn reconfigure(&mut self, budget: Option<u64>, policy: BudgetPolicy) {
        if budget != self.budget {
            self.surfaced = false;
        }
        self.budget = budget;
        self.policy = policy;
    }

    /// This stage's outcome row: `None` without a stage label, without a
    /// budget, or when no call was made. `skipped_calls` is always 0 now
    /// (a stage never skips a call); the field stays for the row's shape.
    pub fn record(&self) -> Option<RemoteBudgetRecord> {
        let stage = self.stage?;
        let budget = self.budget?;
        if self.calls == 0 {
            return None;
        }
        Some(RemoteBudgetRecord {
            stage: stage.to_string(),
            max_tokens: budget,
            used_tokens: self.used,
            exhausted: self.used >= budget,
            skipped_calls: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No budget: nothing counts, every call proceeds, nothing is surfaced,
    /// and no row is emitted, however much is spent.
    #[test]
    fn no_budget_counts_nothing() {
        for policy in [BudgetPolicy::Off, BudgetPolicy::Warn, BudgetPolicy::Wait] {
            let mut b = RemoteBudget::with_stage("s", None, policy);
            for _ in 0..5 {
                assert_eq!(b.admit_reserve(1_000_000), StageAdmit::Proceed);
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
        let mut b = RemoteBudget::new(Some(100), BudgetPolicy::Off);
        assert_eq!(b.admit_reserve(500), StageAdmit::Proceed);
        b.settle(500, 1_000, 1);
        assert_eq!(b.admit_reserve(500), StageAdmit::Proceed);
        assert_eq!(b.take_breach(), None);
    }

    /// `warn`: every call proceeds, the breach is surfaced exactly once, and
    /// no call is ever clamped (the reservation is the full request).
    #[test]
    fn warn_admits_every_call_and_surfaces_the_breach_once() {
        let mut b = RemoteBudget::with_stage("probe", Some(1_000), BudgetPolicy::Warn);
        assert_eq!(b.admit_reserve(4_096), StageAdmit::Proceed);
        assert_eq!(b.used(), 4_096, "reserved in full: never clamped to the budget");
        b.settle(4_096, 1_200, 1);
        assert_eq!(b.take_breach(), Some(StageBreach { used: 1_200, budget: 1_000 }));
        assert_eq!(b.take_breach(), None, "surfaced once");
        for _ in 0..3 {
            assert_eq!(b.admit_reserve(4_096), StageAdmit::Proceed, "warn never holds a call");
            b.settle(4_096, 10, 1);
        }
        let rec = b.record().unwrap();
        assert_eq!((rec.max_tokens, rec.used_tokens, rec.exhausted, rec.skipped_calls), (1_000, 1_230, true, 0));
    }

    /// `wait`: calls proceed while there is room; once spent, the next call
    /// waits (and is not reserved); a raised budget lets it proceed.
    #[test]
    fn wait_holds_only_once_spent_and_a_raised_budget_releases_it() {
        let mut b = RemoteBudget::new(Some(1_000), BudgetPolicy::Wait);
        assert_eq!(b.admit_reserve(600), StageAdmit::Proceed);
        b.settle(600, 900, 1);
        assert_eq!(b.admit_reserve(600), StageAdmit::Proceed, "900 < 1000: room");
        b.settle(600, 300, 1);
        assert_eq!(b.admit_reserve(600), StageAdmit::Wait(StageBreach { used: 1_200, budget: 1_000 }));
        assert_eq!(b.used(), 1_200, "a waiting call reserves nothing");
        b.reconfigure(Some(5_000), BudgetPolicy::Wait);
        assert_eq!(b.admit_reserve(600), StageAdmit::Proceed);
        b.reconfigure(Some(10), BudgetPolicy::Warn);
        assert_eq!(b.admit_reserve(600), StageAdmit::Proceed, "switched to warn: never holds");
    }

    /// A zero budget under `wait` holds the first call; under `warn` it
    /// proceeds and surfaces the breach. (Pre-4.0, 0 was a hard refusal.)
    #[test]
    fn a_zero_budget_waits_or_warns_and_never_refuses() {
        let mut w = RemoteBudget::new(Some(0), BudgetPolicy::Wait);
        assert!(matches!(w.admit_reserve(10), StageAdmit::Wait(_)));
        let mut n = RemoteBudget::new(Some(0), BudgetPolicy::Warn);
        assert_eq!(n.admit_reserve(10), StageAdmit::Proceed);
        assert!(n.take_breach().is_some());
    }

    /// Concurrent siblings on one shared bucket: every attempt is admitted
    /// or told to wait (never lost), and the reservations keep a `wait`
    /// bucket's admitted spend within the budget plus one call per thread in
    /// flight at the crossing.
    #[test]
    fn concurrent_admit_reserve_settle_accounts_every_attempt() {
        use std::sync::{Arc, Mutex};
        const BUDGET: u64 = 20_000;
        const REQ: u32 = 100;
        const THREADS: u32 = 32;
        const ITERS: u32 = 20;
        let bucket = Arc::new(Mutex::new(RemoteBudget::with_stage("stress", Some(BUDGET), BudgetPolicy::Wait)));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let bucket = Arc::clone(&bucket);
                std::thread::spawn(move || {
                    let (mut admitted, mut held) = (0u32, 0u32);
                    for _ in 0..ITERS {
                        let a = bucket.lock().unwrap().admit_reserve(REQ);
                        match a {
                            StageAdmit::Proceed => {
                                admitted += 1;
                                bucket.lock().unwrap().settle(REQ, u64::from(REQ), 1);
                            }
                            StageAdmit::Wait(_) => held += 1,
                        }
                    }
                    (admitted, held)
                })
            })
            .collect();
        let (mut admitted, mut held) = (0, 0);
        for h in handles {
            let (a, w) = h.join().unwrap();
            admitted += a;
            held += w;
        }
        assert_eq!(admitted + held, THREADS * ITERS);
        let b = bucket.lock().unwrap();
        assert!(b.used() <= BUDGET, "reservations keep admitted spend inside the budget: {}", b.used());
        assert_eq!(u64::from(admitted) * u64::from(REQ), b.used());
    }
}
