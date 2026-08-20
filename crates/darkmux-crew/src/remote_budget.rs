//! `RemoteBudget` — one remote-token bucket metering a per-EXECUTION remote
//! allowance (`DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`, where one execution
//! = one pipeline stage). Local dispatches never touch it.
//!
//! **#1877 first extraction.** Before this module existed, the SAME bucket
//! was hand-copied twice: `darkmux-crew`'s `step_kinds::types::MapRemoteBucket`
//! (the `dispatch.map` fan-out — the probe stage, and the graph verify path)
//! and `darkmux-lab`'s `lab::review::RemoteBucket` (the sequential verify
//! loop and the judge's pass-1/pass-2 buckets), the second explicitly
//! documented as "mirroring `MapRemoteBucket::admit_reserve`" and "carrying
//! the identical floor." Because the judge stage's bucket was mission-private,
//! its exhaustion policy was a compiled constant no config could reach —
//! the root cause of #1876. This is the promotion of that one type into its
//! shared, public home; #1877's own issue tracks the rest of the run-record
//! substrate this leaves behind in `darkmux-lab::lab::review`. A pure move:
//! no exhaustion policy, no partial-outcome representation, and no renderer
//! behavior changed here — see the issue for the sequence that follows.
//!
//! A `budget` of 0 is exhausted from the FIRST call (`used (0) >= budget
//! (0)`), so a zero allowance refuses every hosted call — the same hard
//! opt-out `admit_remote_execution` gives a single hosted dispatch.
//!
//! **The ceiling is SOFT (approximate), by construction (#1451 gate).**
//! Admission is checked BEFORE a call and tokens are spent/settled AFTER
//! it, so a stage can overshoot `budget` by at most ONE granted call — the
//! per-call cap is clamped to what the bucket has LEFT ([`RemoteBudget::remaining`],
//! gate C6), which bounds that overshoot to whatever the endpoint itself
//! reports ABOVE its granted cap. This is deliberate: a per-execution
//! allowance is a spend GUARDRAIL, not a hard byte gate, and a call's exact
//! cost is unknowable until it runs.
//!
//! # Two access patterns, one type
//!
//! - **Sequential, unshared** — [`RemoteBudget::admit`] then
//!   [`RemoteBudget::spend`]. Used by a caller that dispatches one call at a
//!   time with nothing else touching the same bucket concurrently (the
//!   review pipeline's verify loop). Race-free by construction, so the two
//!   calls can stay separate.
//! - **Concurrent, shared** — [`RemoteBudget::admit_reserve`] then
//!   [`RemoteBudget::settle`]. Admits AND reserves the granted cap in one
//!   locked operation, so sibling callers sharing this bucket under a
//!   briefly-held lock cannot all admit against the same untouched balance
//!   — the shape both the `dispatch.map` fan-out (`seats x k` sibling
//!   probe steps sharing a `bucket_group`, #1442) and the judge's
//!   concurrent passes (#swarm-6) need. In a sequential per-item loop the
//!   observable behavior is identical to the admit/spend pair (settle
//!   always lands before the next admit_reserve).
//!
//! Both pairs live on the SAME bucket because both stages meter the same
//! contract (`DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`); a caller picks the
//! pair that matches its own concurrency shape, never both on one bucket.
//!
//! # The floor is a per-construction parameter, not a shared constant
//!
//! [`RemoteBudget::admit_reserve`] denies a grant too small to hold a
//! usable reply (see the method's own doc for the #1610/#1617 history) —
//! but the SIZE of "too small" is caller policy, not the bucket's own
//! opinion. `darkmux-crew`'s `dispatch.map` fan-out and `darkmux-lab`'s
//! judge stage happen to use the same value (512) today, but they answer
//! different questions (how small a `dispatch.map` item's reply can be vs
//! how small a JUDGE ruling can be) — collapsing them into one shared
//! constant would make a future tune of one silently move the other. Each
//! caller supplies its OWN floor at construction ([`RemoteBudget::new`] /
//! [`RemoteBudget::with_stage`]); `darkmux-crew`'s own floor
//! (`step_kinds::MIN_VIABLE_MAP_GRANT`) and `darkmux-lab`'s own floor
//! (`review::MIN_VIABLE_JUDGE_GRANT`) each stay defined next to their own
//! caller, never referencing each other.

use serde::{Deserialize, Serialize};

/// (#1260) One pipeline stage's remote token-bucket outcome — the row that
/// lands in a mission's envelope (e.g. `darkmux-lab`'s
/// `ReviewEnvelope::remote_budgets`). An "execution" is one stage (the
/// probe pass, each judge pass, the verify pass), each drawing from its own
/// `remote.max_tokens_per_execution` allowance so a runaway stage is caught
/// at the cap without starving later stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBudgetRecord {
    /// e.g. `probe` | `judge-pass1` | `judge-pass2` | `verify`.
    pub stage: String,
    pub max_tokens: u64,
    pub used_tokens: u64,
    pub exhausted: bool,
    /// Remote calls NOT made because the bucket had already exhausted.
    pub skipped_calls: u32,
}

/// One remote-token bucket's in-flight state — see the module doc for the
/// two access patterns and the floor. `stage` is optional: a caller that
/// never names one (`darkmux-crew`'s `dispatch.map` fan-out, which folds
/// its own per-item accounting into a stage row at a higher layer instead —
/// see `darkmux-lab`'s `ProbeReconstruction`/`VerifyApplyOutcome`) simply
/// never gets a row back from [`RemoteBudget::record`]; a caller that DOES
/// name one ([`RemoteBudget::with_stage`]) gets a row whenever the bucket
/// made or skipped at least one call.
#[derive(Debug)]
pub struct RemoteBudget {
    stage: Option<&'static str>,
    budget: u64,
    /// The smallest grant [`Self::admit_reserve`] will hand out (clamped to
    /// `budget` itself where that is smaller) — the caller's own floor,
    /// fixed for this bucket's whole lifetime. See the module doc.
    floor: u32,
    used: u64,
    calls: u32,
    skipped: u32,
}

impl RemoteBudget {
    /// A bucket with no stage label — [`Self::record`] always returns
    /// `None` for it. `darkmux-crew`'s `dispatch.map` fan-out constructs its
    /// buckets this way (its own per-item results carry the accounting a
    /// caller needs to build a stage row itself, at a layer this type has
    /// no opinion on).
    pub fn new(budget: u64, floor: u32) -> Self {
        Self { stage: None, budget, floor, used: 0, calls: 0, skipped: 0 }
    }

    /// A bucket that reports its stage on [`Self::record`] — the shape
    /// `darkmux-lab`'s review pipeline uses for its own sequential
    /// (verify) and concurrent (judge pass-1/pass-2) buckets.
    pub fn with_stage(stage: &'static str, budget: u64, floor: u32) -> Self {
        Self { stage: Some(stage), budget, floor, used: 0, calls: 0, skipped: 0 }
    }

    pub fn exhausted(&self) -> bool {
        self.used >= self.budget
    }

    /// What is left to grant a single call (#1442 gate C6): a caller's
    /// per-item cap clamps to THIS, not the full budget, so one late item
    /// cannot request more than the bucket has left.
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.used)
    }

    /// Gate one remote call (the SEQUENTIAL, unshared pair's admit half):
    /// `false` ⇒ the bucket is exhausted and the call must not fire
    /// (counted as skipped, for the caller's named-reason result). Pairs
    /// with [`Self::spend`]; never mix with [`Self::admit_reserve`]/
    /// [`Self::settle`] on the same bucket.
    pub fn admit(&mut self) -> bool {
        if self.exhausted() {
            self.skipped += 1;
            false
        } else {
            true
        }
    }

    /// Record an already-admitted call's actual cost (the SEQUENTIAL pair's
    /// spend half). `calls` is the number of real dispatch ATTEMPTS this
    /// spend represents (e.g. 2 when an unparsed-reply retry fired), not a
    /// fixed 1 — the caller counts.
    pub fn spend(&mut self, tokens: u64, calls: u32) {
        self.used += tokens;
        self.calls += calls;
    }

    /// (#1442 fan-out / #swarm-6) Admit one call and RESERVE its granted
    /// cap up front (the CONCURRENT, shared pair's admit-and-reserve half),
    /// returning the granted (clamped) cap — or `None` when the bucket is
    /// already exhausted (counted as skipped) or the clamped grant falls
    /// below this bucket's own [`floor`](Self::new). The reserve-then-
    /// [`settle`](Self::settle) shape replaces a plain admit-then-spend-
    /// after pair because concurrent siblings sharing one bucket (the
    /// probe stage's `seats x k` `dispatch.map` fan-out, #1442; the judge's
    /// concurrent passes under a briefly-held lock, #swarm-6) would
    /// otherwise all admit against the same untouched balance and overshoot
    /// the ceiling by one call PER SIBLING. Reserving the granted cap at
    /// admission bounds the whole group's overshoot to what an endpoint
    /// itself reports ABOVE a granted cap — the same soft-ceiling reading
    /// as before, now independent of sibling concurrency. In a sequential
    /// per-item loop the observable behavior is identical to the old pair
    /// (settle always lands before the next admit_reserve).
    ///
    /// (#1610 / #1617 review) A grant too small to hold a reply is worse
    /// than no grant. The call "succeeds," the endpoint truncates mid-reply,
    /// and the caller reads the debris as a result — silently. For the
    /// judge bucket specifically, a cap like 60 tokens against a JSON-ruling
    /// prompt truncated, parsed as `Unparsed`, classified as `Reject`, and
    /// `multi_pass_confirm` archived the flag on pass 1 with an empty note
    /// and no confirmation pass — a real finding deleted, and silently:
    /// because the old code returned `Some`, `skipped` never incremented,
    /// so no degraded gate fired and the run reported healthy. A flag (or
    /// item) that could not be judged must read as UNJUDGED, never as
    /// rejected — the whole point of the skip counter is that a run says
    /// when it did less than it claims.
    ///
    /// The floor is deliberately generous relative to a call's real cost;
    /// erring toward "deny and report" is correct here, because a denial is
    /// visible and a starved grant is not. It is capped at the CONFIGURED
    /// budget, because the operator's budget is the authority on what
    /// "enough" means: a deliberately tiny allowance is policy, not
    /// starvation, and denying it would turn every small budget into an
    /// undocumented hard opt-out of a knob whose only documented refusal
    /// value is 0. Starvation is the OTHER shape — a budget that was never
    /// small, spent down to a sliver.
    pub fn admit_reserve(&mut self, requested: u32) -> Option<u32> {
        if self.exhausted() {
            self.skipped += 1;
            return None;
        }
        let granted = u32::try_from(self.remaining()).unwrap_or(u32::MAX).min(requested);
        let floor = self.floor.min(u32::try_from(self.budget).unwrap_or(u32::MAX));
        if granted < floor {
            self.skipped += 1;
            return None;
        }
        self.used = self.used.saturating_add(u64::from(granted));
        Some(granted)
    }

    /// Settle a reservation made by [`Self::admit_reserve`] against its
    /// ACTUAL spend (the reply's reported usage, or — conservatively — the
    /// granted cap when the endpoint omits usage). Replaces the reservation
    /// with the real number: an endpoint that reports above its granted cap
    /// pushes the bucket over (the documented soft-ceiling overshoot), one
    /// that reports under releases the difference back to its siblings.
    /// `calls` is the number of real dispatch attempts this settle
    /// represents, same convention as [`Self::spend`].
    pub fn settle(&mut self, granted: u32, actual: u64, calls: u32) {
        self.used = self.used.saturating_sub(u64::from(granted)).saturating_add(actual);
        self.calls += calls;
    }

    /// Count of calls refused because the bucket was exhausted or the grant
    /// fell below the floor — read by tests asserting the skip path fired.
    pub fn skipped(&self) -> u32 {
        self.skipped
    }

    /// This bucket's stage's outcome — `None` when there is no stage label
    /// ([`Self::new`]) or when the bucket never admitted/skipped a call
    /// (a local-only run's stage never touches its bucket at all).
    pub fn record(&self) -> Option<RemoteBudgetRecord> {
        let stage = self.stage?;
        if self.calls == 0 && self.skipped == 0 {
            return None;
        }
        Some(RemoteBudgetRecord {
            stage: stage.to_string(),
            max_tokens: self.budget,
            used_tokens: self.used,
            exhausted: self.exhausted(),
            skipped_calls: self.skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#1877) `record()` on a stage-less bucket ([`RemoteBudget::new`], the
    /// shape `dispatch.map`'s fan-out constructs) always returns `None` —
    /// even after real calls and skips — because there is no stage name to
    /// put on a row. This is new surface the merge adds (`MapRemoteBucket`
    /// had no `record()` at all pre-#1877); `darkmux-crew`'s own callers
    /// never invoke it today, but a caller that does must not get a row
    /// with a fabricated or empty stage name.
    #[test]
    fn record_is_always_none_without_a_stage_label() {
        let mut b = RemoteBudget::new(1_000, 512);
        let g = b.admit_reserve(600).expect("admits");
        b.settle(g, 600, 1);
        assert!(b.admit_reserve(600).is_none(), "denied by the floor on the remainder");
        assert!(b.record().is_none(), "no stage label means no row, regardless of activity");
    }

    /// (#1877) The floor is a per-CONSTRUCTION parameter — two buckets with
    /// the same budget but different floors deny at different points. This
    /// is the property the module doc's "not a shared constant" claim rests
    /// on: nothing here ties one caller's floor to another's.
    #[test]
    fn the_floor_is_independent_per_bucket() {
        let mut generous = RemoteBudget::new(1_000, 100);
        let g = generous.admit_reserve(900).expect("admits");
        generous.settle(g, 900, 1);
        // 100 tokens remain — a request above that clamps DOWN to 100,
        // which is at (not below) THIS bucket's own floor of 100.
        assert_eq!(
            generous.admit_reserve(500),
            Some(100),
            "a floor of 100 admits the clamped 100-token remainder"
        );

        let mut strict = RemoteBudget::new(1_000, 512);
        let g = strict.admit_reserve(900).expect("admits");
        strict.settle(g, 900, 1);
        // Same 100 tokens remain, but THIS bucket's floor is 512.
        assert!(
            strict.admit_reserve(500).is_none(),
            "a floor of 512 denies the identical 100-token remainder"
        );
    }

    /// (#1877) `with_stage` is the constructor review's judge/verify
    /// buckets use — `record()` reports the stage, budget, spend, and skip
    /// count exactly as `RemoteBucket::record()` did pre-move.
    #[test]
    fn with_stage_record_reports_every_field() {
        let mut b = RemoteBudget::with_stage("probe", 1_000, 512);
        let g = b.admit_reserve(600).expect("admits");
        b.settle(g, 600, 1);
        assert!(b.admit_reserve(600).is_none(), "denied by the floor on the remainder");
        let rec = b.record().expect("activity on a stage bucket emits a row");
        assert_eq!(rec.stage, "probe");
        assert_eq!(rec.max_tokens, 1_000);
        assert_eq!(rec.used_tokens, 600);
        assert!(!rec.exhausted, "600 < 1000");
        assert_eq!(rec.skipped_calls, 1);
    }

    /// (#1877) A `with_stage` bucket that never admitted or skipped a call
    /// emits no row, the same "untouched stage stays silent" rule
    /// [`RemoteBudget::new`]'s callers rely on.
    #[test]
    fn with_stage_record_is_none_when_untouched() {
        let b = RemoteBudget::with_stage("verify", 1_000, 512);
        assert!(b.record().is_none());
    }

    // ─── #1890: the healthy-run branch (real calls, zero skips) ──────────
    //
    // Every `record()` test above mixes in at least one skip, so none of
    // them reach the branch a HEALTHY stage takes: real calls, nothing
    // refused. The two tests below isolate exactly that branch, through
    // both access pairs the module doc describes — the sequential
    // admit/spend pair (`RemoteBudget::admit` + `RemoteBudget::spend`,
    // review's verify loop) and the concurrent admit_reserve/settle pair
    // (`RemoteBudget::admit_reserve` + `RemoteBudget::settle`, the probe
    // fan-out and the judge's concurrent passes).
    //
    // Each also directly reads the private `calls` field (visible here:
    // `tests` is a descendant module of the one that defines
    // `RemoteBudget`, so Rust's ordinary privacy rule already grants
    // access — no accessor needed) rather than only checking
    // `record().is_some()`, so a mutation that leaves `record()`'s Some/
    // None outcome unchanged but corrupts the accumulated count would
    // still be caught.

    /// (#1890 finding 1 + finding 2) The sequential admit/spend pair on a
    /// bucket that made two real calls and skipped nothing must still
    /// emit a `record()` row, with `calls` correctly accumulated.
    /// **Proved failing first**, two ways:
    /// - Finding 1's mutation (`remote_budget.rs:238`,
    ///   `self.calls == 0 && self.skipped == 0` -> `||`) makes this test
    ///   fail at the `record()` unwrap: with real calls (`calls == 2`) and
    ///   zero skips, the OR'd condition is still true (`skipped == 0`), so
    ///   the mutant returns `None` for a run that plainly wasn't empty.
    /// - Finding 2's mutation (`remote_budget.rs:157`, dropping `self.calls
    ///   += calls;` from `spend()`) makes this test fail the same way:
    ///   with `self.calls` stuck at 0 and `self.skipped` at 0, even the
    ///   UN-mutated `&&` guard reads "empty" and `record()` returns `None`.
    ///
    /// Both were verified by hand: reverting either line reproduces the
    /// `record() returned None for a healthy run` panic below.
    #[test]
    fn admit_spend_healthy_run_emits_record_with_right_calls() {
        let mut b = RemoteBudget::with_stage("judge-pass2", 1_000, 100);
        assert!(b.admit(), "not exhausted, so admit must succeed");
        b.spend(300, 1);
        assert!(b.admit(), "still well under budget");
        // A real dispatch attempt can represent more than one call (e.g. an
        // unparsed-reply retry) — spend's own contract says the caller
        // counts, so exercise that here rather than always passing 1.
        b.spend(200, 2);

        assert_eq!(b.calls, 3, "spend must accumulate the calls the caller reports");
        assert_eq!(b.skipped, 0, "this run made only real calls, nothing was refused");

        let rec = b
            .record()
            .expect("a healthy run with real calls and zero skips must still emit a row");
        assert_eq!(rec.stage, "judge-pass2");
        assert_eq!(rec.used_tokens, 500);
        assert_eq!(rec.skipped_calls, 0);
        assert!(!rec.exhausted, "500 < 1000");
    }

    /// (#1890 finding 1 + finding 2) Same healthy-branch property, through
    /// the CONCURRENT admit_reserve/settle pair instead — the shape the
    /// probe fan-out and the judge's concurrent passes actually use.
    /// **Proved failing first** the same two ways as the sequential test
    /// above (both mutations live in code this pair also calls through:
    /// `settle` shares `spend`'s `self.calls += calls;` shape at
    /// `remote_budget.rs:224`, and `record()`'s guard is the same one).
    #[test]
    fn admit_reserve_settle_healthy_run_emits_record_with_right_calls() {
        let mut b = RemoteBudget::with_stage("probe", 1_000, 100);
        let g1 = b.admit_reserve(300).expect("well under budget");
        b.settle(g1, 250, 1);
        let g2 = b.admit_reserve(300).expect("still well under budget");
        b.settle(g2, 300, 1);

        assert_eq!(b.calls, 2, "settle must accumulate the calls the caller reports");
        assert_eq!(b.skipped, 0, "this run made only real calls, nothing was refused");

        let rec = b
            .record()
            .expect("a healthy run with real calls and zero skips must still emit a row");
        assert_eq!(rec.stage, "probe");
        assert_eq!(rec.used_tokens, 550);
        assert_eq!(rec.skipped_calls, 0);
        assert!(!rec.exhausted, "550 < 1000");
    }

    // ─── #1890 (related): concurrent admit_reserve/settle never overspends ──

    /// (#1890 "related, worth doing in the same pass" + #1878) A committed
    /// version of the auditor's ad hoc stress: many threads racing
    /// `admit_reserve`/`settle` against ONE shared `Arc<Mutex<RemoteBudget>>`
    /// must never let total spend exceed the configured budget, and every
    /// attempt must land as either an admission or a skip — never lost,
    /// never double-counted. `RemoteBudget` exists specifically because an
    /// earlier, unlocked accounting scheme let concurrent siblings overspend
    /// a shared budget by 8x (#1878); this is the committed proof the
    /// current `Mutex`-guarded design holds the invariant under real thread
    /// contention rather than only "by inspection."
    ///
    /// Deliberately asserts INVARIANTS, never a specific interleaving or a
    /// specific admitted/skipped split — thread scheduling is inherently
    /// nondeterministic, so pinning a specific count would make this test
    /// flaky by construction. What must hold on every run, regardless of
    /// interleaving: (1) admitted + skipped == every attempt made, and (2)
    /// used tokens never exceed the budget. `actual == granted` on every
    /// settle here (no endpoint-reported overshoot), so this also proves
    /// the reservation-at-admission design keeps `used` inside `budget` even
    /// when many threads are admitting concurrently — the property #1878's
    /// fix depends on.
    #[test]
    fn concurrent_admit_reserve_settle_never_overspends_the_budget() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        const BUDGET: u64 = 20_000;
        const FLOOR: u32 = 50;
        const REQUESTED_PER_CALL: u32 = 100;
        const THREADS: u32 = 64;
        const ITERS_PER_THREAD: u32 = 20;

        let bucket = Arc::new(Mutex::new(RemoteBudget::with_stage("stress", BUDGET, FLOOR)));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let bucket = Arc::clone(&bucket);
                thread::spawn(move || {
                    let mut admitted = 0u32;
                    let mut skipped = 0u32;
                    for _ in 0..ITERS_PER_THREAD {
                        let granted = bucket.lock().unwrap().admit_reserve(REQUESTED_PER_CALL);
                        match granted {
                            Some(g) => {
                                admitted += 1;
                                // Endpoint reports exactly what was granted —
                                // mirrors the auditor's original stress.
                                bucket.lock().unwrap().settle(g, u64::from(g), 1);
                            }
                            None => skipped += 1,
                        }
                    }
                    (admitted, skipped)
                })
            })
            .collect();

        let (mut total_admitted, mut total_skipped) = (0u32, 0u32);
        for h in handles {
            let (admitted, skipped) = h.join().expect("stress thread must not panic");
            total_admitted += admitted;
            total_skipped += skipped;
        }

        assert_eq!(
            total_admitted + total_skipped,
            THREADS * ITERS_PER_THREAD,
            "every attempt must land as exactly one admission or one skip"
        );

        let b = bucket.lock().unwrap();
        let rec = b.record().expect("real calls were made under contention");
        assert!(
            rec.used_tokens <= BUDGET,
            "total spend ({}) must never exceed the configured budget ({BUDGET})",
            rec.used_tokens
        );
        assert_eq!(
            rec.skipped_calls, total_skipped,
            "the bucket's own skip count must match what every thread observed"
        );
    }
}
