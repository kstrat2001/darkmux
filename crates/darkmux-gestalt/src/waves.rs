//! Budget-driven co-residency wave scheduling (#1285): partition desired
//! placements into the largest co-resident sets that fit, replacing the
//! funnel's hardware-tier-threshold Auto (the last machine-tier residue —
//! the concept was removed end-to-end in #602/#604/#605) with arithmetic
//! over the same facts the planner reasons on.
//!
//! Pure, like everything else in this crate: `(placements, facts, estimator,
//! mode)` in ⇒ identical [`WaveSchedule`] out, always. The funnel's
//! `LmsCycler` consumes waves at the packet-3 cutover, and the #1243 budget
//! doubles as a hardware-tier emulator: set `budget = 24GB` on a 128GB
//! machine and the schedule is the one a 32GB box would derive.
//!
//! # Packing rule (deterministic, documented)
//!
//! First-fit in ORIGINAL PLACEMENT ORDER: walk placements in input order and
//! append each to the FIRST wave whose running total still fits the
//! effective limit, opening a new wave when none does. Deliberately NOT
//! first-fit-decreasing: FFD packs tighter in the worst case but reorders by
//! size, and input order is operator-declared priority (the primary seat is
//! listed first and should land in wave 1, not wherever its size sorts).
//! Within a wave, placements keep input order. Same input ⇒ identical
//! schedule, always.
//!
//! # The effective limit
//!
//! `min` over the two ceilings, each active only when its fact exists —
//! built exclusively from [`crate::planner`]'s shared #1243/#1140 accounting
//! helpers (one definition of the base and the freed credit, not two):
//!
//! - **Budget headroom** (#1243): `budget − un-evictable darkmux base`. The
//!   base is [`crate::planner`]'s resident accounting — darkmux-owned
//!   resident bytes, minus the stales of reconciles that SURVIVE the refusal
//!   passes (freed before their reload; see the ordering below). The
//!   scheduler plans no evictions (that is `plan_acquire`'s job), so every
//!   darkmux-owned resident it cannot prove leaves is base. Residents a
//!   placement Reuses are base too — which is exactly why a Reuse costs
//!   zero additional bytes below.
//! - **Pool headroom** (single-pool v1 rule, as in the planner's #1140 arm;
//!   zero or multiple pools skip the arm): the pool's `available_bytes`
//!   snapshot plus the stale bytes of surviving reconciles. Foreign
//!   residents are pool consumption ONLY (absolute ownership, #1274): they
//!   already depress `available_bytes` at probe time and are never named as
//!   evictable, and they never touch the budget arm (#1243 counts
//!   darkmux-owned bytes only).
//!
//! v1 simplification (documented): ONE limit, computed from the facts
//! snapshot, applies to every wave — per-wave re-snapshots are the
//! executor's re-verify concern (the #1274 staleness discipline), not the
//! scheduler's. With neither fact present there is no known constraint:
//! everything schedules as one wave and the executor's #1139
//! insufficient-resources fast-fail backstops, exactly as in `plan_acquire`.
//!
//! # Refusal vocabulary and ordering (`plan_acquire` parity)
//!
//! Which ceiling refuses, and with what — `budget_bytes` means ONE thing
//! crate-wide:
//!
//! - **Budget-bound** (the estimate cannot fit the #1243 budget atop the
//!   un-evictable base): [`Reason::BudgetRefuse`] carrying the CONFIGURED
//!   budget in `budget_bytes`, exactly as `plan_acquire` emits it — never
//!   the min'd effective limit (that number lives on
//!   [`WaveSchedule::effective_limit_bytes`], an artifact/packing field).
//! - **Pool-bound, no foreign duplicate**: NOT refused. Pool facts are
//!   advisory headroom, not an operator contract (the planner's #1140
//!   doctrine — `plan_acquire` never refuses this shortfall either): the
//!   placement goes alone in its own wave and the executor's #1139
//!   insufficient-resources fast-fail owns any physical shortfall.
//! - **Pool-bound behind a foreign duplicate**:
//!   [`Reason::ForeignDuplicateNoCapacity`] naming the user-loaded
//!   instance, its bytes, and the pool headroom darkmux may not grow by
//!   eviction (absolute ownership, #1274) — the one pool shortfall with a
//!   nameable, un-evictable cause, exactly the planner's pool arm.
//!
//! Refusal ordering is `plan_acquire`'s, via the shared removal-timing
//! helper ([`crate::planner`]'s `commit_surviving_stales`): flat budget
//! refusals run FIRST against the un-credited accounting, then reconcile
//! stale credits commit only for the survivors, then the fit pass and the
//! packing run against the updated base. A refused reconcile never unloads
//! its stale, so that resident stays loaded AND stays counted — crediting a
//! refused reconcile's stale as freed, then packing later placements into
//! headroom that does not physically exist, is the phantom-headroom bug
//! class this ordering prevents.

use crate::desired::Placement;
use crate::estimator::FootprintEstimator;
use crate::facts::Facts;
use crate::plan::{Reason, Warning};
use crate::planner::{
    commit_surviving_stales, resident_base, single_pool_headroom,
    warn_unknown_owned_resident_bytes, ReconcileStale,
};
use crate::residency::{decide_residency, ResidencyDecision};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Execution-shape override (#1285). `Auto` derives parallel↔sequential from
/// the arithmetic; the Force modes are operator sovereignty (#44) — the
/// operator's explicit shape wins over the derived one. Deliberately NO
/// `Default` impl, the [`crate::planner::AcquireOpts`] reasoning: every
/// caller names its mode; a CLI layer maps flag-absence to `Auto`
/// explicitly, never implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaveMode {
    /// Derived scheduling: first-fit co-residency packing under the
    /// effective limit (module docs).
    Auto,
    /// Everything in ONE wave, or the loud [`ForceParallelRefused`] — NEVER
    /// a silent fallback to sequential (operator decision on #1285: a
    /// silent realignment corrupts timing experiments).
    ForceParallel,
    /// One placement per wave, in input order.
    ForceSequential,
}

/// A placement the schedule cannot place. Emitted under `Auto` and
/// `ForceSequential` (under `ForceParallel` a shortfall escalates to the
/// whole-schedule [`ForceParallelRefused`]), reusing the plan vocabulary —
/// never a parallel one:
///
/// - [`Reason::BudgetRefuse`] when the #1243 budget binds, carrying the
///   CONFIGURED budget in `budget_bytes` (`plan_acquire` parity — never the
///   min'd effective limit).
/// - [`Reason::ForeignDuplicateNoCapacity`] when the pool binds behind a
///   user-loaded duplicate darkmux may not free (absolute ownership,
///   #1274), naming the instance, its bytes, and the pool headroom.
///
/// A pool-bound placement with NO foreign duplicate is never refused: it
/// goes alone in its own wave and the executor's #1139 fast-fail owns
/// physical shortfall (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveRefusal {
    pub placement: Placement,
    pub reason: Reason,
}

/// TOTAL-EQUALITY, DETERMINISTIC-ORDER wave schedule. `Serialize`-only,
/// like [`crate::plan::Plan`]: serialized schedules are run artifacts (the
/// #1285 knob-snapshot story), not re-executable input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveSchedule {
    /// Waves in execution order; within a wave, placements keep input order
    /// (the packing rule in the module docs). Already-resident sufficient
    /// models (Reuse decisions) cost zero additional bytes — their bytes
    /// are already in the un-evictable base — so first-fit lands them in
    /// wave 1.
    pub waves: Vec<Vec<Placement>>,
    /// Placements no wave can ever hold, in input order.
    pub refusals: Vec<WaveRefusal>,
    /// The mode that produced this schedule — self-describing artifact
    /// (operator sovereignty, #44: the operator never has to wonder why the
    /// shape is one-per-wave).
    pub mode: WaveMode,
    /// The co-residency ceiling the packing ran against: min(budget
    /// headroom, pool headroom) over the arms whose facts exist, after the
    /// surviving-reconcile stale credits. `None` = no known constraint
    /// (module docs). An artifact/packing field only — refusal reasons do
    /// NOT carry it: a budget refusal names the CONFIGURED #1243 budget and
    /// a pool-bound foreign-duplicate refusal names the pool headroom (the
    /// refusal-vocabulary division in the module docs).
    pub effective_limit_bytes: Option<u64>,
    /// Same emission vocabulary as [`crate::plan::Plan`] warnings. Order:
    /// per-placement estimate warnings first (input order), then budget-
    /// accounting warnings (host-reported order) — the planner's order.
    pub warnings: Vec<Warning>,
}

/// LOUD whole-schedule refusal of [`WaveMode::ForceParallel`]: the single
/// wave the operator demanded needs more than the effective limit.
///
/// A typed error rather than a schedule-with-a-blocking-outcome because a
/// refused ForceParallel has NO runnable subset by definition — returning a
/// schedule shape would invite callers to run whatever waves it carried,
/// which IS the silent fallback to sequential the operator decision on
/// #1285 forbids (it corrupts timing experiments). The `Result` makes
/// ignoring the refusal a compile-time decision, never a runtime oversight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForceParallelRefused {
    /// Sum of every placement's estimated bytes (Reuse placements add 0).
    pub need_bytes: u64,
    /// The effective limit the wave failed against.
    pub limit_bytes: u64,
}

impl fmt::Display for ForceParallelRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ForceParallel cannot fit: the single wave needs {} bytes against an effective \
             limit of {} bytes — refused; darkmux never silently falls back to sequential \
             (a silent realignment corrupts timing experiments, #1285). Rerun in Auto or \
             Sequential mode, raise the AI RAM budget (#1243), or shrink the staffing.",
            self.need_bytes, self.limit_bytes
        )
    }
}

impl std::error::Error for ForceParallelRefused {}

/// The two ceilings from one `removed` snapshot — the #1243 budget headroom
/// and the single-pool #1140 headroom, each `None` when its fact is absent.
/// Built exclusively from [`crate::planner`]'s shared accounting helpers:
/// one definition of the base and the freed credit.
struct Ceilings {
    budget_headroom: Option<u64>,
    pool_headroom: Option<u64>,
}

impl Ceilings {
    fn compute(facts: &Facts, removed: &BTreeSet<usize>) -> Self {
        Ceilings {
            budget_headroom: facts
                .budget
                .max_darkmux_bytes
                .map(|b| b.saturating_sub(resident_base(facts, removed))),
            pool_headroom: single_pool_headroom(facts, removed),
        }
    }

    /// The binding co-residency ceiling: min over the arms whose facts
    /// exist; `None` = no known constraint (module docs).
    fn effective(&self) -> Option<u64> {
        match (self.budget_headroom, self.pool_headroom) {
            (Some(b), Some(p)) => Some(b.min(p)),
            (b, p) => b.or(p),
        }
    }
}

/// THE pure wave scheduler (#1285): partition `placements` into co-resident
/// waves under the effective limit. See the module docs for the packing
/// rule, the limit accounting, and the refusal vocabulary/ordering; the
/// per-mode behavior is specified by the table tests below.
///
/// `Err` is possible ONLY under [`WaveMode::ForceParallel`] (the loud
/// whole-schedule refusal); `Auto` and `ForceSequential` always return a
/// schedule, carrying per-placement [`WaveRefusal`]s for what nothing can
/// ever hold.
pub fn plan_waves(
    placements: &[Placement],
    facts: &Facts,
    est: &dyn FootprintEstimator,
    mode: WaveMode,
) -> Result<WaveSchedule, ForceParallelRefused> {
    let mut warnings: Vec<Warning> = Vec::new();

    // ── classify + price each placement (input order) ────────────────────
    // A Reuse placement's bytes are already in the base ⇒ zero additional
    // cost. Reconcile stale credits are DEFERRED (the shared #1243/#1140
    // removal-timing accounting — module docs): a stale leaves the budget
    // base and joins the pool's freeable headroom only once its reconcile
    // survives every refusal pass. Unknown estimates count 0 and warn — the
    // planner's documented degradation, never a panic.
    let mut costs: Vec<u64> = Vec::with_capacity(placements.len());
    let mut stales: Vec<ReconcileStale> = Vec::new();
    // Placement index → the foreign duplicate (identifier, pool cost)
    // behind it: a pool-bound refusal names the instance (the planner's
    // #1140 arm vocabulary, absolute ownership #1274).
    let mut foreign_dups: BTreeMap<usize, (String, Option<u64>)> = BTreeMap::new();
    for (i, p) in placements.iter().enumerate() {
        match decide_residency(&facts.residents, p) {
            ResidencyDecision::Reuse { .. } => costs.push(0),
            decision => {
                match &decision {
                    ResidencyDecision::Reconcile { stale_identifier, .. } => {
                        stales.push(ReconcileStale::locate(facts, stale_identifier, i));
                    }
                    ResidencyDecision::ForeignDuplicate { foreign_identifier } => {
                        let bytes = facts
                            .residents
                            .iter()
                            .find(|r| r.identifier == *foreign_identifier)
                            .and_then(|r| r.est_bytes);
                        foreign_dups.insert(i, (foreign_identifier.clone(), bytes));
                    }
                    _ => {}
                }
                let e = est.estimate_bytes(&p.model_key, p.min_ctx, facts.catalog.as_deref());
                if e.is_none() {
                    warnings.push(Warning::LoadEstimateUnknown { model_key: p.model_key.clone() });
                }
                costs.push(e.unwrap_or(0));
            }
        }
    }

    let budget = facts.budget.max_darkmux_bytes;
    if budget.is_some() {
        warn_unknown_owned_resident_bytes(&mut warnings, facts);
    }

    // ── ForceParallel: whole-schedule arithmetic, no per-placement refusals ─
    // Every reconcile in the single demanded wave executes (its unload half
    // precedes the loads), so every stale credit commits.
    if mode == WaveMode::ForceParallel {
        let mut removed: BTreeSet<usize> = BTreeSet::new();
        commit_surviving_stales(&stales, &mut removed, |_| true);
        let limit = Ceilings::compute(facts, &removed).effective();
        let need: u64 = costs.iter().sum();
        if let Some(l) = limit.filter(|&l| need > l) {
            return Err(ForceParallelRefused { need_bytes: need, limit_bytes: l });
        }
        let waves: Vec<Vec<Placement>> =
            if placements.is_empty() { Vec::new() } else { vec![placements.to_vec()] };
        return Ok(WaveSchedule {
            waves,
            refusals: Vec::new(),
            mode,
            effective_limit_bytes: limit,
            warnings,
        });
    }

    // ── refusal passes (Auto + ForceSequential): plan_acquire's ordering ──
    // Flat refusals first against the un-credited accounting, then commit
    // the stale credits of surviving reconciles, then the fit pass against
    // the updated base (module docs). A refused reconcile keeps its stale
    // loaded and counted — refusal is non-destructive.
    let mut refused: Vec<Option<Reason>> = vec![None; placements.len()];

    // Flat half: an estimate exceeding the WHOLE #1243 budget can never
    // fit, whatever frees happen (plan_acquire's flat pass; both refusal
    // halves carry the CONFIGURED budget, never the min'd effective limit).
    if let Some(b) = budget {
        for (i, &cost) in costs.iter().enumerate() {
            if cost > b {
                refused[i] = Some(Reason::BudgetRefuse { est_bytes: cost, budget_bytes: b });
            }
        }
    }

    // Stale credits for surviving reconciles only — the shared removal-
    // timing helper (one implementation of the #1243/#1140 rule, not two).
    let mut removed: BTreeSet<usize> = BTreeSet::new();
    commit_surviving_stales(&stales, &mut removed, |i| refused[i].is_none());

    let ceilings = Ceilings::compute(facts, &removed);
    let limit = ceilings.effective();

    // Fit half: the scheduler plans no evictions, so the darkmux base that
    // remains after the committed frees is un-evictable — a cost that
    // cannot fit the budget atop it is refused (plan_acquire's
    // post-eviction refusal, naming the configured budget).
    if let (Some(b), Some(h)) = (budget, ceilings.budget_headroom) {
        for (i, &cost) in costs.iter().enumerate() {
            if refused[i].is_none() && cost > h {
                refused[i] = Some(Reason::BudgetRefuse { est_bytes: cost, budget_bytes: b });
            }
        }
    }

    // Pool half: a survivor exceeding the effective limit is pool-bound
    // (every budget-bound cost was refused above, so here the pool arm
    // produced the limit), and only a foreign duplicate is ever
    // pool-refused — its bytes are the pressure darkmux may not free
    // (absolute ownership, #1274; the planner's #1140 arm). Non-foreign
    // pool-bound placements proceed alone in their own wave below; the
    // executor's #1139 fast-fail owns physical shortfall (module docs).
    if let Some(l) = limit {
        for (i, &cost) in costs.iter().enumerate() {
            let Some((fid, fbytes)) = foreign_dups.get(&i) else { continue };
            if refused[i].is_none() && cost > l {
                refused[i] = Some(Reason::ForeignDuplicateNoCapacity {
                    foreign_identifier: fid.clone(),
                    foreign_bytes: *fbytes,
                    est_bytes: cost,
                    limit_bytes: l,
                });
            }
        }
    }

    // ── partition per mode ───────────────────────────────────────────────
    let mut waves: Vec<Vec<Placement>> = Vec::new();
    let mut refusals: Vec<WaveRefusal> = Vec::new();
    match mode {
        WaveMode::ForceParallel => {
            unreachable!("ForceParallel returned its whole-schedule result above")
        }
        WaveMode::ForceSequential => {
            for (i, p) in placements.iter().enumerate() {
                if let Some(reason) = refused[i].take() {
                    refusals.push(WaveRefusal { placement: p.clone(), reason });
                    continue;
                }
                waves.push(vec![p.clone()]);
            }
        }
        WaveMode::Auto => {
            // Running per-wave totals for the first-fit walk.
            let mut loads: Vec<u64> = Vec::new();
            for (i, p) in placements.iter().enumerate() {
                if let Some(reason) = refused[i].take() {
                    refusals.push(WaveRefusal { placement: p.clone(), reason });
                    continue;
                }
                let cost = costs[i];
                let Some(l) = limit else {
                    // No known constraint — one wave, executor backstops.
                    // (No refusals exist here: every refusal requires a
                    // budget or pool fact, which would make the limit Some.)
                    if waves.is_empty() {
                        waves.push(Vec::new());
                    }
                    waves[0].push(p.clone());
                    continue;
                };
                if cost > l {
                    // Pool-bound survivor (refusal division, module docs):
                    // alone in its own wave — its load alone exceeds the
                    // limit, so first-fit never adds a companion — with the
                    // executor's #1139 backstop owning the shortfall.
                    waves.push(vec![p.clone()]);
                    loads.push(cost);
                    continue;
                }
                // First-fit in input order (the packing rule, module docs).
                match loads.iter().position(|&w| w + cost <= l) {
                    Some(w_idx) => {
                        waves[w_idx].push(p.clone());
                        loads[w_idx] += cost;
                    }
                    None => {
                        waves.push(vec![p.clone()]);
                        loads.push(cost);
                    }
                }
            }
        }
    }

    Ok(WaveSchedule { waves, refusals, mode, effective_limit_bytes: limit, warnings })
}

#[cfg(test)]
mod tests {
    //! The wave table: one row per #1285 requirement, priced with the REAL
    //! measured potentials from the 2026-07-10 M5 Max probes on #1286 —
    //! judge 19.0GB@65k / devstral 18.75GB@32k / 4B 7.25GB@32k /
    //! 9B 7.35GB@32k. Every row is `(placements, Facts, mode)` in, a
    //! totally-Eq [`WaveSchedule`] out.

    use super::*;
    use crate::estimator::{ArchEstimator, ArchFacts, FixedEstimator};
    use crate::facts::{Budget, CatalogFact, Facts, PoolFact, PoolId, Pools, ResidentFact};
    use std::collections::BTreeMap;

    const GB: u64 = 1_000_000_000;
    // The #1286 probed potentials (weights + KV at profile ctx + margin).
    const JUDGE_65K: u64 = 19 * GB; // 19.0GB@65k
    const DEVSTRAL_32K: u64 = 18_750_000_000; // 18.75GB@32k
    const QWEN_4B_32K: u64 = 7_250_000_000; // 7.25GB@32k
    const QWEN_9B_32K: u64 = 7_350_000_000; // 7.35GB@32k

    fn placement(model_key: &str, min_ctx: u32) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: format!("darkmux:{model_key}"),
            min_ctx,
            seat: "test".to_string(),
        }
    }

    fn aliased(model_key: &str, min_ctx: u32, alias: &str) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: alias.to_string(),
            min_ctx,
            seat: "test".to_string(),
        }
    }

    fn resident(identifier: &str, model_key: &str, ctx: u64, est_bytes: Option<u64>) -> ResidentFact {
        ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx,
            est_bytes,
        }
    }

    fn budget_gb(gb: u64) -> Facts {
        Facts { budget: Budget { max_darkmux_bytes: Some(gb * GB) }, ..Default::default() }
    }

    fn est_map(pairs: &[(&str, u64)]) -> FixedEstimator {
        FixedEstimator(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    /// The probed-potential estimator every measured row uses.
    fn probed() -> FixedEstimator {
        est_map(&[
            ("judge", JUDGE_65K),
            ("devstral", DEVSTRAL_32K),
            ("qwen-4b", QWEN_4B_32K),
            ("qwen-9b", QWEN_9B_32K),
        ])
    }

    fn single_pool(available: u64) -> Pools {
        BTreeMap::from([(
            PoolId("unified".into()),
            PoolFact { capacity_bytes: 128 * GB, available_bytes: available },
        )])
    }

    // ── the required #1285 rows ──────────────────────────────────────────

    #[test]
    fn auto_splits_two_waves_when_sum_exceeds_budget() {
        // devstral 18.75GB + 4B 7.25GB = 26GB > the 24GB budget (the
        // 32GB-tier emulation setting) — two waves, input order.
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &budget_gb(24), &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(24 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn auto_single_wave_when_everything_fits() {
        // judge + devstral + 4B = 45.0GB under a 96GB budget — one wave,
        // input order.
        let placements = vec![
            placement("judge", 65_536),
            placement("devstral", 32_768),
            placement("qwen-4b", 32_768),
        ];
        let schedule = plan_waves(&placements, &budget_gb(96), &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(96 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn auto_refuses_model_bigger_than_budget() {
        // A 27GB estimate alone exceeds the 24GB budget: no wave composition
        // can ever hold it — a typed refusal naming the budget, reusing
        // Reason::BudgetRefuse (no parallel vocabulary).
        let placements = vec![placement("big-27b", 32_768)];
        let schedule = plan_waves(
            &placements,
            &budget_gb(24),
            &est_map(&[("big-27b", 27 * GB)]),
            WaveMode::Auto,
        )
        .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![],
                refusals: vec![WaveRefusal {
                    placement: placements[0].clone(),
                    reason: Reason::BudgetRefuse { est_bytes: 27 * GB, budget_bytes: 24 * GB },
                }],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(24 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn force_parallel_under_capacity_refuses_loud() {
        // The operator demanded one wave that cannot fit (26GB > 24GB):
        // typed whole-schedule refusal — NEVER a silent fallback to
        // sequential (#1285: a silent realignment corrupts timing
        // experiments).
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let err = plan_waves(&placements, &budget_gb(24), &probed(), WaveMode::ForceParallel)
            .unwrap_err();
        assert_eq!(
            err,
            ForceParallelRefused { need_bytes: 26 * GB, limit_bytes: 24 * GB }
        );
        let s = err.to_string();
        assert!(s.contains("never silently falls back to sequential"), "{s}");
    }

    #[test]
    fn force_parallel_fits_single_wave() {
        // The override under capacity: one wave of everything.
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &budget_gb(96), &probed(), WaveMode::ForceParallel)
            .expect("26GB fits the 96GB budget");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::ForceParallel,
                effective_limit_bytes: Some(96 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn force_parallel_exact_fit_proceeds() {
        // Equality edge on ForceParallel's OWN comparison site (a different
        // site from Auto's first-fit `<=`): the demanded single wave needing
        // EXACTLY the effective limit (18.75 + 7.25 = 26GB) proceeds. A
        // `need > limit` flipped to `>=` inverts this row while the
        // wide-margin force_parallel_fits_single_wave row stays green.
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &budget_gb(26), &probed(), WaveMode::ForceParallel)
            .expect("need == limit is a fit, not a refusal");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::ForceParallel,
                effective_limit_bytes: Some(26 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn force_sequential_one_placement_per_wave() {
        // The other override: one placement per wave, in input order, even
        // when everything would co-fit.
        let placements = vec![placement("devstral", 32_768), placement("qwen-9b", 32_768)];
        let schedule =
            plan_waves(&placements, &budget_gb(96), &probed(), WaveMode::ForceSequential)
                .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::ForceSequential,
                effective_limit_bytes: Some(96 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn exact_fit_is_one_wave() {
        // Equality edge on the strict comparison: devstral 18.75GB + 4B
        // 7.25GB sum to EXACTLY a 26GB budget — one wave. A `<=` flipped to
        // `<` in the packing inverts this row while every wide-margin row
        // stays green (the planner's exact-fit discipline).
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &budget_gb(26), &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(schedule.waves, vec![placements]);
        assert_eq!(schedule.refusals, vec![]);
    }

    // ── packing-rule rows ────────────────────────────────────────────────

    #[test]
    fn first_fit_backfills_earlier_waves() {
        // The #1285 issue's own example: budget 24GB, footprints
        // [18.5, 14, 2.3] → waves [[18.5, 2.3], [14]] — the 2.3GB third
        // placement backfills wave 1 (first-fit), and input order is kept
        // WITHIN each wave.
        let placements =
            vec![placement("a", 8_000), placement("b", 8_000), placement("c", 8_000)];
        let est = est_map(&[("a", 18_500_000_000), ("b", 14 * GB), ("c", 2_300_000_000)]);
        let schedule = plan_waves(&placements, &budget_gb(24), &est, WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule.waves,
            vec![
                vec![placements[0].clone(), placements[2].clone()],
                vec![placements[1].clone()],
            ]
        );
    }

    #[test]
    fn reuse_rides_wave_one_at_zero_cost() {
        // An already-resident sufficient model (a Reuse decision) costs zero
        // additional bytes — its 19GB is already in the un-evictable base
        // (which is why the headroom is 44 − 19 = 25GB, not 44GB) — so it
        // belongs to wave 1 alongside whatever else fits.
        let f = Facts {
            residents: vec![resident("darkmux:judge", "judge", 65_536, Some(JUDGE_65K))],
            budget: Budget { max_darkmux_bytes: Some(44 * GB) },
            ..Default::default()
        };
        let placements = vec![
            placement("judge", 65_536),
            placement("devstral", 32_768),
            placement("qwen-4b", 32_768),
        ];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                // devstral (18.75) joins the zero-cost judge in wave 1; the
                // 4B would push wave 1 to 26GB > the 25GB headroom → wave 2.
                waves: vec![
                    vec![placements[0].clone(), placements[1].clone()],
                    vec![placements[2].clone()],
                ],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(25 * GB),
                warnings: vec![],
            }
        );
    }

    // ── limit-accounting rows ────────────────────────────────────────────

    #[test]
    fn foreign_residents_shrink_pool_headroom_never_budget() {
        // Absolute ownership (#1274): the user-loaded 20GB resident is pool
        // consumption only — it already depresses available_bytes at probe
        // time and never counts against the #1243 budget (base stays 0, so
        // budget headroom is the full 30GB). The pool's 20GB is the binding
        // ceiling: waves shrink accordingly, and the foreign instance is
        // never named anywhere in the schedule.
        let f = Facts {
            residents: vec![resident("user-model", "user-model", 8_000, Some(20 * GB))],
            pools: single_pool(20 * GB),
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(20 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn reconcile_stale_frees_into_the_limit() {
        // plan_acquire accounting parity: a SURVIVING reconcile's stale
        // resident leaves the budget base AND its bytes join the pool's
        // freeable headroom (it is freed before the reload). Base 0 →
        // budget headroom 24GB; pool 5GB + 18.75GB freed = 23.75GB is the
        // binding min — and the reload fits in one wave. Without the
        // removal the base would be 18.75GB (headroom 5.25GB) and this row
        // would refuse.
        let f = Facts {
            residents: vec![resident("darkmux:devstral", "devstral", 4_096, Some(DEVSTRAL_32K))],
            pools: single_pool(5 * GB),
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("devstral", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(5 * GB + DEVSTRAL_32K),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn refused_reconcile_stays_counted_no_phantom_headroom() {
        // The review's empirical MUST_FIX scenario (#1285): an explicit
        // un-namespaced alias resident "devstral" at ctx 4096 (18.75GB),
        // single pool at 5GB available, placements
        // [devstral@262144 (27GB), qwen-4b (7.25GB)], Auto.
        //
        // The pre-fix draft committed the reconcile's stale credit during
        // classification — BEFORE refusal decisions — so the 27GB reconcile
        // was refused against a 23.75GB limit while its 18.75GB "freed"
        // credit persisted, and the 4B was packed against headroom that did
        // not physically exist (real available: 5GB < 7.25GB).
        //
        // The corrected accounting (shared with plan_acquire: flat refusals
        // first, credits committed only for surviving reconciles) yields
        // the HONEST outcome documented here — alone-in-wave with a real
        // limit: under the refusal division (module docs) the pool never
        // refuses a non-foreign placement, so the 27GB reconcile is NOT
        // refused; it goes ALONE in wave 1, where its own unload half
        // executes first, making the 5 + 18.75 = 23.75GB limit physically
        // real (the executor's #1139 fast-fail owns the 27 > 23.75
        // shortfall). The 4B lands in wave 2, after the wave-1 free has
        // genuinely happened — never against phantom headroom.
        let f = Facts {
            residents: vec![resident("devstral", "devstral", 4_096, Some(DEVSTRAL_32K))],
            pools: single_pool(5 * GB),
            ..Default::default()
        };
        let placements = vec![
            aliased("devstral", 262_144, "devstral"),
            placement("qwen-4b", 32_768),
        ];
        let est = est_map(&[("devstral", 27 * GB), ("qwen-4b", QWEN_4B_32K)]);
        let schedule = plan_waves(&placements, &f, &est, WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(5 * GB + DEVSTRAL_32K),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn budget_refused_reconcile_keeps_stale_counted() {
        // plan_acquire's budget_refused_reconcile_keeps_stale_resident row,
        // on the scheduler's accounting: the devstral reconcile at 262k
        // (27GB) exceeds the WHOLE 24GB budget — flat-refused, so its
        // 18.75GB stale credit never commits (refusal is non-destructive:
        // the stale stays loaded, and stays counted). The 4B is then
        // honestly refused too: the un-evictable base is still 18.75GB,
        // headroom 5.25GB < 7.25GB. Both refusals carry the CONFIGURED
        // 24GB budget (plan_acquire parity), never the min'd effective
        // limit. Pre-fix, the phantom credit let the 4B schedule against
        // 24GB of headroom while the stale stayed loaded.
        let f = Facts {
            residents: vec![resident("darkmux:devstral", "devstral", 4_096, Some(DEVSTRAL_32K))],
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("devstral", 262_144), placement("qwen-4b", 32_768)];
        let est = est_map(&[("devstral", 27 * GB), ("qwen-4b", QWEN_4B_32K)]);
        let schedule = plan_waves(&placements, &f, &est, WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![],
                refusals: vec![
                    WaveRefusal {
                        placement: placements[0].clone(),
                        reason: Reason::BudgetRefuse {
                            est_bytes: 27 * GB,
                            budget_bytes: 24 * GB,
                        },
                    },
                    WaveRefusal {
                        placement: placements[1].clone(),
                        reason: Reason::BudgetRefuse {
                            est_bytes: QWEN_4B_32K,
                            budget_bytes: 24 * GB,
                        },
                    },
                ],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(24 * GB - DEVSTRAL_32K),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn budget_refusal_names_configured_budget_not_effective_limit() {
        // BudgetRefuse.budget_bytes parity (#1243): plan_acquire's refusals
        // always carry the CONFIGURED budget; the scheduler's must too.
        // Here the un-evictable 14GB base (the scheduler plans no
        // evictions) leaves 10GB of headroom under the 24GB budget — the
        // 12GB placement is refused, and the refusal names 24GB, not the
        // min'd 10GB the packing ran against (that number stays on
        // effective_limit_bytes).
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, Some(14 * GB))],
            pools: single_pool(30 * GB),
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("mid", 8_000)];
        let schedule =
            plan_waves(&placements, &f, &est_map(&[("mid", 12 * GB)]), WaveMode::Auto)
                .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![],
                refusals: vec![WaveRefusal {
                    placement: placements[0].clone(),
                    reason: Reason::BudgetRefuse { est_bytes: 12 * GB, budget_bytes: 24 * GB },
                }],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(10 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn foreign_duplicate_pool_bound_names_the_instance() {
        // Vocabulary parity with the planner's #1140 pool arm (#1274
        // absolute ownership): a placement colliding with a user-loaded
        // duplicate that cannot fit alongside it is refused as
        // ForeignDuplicateNoCapacity — naming the instance, its bytes, and
        // the pool headroom darkmux may not grow by eviction — never an
        // anonymous BudgetRefuse.
        let f = Facts {
            residents: vec![resident("devstral-manual", "devstral", 40_960, Some(20 * GB))],
            pools: single_pool(5 * GB),
            ..Default::default()
        };
        let placements = vec![placement("devstral", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![],
                refusals: vec![WaveRefusal {
                    placement: placements[0].clone(),
                    reason: Reason::ForeignDuplicateNoCapacity {
                        foreign_identifier: "devstral-manual".into(),
                        foreign_bytes: Some(20 * GB),
                        est_bytes: DEVSTRAL_32K,
                        limit_bytes: 5 * GB,
                    },
                }],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(5 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn pool_bound_placement_rides_alone_never_refused() {
        // The pool half of the refusal division (module docs): pool facts
        // are advisory headroom, not an operator contract — a non-foreign
        // placement exceeding the pool headroom is NOT refused
        // (plan_acquire's #1140 arm never refuses this shortfall either);
        // it goes alone in its own wave and the executor's #1139
        // insufficient-resources fast-fail owns the physical shortfall.
        // Placements after it still pack normally.
        let f = Facts { pools: single_pool(10 * GB), ..Default::default() };
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(10 * GB),
                warnings: vec![],
            }
        );
    }

    #[test]
    fn multiple_pools_skip_the_pool_arm() {
        // The single-pool v1 rule, shared with the planner's #1140 arm:
        // with two pools the pool ceiling is skipped (a placement→pool
        // mapping fact arrives with a second ResourceProbe) and the budget
        // alone binds.
        let pools: Pools = BTreeMap::from([
            (
                PoolId("system-ram".into()),
                PoolFact { capacity_bytes: 64 * GB, available_bytes: 10 * GB },
            ),
            (
                PoolId("gpu0-vram".into()),
                PoolFact { capacity_bytes: 24 * GB, available_bytes: 10 * GB },
            ),
        ]);
        let f = Facts {
            pools,
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(schedule.effective_limit_bytes, Some(24 * GB));
        assert_eq!(
            schedule.waves,
            vec![vec![placements[0].clone()], vec![placements[1].clone()]]
        );
    }

    #[test]
    fn no_known_constraint_is_one_wave() {
        // No budget, no pool facts: nothing to partition against — one wave,
        // the executor's #1139 fast-fail backstops (plan_acquire parity).
        let placements = vec![placement("judge", 65_536), placement("devstral", 32_768)];
        let schedule = plan_waves(&placements, &Facts::default(), &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: None,
                warnings: vec![],
            }
        );
    }

    // ── degradation rows (the planner's loud-never-silent discipline) ────

    #[test]
    fn unknown_estimate_counts_zero_and_warns() {
        // An unpriceable placement schedules at cost 0 with the planner's
        // LoadEstimateUnknown warning — documented degradation, never a
        // panic, never a silent drop.
        let placements = vec![placement("devstral", 32_768), placement("mystery", 8_000)];
        let schedule = plan_waves(&placements, &budget_gb(24), &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![placements.clone()],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(24 * GB),
                warnings: vec![Warning::LoadEstimateUnknown { model_key: "mystery".into() }],
            }
        );
    }

    #[test]
    fn resident_bytes_unknown_warns_under_budget() {
        // Same accounting degradation as the planner's budget arm (shared
        // helper): an unknown-size darkmux resident counts 0 against the
        // base, loudly.
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, None)],
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &f, &probed(), WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule.warnings,
            vec![Warning::ResidentBytesUnknown { identifier: "darkmux:idle".into() }]
        );
        assert_eq!(schedule.effective_limit_bytes, Some(24 * GB));
        assert_eq!(schedule.waves, vec![placements]);
    }

    #[test]
    fn force_sequential_still_refuses_impossible_placement() {
        // The override changes the packing, never the physics: a placement
        // whose estimate alone exceeds the budget cannot run even in its
        // own wave — same typed refusal as Auto; the rest still schedules.
        let placements = vec![placement("big-27b", 32_768), placement("qwen-4b", 32_768)];
        let est = est_map(&[("big-27b", 27 * GB), ("qwen-4b", QWEN_4B_32K)]);
        let schedule = plan_waves(&placements, &budget_gb(24), &est, WaveMode::ForceSequential)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(schedule.waves, vec![vec![placements[1].clone()]]);
        assert_eq!(
            schedule.refusals,
            vec![WaveRefusal {
                placement: placements[0].clone(),
                reason: Reason::BudgetRefuse { est_bytes: 27 * GB, budget_bytes: 24 * GB },
            }]
        );
    }

    // ── #1286 composition row ────────────────────────────────────────────

    /// devstral 24B dense — the estimator's probed 2026-07-10 M5 Max row
    /// (see estimator.rs): all 40 layers full attention, kv_heads 8,
    /// head_dim 128, fp16 cache — 160 KB/token.
    fn devstral_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 40,
            full_attention_layers: 40,
            kv_heads: 8,
            head_dim: 128,
            kv_bytes_per_element: 2,
        }
    }

    /// 4B dense — the estimator's probed row: all 36 layers full attention,
    /// kv_heads 8, head_dim 128, fp16 cache — 144 KB/token.
    fn qwen_4b_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 36,
            full_attention_layers: 36,
            kv_heads: 8,
            head_dim: 128,
            kv_bytes_per_element: 2,
        }
    }

    #[test]
    fn arch_estimator_composes_into_wave_split() {
        // The advertised #1285 × #1286 composition, end-to-end: REAL arch
        // facts (the estimator's probed devstral/4B rows) + a catalog,
        // wired into plan_waves under the 24GB budget (the 32GB-tier
        // emulation setting). The ARCH-derived potentials — devstral 13GB
        // weights + 5.369GB KV @32k + 0.75GB margin = 19.119GB; 4B 2.3GB +
        // 4.832GB KV + 0.75GB = 7.882GB ('4B doesn't mean 4GB') — sum to
        // 27.0GB > 24GB, so the two-wave split arises from the architecture
        // numbers alone, no FixedEstimator anywhere.
        let arch = ArchEstimator::new(BTreeMap::from([
            ("devstral".to_string(), devstral_arch()),
            ("qwen-4b".to_string(), qwen_4b_arch()),
        ]));
        let f = Facts {
            catalog: Some(vec![
                CatalogFact { model_key: "devstral".into(), size_bytes: Some(13_000_000_000) },
                CatalogFact { model_key: "qwen-4b".into(), size_bytes: Some(2_300_000_000) },
            ]),
            budget: Budget { max_darkmux_bytes: Some(24 * GB) },
            ..Default::default()
        };
        let placements = vec![placement("devstral", 32_768), placement("qwen-4b", 32_768)];
        let schedule = plan_waves(&placements, &f, &arch, WaveMode::Auto)
            .expect("only ForceParallel refuses the whole schedule");
        assert_eq!(
            schedule,
            WaveSchedule {
                waves: vec![vec![placements[0].clone()], vec![placements[1].clone()]],
                refusals: vec![],
                mode: WaveMode::Auto,
                effective_limit_bytes: Some(24 * GB),
                warnings: vec![],
            }
        );
    }

    // ── cross-cutting invariants ─────────────────────────────────────────

    #[test]
    fn empty_placements_schedule_no_waves() {
        // No placements ⇒ no waves, under every mode (no empty wave is ever
        // emitted).
        for mode in [WaveMode::Auto, WaveMode::ForceParallel, WaveMode::ForceSequential] {
            let schedule = plan_waves(&[], &budget_gb(24), &probed(), mode)
                .expect("an empty wave never exceeds any limit");
            assert_eq!(schedule.waves, Vec::<Vec<Placement>>::new(), "{mode:?}");
        }
    }

    #[test]
    fn schedule_total_equality_determinism() {
        // The property every other row relies on: same input ⇒ identical
        // WaveSchedule, across a fixture with residents, a budget, and a
        // pool.
        let f = Facts {
            residents: vec![
                resident("darkmux:judge", "judge", 65_536, Some(JUDGE_65K)),
                resident("user-model", "user-model", 8_000, Some(20 * GB)),
            ],
            pools: single_pool(40 * GB),
            budget: Budget { max_darkmux_bytes: Some(44 * GB) },
            ..Default::default()
        };
        let placements = vec![
            placement("judge", 65_536),
            placement("devstral", 32_768),
            placement("qwen-4b", 32_768),
        ];
        let a = plan_waves(&placements, &f, &probed(), WaveMode::Auto);
        let b = plan_waves(&placements.clone(), &f.clone(), &probed(), WaveMode::Auto);
        assert_eq!(a, b);
    }

    #[test]
    fn schedule_serializes_for_artifacts() {
        // Serialize-only, like Plan: a schedule can land in a run artifact;
        // the absence of Deserialize on WaveSchedule is a compile-time
        // property.
        let schedule = plan_waves(
            &[placement("devstral", 32_768)],
            &budget_gb(24),
            &probed(),
            WaveMode::Auto,
        )
        .expect("only ForceParallel refuses the whole schedule");
        let json = serde_json::to_string(&schedule).expect("schedule serializes");
        assert!(json.contains("darkmux:devstral"), "{json}");
    }
}
