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
//! the same accounting `plan_acquire` uses (shared helpers, one definition):
//!
//! - **Budget headroom** (#1243): `budget − un-evictable darkmux base`. The
//!   base is [`crate::planner`]'s resident accounting — darkmux-owned
//!   resident bytes, minus reconcile stales (freed before their reload).
//!   The scheduler plans no evictions (that is `plan_acquire`'s job), so
//!   every darkmux-owned resident it cannot prove leaves is base. Residents
//!   a placement Reuses are base too — which is exactly why a Reuse costs
//!   zero additional bytes below.
//! - **Pool headroom** (single-pool v1 rule, as in the planner's #1140 arm;
//!   zero or multiple pools skip the arm): the pool's `available_bytes`
//!   snapshot plus reconcile-stale bytes freed before loads. Foreign
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

use crate::desired::Placement;
use crate::estimator::FootprintEstimator;
use crate::facts::Facts;
use crate::plan::{Reason, Warning};
use crate::planner::{resident_base, warn_unknown_owned_resident_bytes};
use crate::residency::{decide_residency, ResidencyDecision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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

/// A placement the schedule cannot place: its estimate ALONE exceeds the
/// effective limit, so no wave composition can ever hold it. Emitted under
/// `Auto` and `ForceSequential` (under `ForceParallel` the refusal
/// escalates to the whole-schedule [`ForceParallelRefused`]). Reuses the
/// plan vocabulary — [`Reason::BudgetRefuse`] with the effective limit in
/// `budget_bytes` — never a parallel refusal vocabulary.
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
    /// The binding ceiling: min(budget headroom, pool headroom) over the
    /// arms whose facts exist. `None` = no known constraint (module docs).
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

/// THE pure wave scheduler (#1285): partition `placements` into co-resident
/// waves under the effective limit. See the module docs for the packing
/// rule and the limit accounting; the per-mode behavior is specified by the
/// table tests below.
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
    // cost. Reconcile stales leave residency before their reload
    // (plan_acquire parity): their indexes leave the budget base and their
    // bytes join the pool's freeable headroom. Unknown estimates count 0
    // and warn — the planner's documented degradation, never a panic.
    let mut removed: BTreeSet<usize> = BTreeSet::new();
    let mut costs: Vec<u64> = Vec::with_capacity(placements.len());
    for p in placements {
        match decide_residency(&facts.residents, p) {
            ResidencyDecision::Reuse { .. } => costs.push(0),
            decision => {
                if let ResidencyDecision::Reconcile { stale_identifier, .. } = &decision {
                    let idx = facts
                        .residents
                        .iter()
                        .position(|r| r.identifier == *stale_identifier)
                        .expect("reconcile stale is a reported resident");
                    removed.insert(idx);
                }
                let e = est.estimate_bytes(&p.model_key, p.min_ctx, facts.catalog.as_deref());
                if e.is_none() {
                    warnings.push(Warning::LoadEstimateUnknown { model_key: p.model_key.clone() });
                }
                costs.push(e.unwrap_or(0));
            }
        }
    }

    // ── the effective limit (module docs) ────────────────────────────────
    let mut budget_headroom: Option<u64> = None;
    if let Some(budget) = facts.budget.max_darkmux_bytes {
        warn_unknown_owned_resident_bytes(&mut warnings, facts);
        budget_headroom = Some(budget.saturating_sub(resident_base(facts, &removed)));
    }
    let pool_headroom: Option<u64> = if facts.pools.len() == 1 {
        let freed: u64 =
            removed.iter().map(|&idx| facts.residents[idx].est_bytes.unwrap_or(0)).sum();
        facts.pools.values().next().map(|pool| pool.available_bytes + freed)
    } else {
        None
    };
    let limit = match (budget_headroom, pool_headroom) {
        (Some(b), Some(p)) => Some(b.min(p)),
        (b, p) => b.or(p),
    };

    // ── partition per mode ───────────────────────────────────────────────
    let mut waves: Vec<Vec<Placement>> = Vec::new();
    let mut refusals: Vec<WaveRefusal> = Vec::new();
    match mode {
        WaveMode::ForceParallel => {
            let need: u64 = costs.iter().sum();
            if let Some(l) = limit.filter(|&l| need > l) {
                return Err(ForceParallelRefused { need_bytes: need, limit_bytes: l });
            }
            if !placements.is_empty() {
                waves.push(placements.to_vec());
            }
        }
        WaveMode::ForceSequential => {
            for (p, &cost) in placements.iter().zip(&costs) {
                if let Some(l) = limit.filter(|&l| cost > l) {
                    refusals.push(refuse(p, cost, l));
                    continue;
                }
                waves.push(vec![p.clone()]);
            }
        }
        WaveMode::Auto => {
            // Running per-wave totals for the first-fit walk.
            let mut loads: Vec<u64> = Vec::new();
            for (p, &cost) in placements.iter().zip(&costs) {
                let Some(l) = limit else {
                    // No known constraint — one wave, executor backstops.
                    if waves.is_empty() {
                        waves.push(Vec::new());
                    }
                    waves[0].push(p.clone());
                    continue;
                };
                if cost > l {
                    refusals.push(refuse(p, cost, l));
                    continue;
                }
                // First-fit in input order (the packing rule, module docs).
                match loads.iter().position(|&w| w + cost <= l) {
                    Some(i) => {
                        waves[i].push(p.clone());
                        loads[i] += cost;
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

/// Per-placement refusal: the estimate alone exceeds the effective limit.
/// When the pool is the binding ceiling this rides the budget vocabulary
/// with the pool-derived limit in `budget_bytes` — exactly the convention
/// of the planner's #1140 arm ([`Reason::BudgetEvict`] carries the pool
/// snapshot there); no parallel vocabulary.
fn refuse(p: &Placement, est_bytes: u64, limit_bytes: u64) -> WaveRefusal {
    WaveRefusal {
        placement: p.clone(),
        reason: Reason::BudgetRefuse { est_bytes, budget_bytes: limit_bytes },
    }
}

#[cfg(test)]
mod tests {
    //! The wave table: one row per #1285 requirement, priced with the REAL
    //! measured potentials from the 2026-07-10 M5 Max probes on #1286 —
    //! judge 19.0GB@65k / devstral 18.75GB@32k / 4B 7.25GB@32k /
    //! 9B 7.35GB@32k. Every row is `(placements, Facts, mode)` in, a
    //! totally-Eq [`WaveSchedule`] out.

    use super::*;
    use crate::estimator::FixedEstimator;
    use crate::facts::{Budget, Facts, PoolFact, PoolId, Pools, ResidentFact};
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
        // plan_acquire accounting parity: a reconcile's stale resident
        // leaves the budget base AND its bytes join the pool's freeable
        // headroom (it is freed before the reload). Base 0 → budget
        // headroom 24GB; pool 5GB + 18.75GB freed = 23.75GB is the binding
        // min — and the reload fits in one wave. Without the removal the
        // base would be 18.75GB (headroom 5.25GB) and this row would
        // refuse.
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
        // whose estimate alone exceeds the limit cannot run even in its own
        // wave — same typed refusal as Auto; the rest still schedules.
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
