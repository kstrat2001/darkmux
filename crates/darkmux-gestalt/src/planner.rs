//! The pure planner (#1274): deterministic and total — same
//! `(desired, facts, opts, estimator)` ⇒ identical [`Plan`] (`Eq`), always.
//!
//! Zero I/O, zero clock reads, zero env reads. Composes
//! [`decide_residency`] per placement, then the #1243 budget pass and the
//! #1140 pool-headroom pass, then orders actions per the [`Plan`] ordering
//! contract. Every placement — primary, utility, probe, judge — goes through
//! the SAME path (#1280: no seat is exempt).

use crate::desired::Placement;
use crate::estimator::FootprintEstimator;
use crate::facts::{CallerIntent, CatalogFact, Facts};
use crate::ownership::{ctx_sufficient, is_darkmux_owned};
use crate::plan::{
    Action, EvictionOrder, ExecHint, OwnedTarget, Plan, PlannedAction, Precondition, Reason,
    Warning,
};
use crate::residency::{decide_residency, ResidencyDecision};
use std::collections::BTreeSet;

/// Acquisition options. Deliberately NO `Default` impl: every caller chooses
/// its intent, scope, and foreign policy explicitly — a defaulted foreign
/// policy would silently pick one of two existing production behaviors
/// (see [`ForeignPolicy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireOpts {
    pub intent: CallerIntent,
    pub scope: AcquireScope,
    pub foreign_policy: ForeignPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireScope {
    /// The `swap::swap` shape: darkmux-owned residents NOT in the desired
    /// set get `Unload(NoLongerDesired)` — pass 1, before loads (the
    /// RAM-headroom two-pass order).
    Exclusive,
    /// The ensure-loaded / cycler shape: touch only the desired placements;
    /// other darkmux residents are left alone.
    Additive,
}

/// What to do when a FOREIGN resident holds the desired weights. Both
/// variants preserve an existing production behavior — the cutover map's
/// verified finding is that the funnel Blocks on foreign while the crew
/// dispatch path adopts (reuses any sufficient resident and reconciles even
/// a foreign undersized one, under the #408 standing operator authority).
/// Each call site keeps its current semantics at cutover; neither is a
/// silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignPolicy {
    /// Fail loud before spending a doomed load attempt (funnel semantics,
    /// #1271) — the foreign resident is never touched.
    Block,
    /// Adopt the foreign resident (#408): reuse it when its ctx is
    /// sufficient; reconcile it when undersized. Every adoption is surfaced
    /// via [`Warning::ForeignResidentAdopted`] — surface, don't ask, never
    /// silent.
    AdoptPer408,
}

/// Per-load budget/headroom bookkeeping.
struct Pending {
    decision_idx: usize,
    model_key: String,
    est: Option<u64>,
}

/// THE pure acquisition planner. See module docs; the per-arm behavior is
/// specified by the table tests below, one row per #1278-family bug class.
pub fn plan_acquire(
    desired: &[Placement],
    facts: &Facts,
    opts: AcquireOpts,
    est: &dyn FootprintEstimator,
) -> Plan {
    let mut warnings: Vec<Warning> = Vec::new();

    // ── per-desired decisions, in desired-input order ────────────────────
    let mut decisions: Vec<PlannedAction> = Vec::new();
    // Resident identifiers a decision reuses or reconciles — never pass-1
    // unloaded, never eviction candidates (a claimed resident is targeted
    // at most once).
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    // Foreign residents an AdoptPer408 decision uses (reuse) or mutates
    // (reconcile) — excluded from user_state_respected (used, not merely
    // respected).
    let mut adopted: BTreeSet<String> = BTreeSet::new();

    for p in desired {
        match decide_residency(&facts.residents, p) {
            ResidencyDecision::LoadFresh => {
                // (#1276) Existence fast-fail: refuse before any load
                // attempt can hang. Skipped — not failed — when the catalog
                // is unavailable (leniency; the Deadline port backstops
                // execution instead).
                if let Some(catalog) = facts.catalog.as_deref() {
                    if !catalog.iter().any(|c| c.model_key == p.model_key) {
                        decisions.push(PlannedAction {
                            action: Action::Block {
                                model_key: p.model_key.clone(),
                                resident_identifier: None,
                            },
                            reason: Reason::UnknownModelKey {
                                nearest: nearest_model_keys(&p.model_key, catalog),
                            },
                            precondition: Precondition::None,
                        });
                        continue;
                    }
                }
                decisions.push(PlannedAction {
                    action: Action::Load {
                        model_key: p.model_key.clone(),
                        identifier: p.identifier.clone(),
                        min_ctx: p.min_ctx,
                    },
                    reason: Reason::NoResident,
                    precondition: Precondition::NoResidentForModelKey {
                        model_key: p.model_key.clone(),
                    },
                });
            }
            ResidencyDecision::Reuse { identifier, resident_ctx } => {
                claimed.insert(identifier.clone());
                push_reuse(&mut decisions, &mut warnings, identifier, resident_ctx, p.min_ctx);
            }
            ResidencyDecision::Reconcile { stale_identifier, stale_ctx } => {
                claimed.insert(stale_identifier.clone());
                let stale = OwnedTarget::claim(&stale_identifier, Some(&p.identifier))
                    .expect("decide_residency only reconciles darkmux-owned or exact-alias residents");
                decisions.push(PlannedAction {
                    action: Action::Reconcile {
                        stale,
                        stale_ctx,
                        model_key: p.model_key.clone(),
                        identifier: p.identifier.clone(),
                        min_ctx: p.min_ctx,
                    },
                    reason: Reason::InsufficientCtx,
                    precondition: Precondition::ResidentPresent {
                        identifier: stale_identifier,
                        at_ctx: Some(stale_ctx),
                    },
                });
            }
            ResidencyDecision::Blocked { resident_identifier } => match opts.foreign_policy {
                ForeignPolicy::Block => {
                    decisions.push(PlannedAction {
                        action: Action::Block {
                            model_key: p.model_key.clone(),
                            resident_identifier: Some(resident_identifier),
                        },
                        reason: Reason::ForeignResident,
                        precondition: Precondition::None,
                    });
                }
                ForeignPolicy::AdoptPer408 => {
                    // The blocking resident is the FIRST modelKey match —
                    // decide_residency's own first-match rule.
                    let r = facts
                        .residents
                        .iter()
                        .find(|r| r.model_key == p.model_key)
                        .expect("Blocked implies a resident sharing the modelKey");
                    warnings.push(Warning::ForeignResidentAdopted {
                        identifier: r.identifier.clone(),
                        model_key: p.model_key.clone(),
                    });
                    adopted.insert(r.identifier.clone());
                    if ctx_sufficient(r.ctx, p.min_ctx) {
                        push_reuse(
                            &mut decisions,
                            &mut warnings,
                            r.identifier.clone(),
                            r.ctx,
                            p.min_ctx,
                        );
                    } else {
                        claimed.insert(r.identifier.clone());
                        decisions.push(PlannedAction {
                            action: Action::Reconcile {
                                stale: OwnedTarget::claim_foreign_per_408(&r.identifier),
                                stale_ctx: r.ctx,
                                model_key: p.model_key.clone(),
                                identifier: p.identifier.clone(),
                                min_ctx: p.min_ctx,
                            },
                            reason: Reason::InsufficientCtx,
                            precondition: Precondition::ResidentPresent {
                                identifier: r.identifier.clone(),
                                at_ctx: Some(r.ctx),
                            },
                        });
                    }
                }
            },
        }
    }

    // ── Exclusive pass 1: unload the no-longer-desired, respect user state ─
    let desired_idents: BTreeSet<&str> = desired.iter().map(|p| p.identifier.as_str()).collect();
    // Unloads keyed by resident index so the final assembly can emit them in
    // host-reported order regardless of which pass produced them.
    let mut unloads: Vec<(usize, PlannedAction)> = Vec::new();
    // Resident indexes leaving residency (pass-1 unloads + reconcile stales
    // + evictions) — the freed-bytes and remaining-base bookkeeping below.
    let mut removed: BTreeSet<usize> = BTreeSet::new();
    let mut user_state_respected: Vec<String> = Vec::new();

    if opts.scope == AcquireScope::Exclusive {
        for (idx, r) in facts.residents.iter().enumerate() {
            if !is_darkmux_owned(&r.identifier) {
                // Foreign — never touched. Listed as respected unless a
                // decision uses it (own-alias claim or a #408 adoption).
                let used = claimed.contains(&r.identifier)
                    || adopted.contains(&r.identifier)
                    || desired_idents.contains(r.identifier.as_str());
                if !used {
                    user_state_respected.push(r.identifier.clone());
                }
                continue;
            }
            if desired_idents.contains(r.identifier.as_str()) || claimed.contains(&r.identifier) {
                continue; // wanted (or already targeted by a Reconcile)
            }
            // (#1280 guard) Evicting the standing utility binding is legal
            // but never silent — a swap-shaped caller that forgot the
            // utility seat would otherwise evict the compactor quietly.
            if facts.utility_binding.as_deref() == Some(r.identifier.as_str()) {
                warnings.push(Warning::UtilityBindingEvicted { identifier: r.identifier.clone() });
            }
            let target = OwnedTarget::claim(&r.identifier, None)
                .expect("namespaced residents always claim");
            unloads.push((
                idx,
                PlannedAction {
                    action: Action::Unload { target },
                    reason: Reason::NoLongerDesired,
                    precondition: Precondition::ResidentPresent {
                        identifier: r.identifier.clone(),
                        at_ctx: Some(r.ctx),
                    },
                },
            ));
            removed.insert(idx);
        }
    }

    // ── estimate pending loads (only when a budget or pool arm will run) ──
    let budget_active = facts.budget.max_darkmux_bytes.is_some();
    let pool_arm_active = opts.intent == CallerIntent::Auto && facts.pools.len() == 1;
    let mut pendings: Vec<Pending> = Vec::new();
    if budget_active || pool_arm_active {
        for (i, d) in decisions.iter().enumerate() {
            let (model_key, min_ctx) = match &d.action {
                Action::Load { model_key, min_ctx, .. } => (model_key.clone(), *min_ctx),
                Action::Reconcile { model_key, min_ctx, .. } => (model_key.clone(), *min_ctx),
                _ => continue,
            };
            let e = est.estimate_bytes(&model_key, min_ctx, facts.catalog.as_deref());
            if e.is_none() {
                warnings.push(Warning::LoadEstimateUnknown { model_key: model_key.clone() });
            }
            pendings.push(Pending { decision_idx: i, model_key, est: e });
        }
    }

    let mut exec_hint = ExecHint::Concurrent;

    // ── #1243 budget arm, refusal half ───────────────────────────────────
    if let Some(budget) = facts.budget.max_darkmux_bytes {
        // Accounting degradation is loud: unknown-size darkmux residents
        // count as 0 against the cap, and say so (host order).
        for r in &facts.residents {
            if is_darkmux_owned(&r.identifier) && r.est_bytes.is_none() {
                warnings.push(Warning::ResidentBytesUnknown { identifier: r.identifier.clone() });
            }
        }
        // A load whose estimate alone exceeds the whole budget is refused
        // for BOTH intents — no eviction sequence can ever satisfy it.
        for pend in &pendings {
            if pend.est.is_some_and(|e| e > budget) {
                decisions[pend.decision_idx] = PlannedAction {
                    action: Action::Block {
                        model_key: pend.model_key.clone(),
                        resident_identifier: None,
                    },
                    reason: Reason::BudgetRefuse {
                        est_bytes: pend.est.expect("checked is_some"),
                        budget_bytes: budget,
                    },
                    precondition: Precondition::None,
                };
            }
        }
    }

    // Reconcile stales leave residency too (freed before their reload).
    // Computed AFTER the refusal pass: a refused Reconcile no longer
    // unloads its stale, so that resident stays counted as occupying.
    for d in &decisions {
        if let Action::Reconcile { stale, .. } = &d.action {
            if let Some(idx) =
                facts.residents.iter().position(|r| r.identifier == stale.identifier())
            {
                removed.insert(idx);
            }
        }
    }

    // ── #1243 budget arm, fit half ───────────────────────────────────────
    if let Some(budget) = facts.budget.max_darkmux_bytes {
        // Base = darkmux-owned residents that remain after pass 1 +
        // reconcile stales. User loads NEVER count (#1243) — physical
        // pressure cross-checks are doctor scope.
        let mut base: u64 = resident_base(facts, &removed);
        let need = pending_sum(&decisions, &pendings);
        if base + need > budget {
            match opts.intent {
                CallerIntent::OperatorExplicit => {
                    // Operator intent wins, loudly — the Load stays.
                    warnings.push(Warning::BudgetExceededOperatorOverride {
                        est_new_bytes: need,
                        darkmux_resident_bytes: base,
                        budget_bytes: budget,
                    });
                }
                CallerIntent::Auto => {
                    // Evict idle darkmux-owned residents in host-reported
                    // order (#1243; named honestly — no recency fact exists,
                    // so this is NOT LRU). Unknown-size residents are never
                    // chosen: the plan cannot account the gain (they already
                    // warned above and count 0 against the base).
                    for (idx, r) in facts.residents.iter().enumerate() {
                        if base + need <= budget {
                            break;
                        }
                        if removed.contains(&idx)
                            || !is_darkmux_owned(&r.identifier)
                            || desired_idents.contains(r.identifier.as_str())
                            || claimed.contains(&r.identifier)
                        {
                            continue;
                        }
                        let Some(freeing) = r.est_bytes else { continue };
                        unloads.push((
                            idx,
                            PlannedAction {
                                action: Action::Unload {
                                    target: OwnedTarget::claim(&r.identifier, None)
                                        .expect("namespaced residents always claim"),
                                },
                                reason: Reason::BudgetEvict {
                                    freeing_bytes: freeing,
                                    need_bytes: need,
                                    budget_bytes: budget,
                                    eviction_order: EvictionOrder::HostReported,
                                },
                                precondition: Precondition::ResidentPresent {
                                    identifier: r.identifier.clone(),
                                    at_ctx: Some(r.ctx),
                                },
                            },
                        ));
                        removed.insert(idx);
                        base = base.saturating_sub(freeing);
                    }
                    if base + need > budget {
                        // Auto never breaches (#1243): refuse any load that
                        // cannot fit even alone after every eviction; the
                        // survivors each fit alone, so if together they
                        // still exceed, serialize them.
                        for pend in &pendings {
                            if !is_load_like(&decisions[pend.decision_idx].action) {
                                continue;
                            }
                            let e = pend.est.unwrap_or(0);
                            if base + e > budget {
                                decisions[pend.decision_idx] = PlannedAction {
                                    action: Action::Block {
                                        model_key: pend.model_key.clone(),
                                        resident_identifier: None,
                                    },
                                    reason: Reason::BudgetRefuse {
                                        est_bytes: e,
                                        budget_bytes: budget,
                                    },
                                    precondition: Precondition::None,
                                };
                            }
                        }
                        if base + pending_sum(&decisions, &pendings) > budget {
                            exec_hint = ExecHint::Sequential;
                        }
                    }
                }
            }
        }
    }

    // ── #1140 pool-headroom arm (Auto only; single-pool v1 rule) ─────────
    // Pool facts are advisory headroom, not an operator contract: the arm
    // evicts to make room and serializes when it can't, but never refuses —
    // the executor's #1139 insufficient-resources fast-fail is the
    // enforcement backstop. With zero or multiple pools the arm is skipped
    // (a placement→pool mapping fact arrives with a second ResourceProbe).
    if pool_arm_active {
        if let Some(pool) = facts.pools.values().next() {
            let snapshot_available = pool.available_bytes;
            // Every planned unload (pass 1, budget evictions, reconcile
            // stales) frees its bytes before the loads run.
            let freed: u64 = removed
                .iter()
                .map(|&idx| facts.residents[idx].est_bytes.unwrap_or(0))
                .sum();
            let mut effective = snapshot_available + freed;
            let need = pending_sum(&decisions, &pendings);
            if need > effective {
                for (idx, r) in facts.residents.iter().enumerate() {
                    if need <= effective {
                        break;
                    }
                    if removed.contains(&idx)
                        || !is_darkmux_owned(&r.identifier)
                        || desired_idents.contains(r.identifier.as_str())
                        || claimed.contains(&r.identifier)
                    {
                        continue;
                    }
                    let Some(freeing) = r.est_bytes else { continue };
                    unloads.push((
                        idx,
                        PlannedAction {
                            action: Action::Unload {
                                target: OwnedTarget::claim(&r.identifier, None)
                                    .expect("namespaced residents always claim"),
                            },
                            reason: Reason::BudgetEvict {
                                freeing_bytes: freeing,
                                need_bytes: need,
                                budget_bytes: snapshot_available,
                                eviction_order: EvictionOrder::HostReported,
                            },
                            precondition: Precondition::ResidentPresent {
                                identifier: r.identifier.clone(),
                                at_ctx: Some(r.ctx),
                            },
                        },
                    ));
                    removed.insert(idx);
                    effective += freeing;
                }
                if need > effective {
                    let load_count = pendings
                        .iter()
                        .filter(|p| is_load_like(&decisions[p.decision_idx].action))
                        .count();
                    let all_fit_alone = pendings
                        .iter()
                        .filter(|p| is_load_like(&decisions[p.decision_idx].action))
                        .all(|p| p.est.unwrap_or(0) <= effective);
                    if load_count > 1 && all_fit_alone {
                        exec_hint = ExecHint::Sequential;
                    }
                }
            }
        }
    }

    // ── assembly: unloads in host-reported order, then decisions ─────────
    unloads.sort_by_key(|(idx, _)| *idx);
    let mut actions: Vec<PlannedAction> = unloads.into_iter().map(|(_, a)| a).collect();
    actions.extend(decisions);
    Plan {
        actions,
        quarantined: Vec::new(),
        user_state_respected,
        warnings,
        exec_hint,
    }
}

/// Refcounted, deduplicated release — the #1279 fix by construction: the
/// planning unit is the BATCH, not one seat. Emits at most ONE
/// `Unload(LastWanterReleased)` per DISTINCT identifier among `releasing`
/// (in host-reported resident order), and none at all for identifiers any
/// `still_active` placement still wants, or that are not resident in `facts`
/// (a phantom unload is never issued).
///
/// Alias-release asymmetry (recorded deliberately): a placement under an
/// explicit un-namespaced alias is treated as OURS by acquisition
/// (reuse/reconcile eligible) but is SKIPPED here — release-parity with the
/// funnel cycler's namespace guard, which no-ops on aliases. The fix
/// (release-by-OwnedTarget would permit alias unload, since the alias is
/// claimable as this call's own identifier) is deferred as an operator call;
/// until then an aliased resident is only ever reclaimed manually or by the
/// alias-bearing profile's own reconcile.
pub fn plan_release(releasing: &[Placement], still_active: &[Placement], facts: &Facts) -> Plan {
    let active: BTreeSet<&str> = still_active.iter().map(|p| p.identifier.as_str()).collect();
    let mut actions: Vec<PlannedAction> = Vec::new();
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    for r in &facts.residents {
        let seats: Vec<String> = {
            let mut s: Vec<String> = releasing
                .iter()
                .filter(|p| p.identifier == r.identifier)
                .map(|p| p.seat.clone())
                .collect();
            s.sort();
            s.dedup();
            s
        };
        if seats.is_empty() {
            continue; // not being released (includes the phantom case by construction)
        }
        if active.contains(r.identifier.as_str()) {
            continue; // a wanter remains — refcount not yet zero (#1279)
        }
        if !is_darkmux_owned(&r.identifier) {
            continue; // alias-release asymmetry — see fn docs
        }
        if !emitted.insert(r.identifier.as_str()) {
            continue; // duplicate resident rows collapse to one unload
        }
        actions.push(PlannedAction {
            action: Action::Unload {
                target: OwnedTarget::claim(&r.identifier, None)
                    .expect("namespaced residents always claim"),
            },
            reason: Reason::LastWanterReleased { seats },
            precondition: Precondition::ResidentPresent {
                identifier: r.identifier.clone(),
                at_ctx: Some(r.ctx),
            },
        });
    }
    Plan { actions, ..Default::default() }
}

/// Shared Reuse emission: the action plus the declared-vs-actual divergence
/// breadcrumb when the resident is bigger than requested (typed interim
/// provenance until #1257).
fn push_reuse(
    decisions: &mut Vec<PlannedAction>,
    warnings: &mut Vec<Warning>,
    identifier: String,
    resident_ctx: u64,
    min_ctx: u32,
) {
    if resident_ctx > u64::from(min_ctx) {
        warnings.push(Warning::CtxDivergence {
            identifier: identifier.clone(),
            requested: min_ctx,
            resident: resident_ctx,
        });
    }
    decisions.push(PlannedAction {
        action: Action::Reuse { identifier, resident_ctx, min_ctx },
        reason: Reason::SufficientCtxResident,
        precondition: Precondition::None,
    });
}

fn is_load_like(action: &Action) -> bool {
    matches!(action, Action::Load { .. } | Action::Reconcile { .. })
}

/// Sum of darkmux-owned resident bytes that remain after the indexes in
/// `removed` leave residency. Unknown sizes count 0 (warned separately).
fn resident_base(facts: &Facts, removed: &BTreeSet<usize>) -> u64 {
    facts
        .residents
        .iter()
        .enumerate()
        .filter(|(idx, r)| !removed.contains(idx) && is_darkmux_owned(&r.identifier))
        .map(|(_, r)| r.est_bytes.unwrap_or(0))
        .sum()
}

/// Sum of estimates for pendings whose decision is still a Load/Reconcile
/// (refused ones drop out). Unknown estimates count 0 (warned at estimation).
fn pending_sum(decisions: &[PlannedAction], pendings: &[Pending]) -> u64 {
    pendings
        .iter()
        .filter(|p| is_load_like(&decisions[p.decision_idx].action))
        .map(|p| p.est.unwrap_or(0))
        .sum()
}

/// v1 fix-hint matching for [`Reason::UnknownModelKey`] (#1276): keys that
/// contain the requested key (or vice versa) case-insensitively, else keys
/// sharing a >= 3-char prefix; alphabetical, capped at 3. Deliberately
/// simple — std-only, no similarity crate.
fn nearest_model_keys(requested: &str, catalog: &[CatalogFact]) -> Vec<String> {
    let req = requested.to_ascii_lowercase();
    let mut hits: Vec<String> = catalog
        .iter()
        .filter(|c| {
            let key = c.model_key.to_ascii_lowercase();
            key.contains(&req) || req.contains(&key) || common_prefix_len(&key, &req) >= 3
        })
        .map(|c| c.model_key.clone())
        .collect();
    hits.sort();
    hits.dedup();
    hits.truncate(3);
    hits
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    //! The table: one row per #1278-family bug class. Every row is a
    //! `Facts` + `Vec<Placement>` in, a totally-Eq [`Plan`] out, asserted
    //! with `assert_eq!` on the whole value.

    use super::*;
    use crate::estimator::FixedEstimator;
    use crate::facts::{Budget, Facts, PoolFact, PoolId, Pools, ResidentFact};
    use std::collections::BTreeMap;

    const GB: u64 = 1_000_000_000;

    fn resident(identifier: &str, model_key: &str, ctx: u64, est_bytes: Option<u64>) -> ResidentFact {
        ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx,
            est_bytes,
        }
    }

    fn placement(model_key: &str, min_ctx: u32) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: format!("darkmux:{model_key}"),
            min_ctx,
            seat: "test".to_string(),
        }
    }

    fn placement_seat(model_key: &str, min_ctx: u32, seat: &str) -> Placement {
        Placement { seat: seat.to_string(), ..placement(model_key, min_ctx) }
    }

    fn aliased(model_key: &str, min_ctx: u32, alias: &str) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: alias.to_string(),
            min_ctx,
            seat: "test".to_string(),
        }
    }

    fn facts(residents: Vec<ResidentFact>) -> Facts {
        Facts { residents, ..Default::default() }
    }

    fn opts(intent: CallerIntent, scope: AcquireScope) -> AcquireOpts {
        AcquireOpts { intent, scope, foreign_policy: ForeignPolicy::Block }
    }

    fn additive_auto() -> AcquireOpts {
        opts(CallerIntent::Auto, AcquireScope::Additive)
    }

    fn no_est() -> FixedEstimator {
        FixedEstimator::default()
    }

    fn est_map(pairs: &[(&str, u64)]) -> FixedEstimator {
        FixedEstimator(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    fn load_action(model_key: &str, min_ctx: u32) -> PlannedAction {
        PlannedAction {
            action: Action::Load {
                model_key: model_key.to_string(),
                identifier: format!("darkmux:{model_key}"),
                min_ctx,
            },
            reason: Reason::NoResident,
            precondition: Precondition::NoResidentForModelKey { model_key: model_key.to_string() },
        }
    }

    // ── residency table rows ─────────────────────────────────────────────

    #[test]
    fn load_fresh() {
        let f = Facts {
            catalog: Some(vec![CatalogFact { model_key: "m".into(), size_bytes: Some(GB) }]),
            ..Default::default()
        };
        let plan = plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan,
            Plan { actions: vec![load_action("m", 8_000)], ..Default::default() }
        );
    }

    #[test]
    fn reuse_sufficient_ctx() {
        // Never reload down: a 16k resident satisfies an 8k request with
        // zero Load/Unload emitted.
        let f = facts(vec![resident("darkmux:m", "m", 16_000, None)]);
        let plan = plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan,
            Plan {
                actions: vec![PlannedAction {
                    action: Action::Reuse {
                        identifier: "darkmux:m".into(),
                        resident_ctx: 16_000,
                        min_ctx: 8_000,
                    },
                    reason: Reason::SufficientCtxResident,
                    precondition: Precondition::None,
                }],
                warnings: vec![Warning::CtxDivergence {
                    identifier: "darkmux:m".into(),
                    requested: 8_000,
                    resident: 16_000,
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn reuse_cross_profile_divergence_breadcrumb() {
        // The typed declared-vs-actual breadcrumb (#1257 interim): reuse at
        // 100k when 68k was requested carries the divergence numbers.
        let f = facts(vec![resident("darkmux:m", "m", 100_000, None)]);
        let plan = plan_acquire(&[placement("m", 68_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan.warnings,
            vec![Warning::CtxDivergence {
                identifier: "darkmux:m".into(),
                requested: 68_000,
                resident: 100_000,
            }]
        );
        assert!(matches!(plan.actions[0].action, Action::Reuse { .. }));
    }

    #[test]
    fn reconcile_undersized() {
        // The #1135 class as one row: a 4096 default-ctx resident wanted at
        // 32k reconciles (one unload-then-load action).
        let f = facts(vec![resident("darkmux:m", "m", 4_096, None)]);
        let plan = plan_acquire(&[placement("m", 32_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan,
            Plan {
                actions: vec![PlannedAction {
                    action: Action::Reconcile {
                        stale: OwnedTarget::claim("darkmux:m", None).unwrap(),
                        stale_ctx: 4_096,
                        model_key: "m".into(),
                        identifier: "darkmux:m".into(),
                        min_ctx: 32_000,
                    },
                    reason: Reason::InsufficientCtx,
                    precondition: Precondition::ResidentPresent {
                        identifier: "darkmux:m".into(),
                        at_ctx: Some(4_096),
                    },
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn blocked_foreign_resident() {
        // (#1271) Never spend a doomed load: a foreign resident sharing the
        // modelKey blocks, and NO Load is emitted.
        let f = facts(vec![resident("m", "m", 16_000, None)]);
        let plan = plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan,
            Plan {
                actions: vec![PlannedAction {
                    action: Action::Block {
                        model_key: "m".into(),
                        resident_identifier: Some("m".into()),
                    },
                    reason: Reason::ForeignResident,
                    precondition: Precondition::None,
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn first_match_wins_order_determinism() {
        // Host-reported order is decision-bearing: user copy first → Block;
        // darkmux copy first → Reuse. Same input order, same output, always.
        let user_first = facts(vec![
            resident("m-manual", "m", 16_000, None),
            resident("darkmux:m", "m", 16_000, None),
        ]);
        let plan = plan_acquire(&[placement("m", 8_000)], &user_first, additive_auto(), &no_est());
        assert!(matches!(
            &plan.actions[0],
            PlannedAction { reason: Reason::ForeignResident, .. }
        ));

        let darkmux_first = facts(vec![
            resident("darkmux:m", "m", 16_000, None),
            resident("m-manual", "m", 16_000, None),
        ]);
        let plan =
            plan_acquire(&[placement("m", 8_000)], &darkmux_first, additive_auto(), &no_est());
        assert!(matches!(
            &plan.actions[0],
            PlannedAction { reason: Reason::SufficientCtxResident, .. }
        ));
    }

    #[test]
    fn explicit_alias_is_ours() {
        // The namespace opt-out: a resident under the placement's own
        // explicit identifier reuses, never Blocked.
        let f = facts(vec![resident("my-alias", "m", 8_000, None)]);
        let plan =
            plan_acquire(&[aliased("m", 8_000, "my-alias")], &f, additive_auto(), &no_est());
        assert_eq!(
            plan.actions,
            vec![PlannedAction {
                action: Action::Reuse {
                    identifier: "my-alias".into(),
                    resident_ctx: 8_000,
                    min_ctx: 8_000,
                },
                reason: Reason::SufficientCtxResident,
                precondition: Precondition::None,
            }]
        );
    }

    // ── #1276 catalog rows ───────────────────────────────────────────────

    #[test]
    fn unknown_model_fast_fail_1276() {
        // No Load can ever reach a hanging load attempt: an uncataloged key
        // blocks at plan time, with nearest-match fix hints.
        let f = Facts {
            catalog: Some(vec![
                CatalogFact { model_key: "qwen3-4b-instruct".into(), size_bytes: Some(GB) },
                CatalogFact { model_key: "devstral".into(), size_bytes: Some(GB) },
            ]),
            ..Default::default()
        };
        let plan = plan_acquire(&[placement("qwen3-4b", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(
            plan.actions,
            vec![PlannedAction {
                action: Action::Block { model_key: "qwen3-4b".into(), resident_identifier: None },
                reason: Reason::UnknownModelKey { nearest: vec!["qwen3-4b-instruct".into()] },
                precondition: Precondition::None,
            }]
        );
    }

    #[test]
    fn catalog_unavailable_lenient() {
        // Facts.catalog = None means the existence check is SKIPPED, not
        // failed — the bounded Deadline port backstops execution instead.
        let f = Facts { catalog: None, ..Default::default() };
        let plan = plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(plan.actions, vec![load_action("m", 8_000)]);
    }

    // ── #1280 utility rows ───────────────────────────────────────────────

    #[test]
    fn utility_same_contract_1280() {
        // The utility seat rides the IDENTICAL path — no warn-only gap: the
        // absent utility model gets a real namespaced ctx-pinned Load.
        let f = facts(vec![resident("darkmux:primary", "primary", 32_000, None)]);
        let desired =
            vec![placement_seat("primary", 32_000, "primary"), placement_seat("util", 68_000, "utility")];
        let plan = plan_acquire(&desired, &f, additive_auto(), &no_est());
        assert_eq!(
            plan.actions,
            vec![
                PlannedAction {
                    action: Action::Reuse {
                        identifier: "darkmux:primary".into(),
                        resident_ctx: 32_000,
                        min_ctx: 32_000,
                    },
                    reason: Reason::SufficientCtxResident,
                    precondition: Precondition::None,
                },
                load_action("util", 68_000),
            ]
        );
    }

    #[test]
    fn utility_reconcile_jit_default_ctx() {
        // The #1135 class closed for the compactor too: a stale 4096
        // default-ctx utility load reconciles to the 68k the seat needs.
        let f = facts(vec![resident("darkmux:util", "util", 4_096, None)]);
        let desired =
            vec![placement_seat("primary", 32_000, "primary"), placement_seat("util", 68_000, "utility")];
        let plan = plan_acquire(&desired, &f, additive_auto(), &no_est());
        assert_eq!(plan.actions[0], load_action("primary", 32_000));
        assert_eq!(
            plan.actions[1],
            PlannedAction {
                action: Action::Reconcile {
                    stale: OwnedTarget::claim("darkmux:util", None).unwrap(),
                    stale_ctx: 4_096,
                    model_key: "util".into(),
                    identifier: "darkmux:util".into(),
                    min_ctx: 68_000,
                },
                reason: Reason::InsufficientCtx,
                precondition: Precondition::ResidentPresent {
                    identifier: "darkmux:util".into(),
                    at_ctx: Some(4_096),
                },
            }
        );
    }

    // ── scope rows ───────────────────────────────────────────────────────

    #[test]
    fn exclusive_scope_pass1_unloads() {
        // The swap two-pass shape: unload-before-load, in THAT order.
        let f = facts(vec![resident("darkmux:old", "old", 8_000, None)]);
        let plan = plan_acquire(
            &[placement("new", 8_000)],
            &f,
            opts(CallerIntent::OperatorExplicit, AcquireScope::Exclusive),
            &no_est(),
        );
        assert_eq!(
            plan.actions,
            vec![
                PlannedAction {
                    action: Action::Unload {
                        target: OwnedTarget::claim("darkmux:old", None).unwrap(),
                    },
                    reason: Reason::NoLongerDesired,
                    precondition: Precondition::ResidentPresent {
                        identifier: "darkmux:old".into(),
                        at_ctx: Some(8_000),
                    },
                },
                load_action("new", 8_000),
            ]
        );
    }

    #[test]
    fn additive_scope_leaves_others() {
        // ensure-loaded parity: only the desired placement is touched.
        let f = facts(vec![resident("darkmux:old", "old", 8_000, None)]);
        let plan = plan_acquire(&[placement("new", 8_000)], &f, additive_auto(), &no_est());
        assert_eq!(plan.actions, vec![load_action("new", 8_000)]);
    }

    #[test]
    fn user_state_respected_provenance() {
        // SwapResult.user_state_respected parity: foreign residents
        // deliberately left alone are surfaced, and no action targets them.
        let f = facts(vec![resident("their-model", "their-model", 8_000, None)]);
        let plan = plan_acquire(
            &[placement("new", 8_000)],
            &f,
            opts(CallerIntent::OperatorExplicit, AcquireScope::Exclusive),
            &no_est(),
        );
        assert_eq!(plan.user_state_respected, vec!["their-model".to_string()]);
        assert_eq!(plan.actions, vec![load_action("new", 8_000)]);
    }

    #[test]
    fn exclusive_evicting_utility_binding_warns() {
        // (#1280 guard, other direction) A swap-shaped caller that forgot
        // to include the utility seat cannot silently evict the compactor:
        // the pass-1 unload still happens, loudly.
        let f = Facts {
            residents: vec![resident("darkmux:util-4b", "util-4b", 68_000, None)],
            utility_binding: Some("darkmux:util-4b".into()),
            ..Default::default()
        };
        let plan = plan_acquire(
            &[placement("new", 8_000)],
            &f,
            opts(CallerIntent::OperatorExplicit, AcquireScope::Exclusive),
            &no_est(),
        );
        assert_eq!(
            plan.warnings,
            vec![Warning::UtilityBindingEvicted { identifier: "darkmux:util-4b".into() }]
        );
        assert!(matches!(
            &plan.actions[0],
            PlannedAction { reason: Reason::NoLongerDesired, .. }
        ));
    }

    // ── #408 foreign-policy rows ─────────────────────────────────────────

    #[test]
    fn adopt_per_408_reuses_foreign_sufficient() {
        // The dispatch-path behavior preserved: a sufficient foreign
        // resident is adopted (reused), surfaced via warning — never silent.
        let f = facts(vec![resident("m-manual", "m", 100_000, None)]);
        let o = AcquireOpts {
            intent: CallerIntent::Auto,
            scope: AcquireScope::Additive,
            foreign_policy: ForeignPolicy::AdoptPer408,
        };
        let plan = plan_acquire(&[placement("m", 8_000)], &f, o, &no_est());
        assert_eq!(
            plan.actions,
            vec![PlannedAction {
                action: Action::Reuse {
                    identifier: "m-manual".into(),
                    resident_ctx: 100_000,
                    min_ctx: 8_000,
                },
                reason: Reason::SufficientCtxResident,
                precondition: Precondition::None,
            }]
        );
        assert_eq!(
            plan.warnings,
            vec![
                Warning::ForeignResidentAdopted {
                    identifier: "m-manual".into(),
                    model_key: "m".into(),
                },
                Warning::CtxDivergence {
                    identifier: "m-manual".into(),
                    requested: 8_000,
                    resident: 100_000,
                },
            ]
        );
    }

    #[test]
    fn adopt_per_408_reconciles_foreign_undersized() {
        // The #408 standing authority: an undersized foreign resident is
        // reconciled (unloaded, reloaded under the darkmux namespace),
        // through the explicitly-named claim path, surfaced via warning.
        let f = facts(vec![resident("m-manual", "m", 4_096, None)]);
        let o = AcquireOpts {
            intent: CallerIntent::Auto,
            scope: AcquireScope::Additive,
            foreign_policy: ForeignPolicy::AdoptPer408,
        };
        let plan = plan_acquire(&[placement("m", 32_000)], &f, o, &no_est());
        assert_eq!(
            plan.actions,
            vec![PlannedAction {
                action: Action::Reconcile {
                    stale: OwnedTarget::claim_foreign_per_408("m-manual"),
                    stale_ctx: 4_096,
                    model_key: "m".into(),
                    identifier: "darkmux:m".into(),
                    min_ctx: 32_000,
                },
                reason: Reason::InsufficientCtx,
                precondition: Precondition::ResidentPresent {
                    identifier: "m-manual".into(),
                    at_ctx: Some(4_096),
                },
            }]
        );
        assert_eq!(
            plan.warnings,
            vec![Warning::ForeignResidentAdopted {
                identifier: "m-manual".into(),
                model_key: "m".into(),
            }]
        );
    }

    // ── #1243 budget rows ────────────────────────────────────────────────

    #[test]
    fn budget_auto_evicts_before_load_1243() {
        // Auto never breaches: an idle darkmux-owned resident is evicted
        // (host-reported order, named honestly) before the Load.
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, Some(20 * GB))],
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let plan =
            plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &est_map(&[("m", 15 * GB)]));
        assert_eq!(
            plan,
            Plan {
                actions: vec![
                    PlannedAction {
                        action: Action::Unload {
                            target: OwnedTarget::claim("darkmux:idle", None).unwrap(),
                        },
                        reason: Reason::BudgetEvict {
                            freeing_bytes: 20 * GB,
                            need_bytes: 15 * GB,
                            budget_bytes: 30 * GB,
                            eviction_order: EvictionOrder::HostReported,
                        },
                        precondition: Precondition::ResidentPresent {
                            identifier: "darkmux:idle".into(),
                            at_ctx: Some(8_000),
                        },
                    },
                    load_action("m", 8_000),
                ],
                ..Default::default()
            }
        );
    }

    #[test]
    fn budget_operator_override_warns_1243() {
        // Same over-budget shape, operator-explicit: the Load survives, the
        // numbers are loud, nothing is evicted.
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, Some(20 * GB))],
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let plan = plan_acquire(
            &[placement("m", 8_000)],
            &f,
            opts(CallerIntent::OperatorExplicit, AcquireScope::Additive),
            &est_map(&[("m", 15 * GB)]),
        );
        assert_eq!(plan.actions, vec![load_action("m", 8_000)]);
        assert_eq!(
            plan.warnings,
            vec![Warning::BudgetExceededOperatorOverride {
                est_new_bytes: 15 * GB,
                darkmux_resident_bytes: 20 * GB,
                budget_bytes: 30 * GB,
            }]
        );
    }

    #[test]
    fn budget_refuse_model_bigger_than_budget() {
        // A model whose estimate alone exceeds the whole budget is refused
        // for BOTH intents — no eviction sequence is proposed, ever.
        let f = Facts {
            budget: Budget { max_darkmux_bytes: Some(8 * GB) },
            ..Default::default()
        };
        let expected = Plan {
            actions: vec![PlannedAction {
                action: Action::Block { model_key: "m".into(), resident_identifier: None },
                reason: Reason::BudgetRefuse { est_bytes: 22 * GB, budget_bytes: 8 * GB },
                precondition: Precondition::None,
            }],
            ..Default::default()
        };
        for intent in [CallerIntent::Auto, CallerIntent::OperatorExplicit] {
            let plan = plan_acquire(
                &[placement("m", 8_000)],
                &f,
                opts(intent, AcquireScope::Additive),
                &est_map(&[("m", 22 * GB)]),
            );
            assert_eq!(plan, expected, "refused under {intent:?}");
        }
    }

    #[test]
    fn budget_counts_only_darkmux_owned() {
        // User loads never count against the cap: 25GB of user state +
        // 4GB darkmux + a 10GB load fits a 30GB budget — plain Load.
        let f = Facts {
            residents: vec![
                resident("user-big", "user-big", 8_000, Some(25 * GB)),
                resident("darkmux:small", "small", 8_000, Some(4 * GB)),
            ],
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let plan =
            plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &est_map(&[("m", 10 * GB)]));
        assert_eq!(
            plan,
            Plan { actions: vec![load_action("m", 8_000)], ..Default::default() }
        );
    }

    #[test]
    fn budget_sequential_hint() {
        // The #1243 serialize arm: two loads that each fit alone but not
        // together — both Loads survive, hint says run them one at a time.
        let f = Facts {
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let plan = plan_acquire(
            &[placement("a", 8_000), placement("b", 8_000)],
            &f,
            additive_auto(),
            &est_map(&[("a", 20 * GB), ("b", 15 * GB)]),
        );
        assert_eq!(
            plan,
            Plan {
                actions: vec![load_action("a", 8_000), load_action("b", 8_000)],
                exec_hint: ExecHint::Sequential,
                ..Default::default()
            }
        );
    }

    #[test]
    fn pool_headroom_evict_1140() {
        // The headroom arm: eviction scoped strictly to the namespace makes
        // room in the pool before the Load.
        let pools: Pools = BTreeMap::from([(
            PoolId("unified".into()),
            PoolFact { capacity_bytes: 32 * GB, available_bytes: 10 * GB },
        )]);
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, Some(12 * GB))],
            pools,
            ..Default::default()
        };
        let plan =
            plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &est_map(&[("m", 15 * GB)]));
        assert_eq!(
            plan.actions,
            vec![
                PlannedAction {
                    action: Action::Unload {
                        target: OwnedTarget::claim("darkmux:idle", None).unwrap(),
                    },
                    reason: Reason::BudgetEvict {
                        freeing_bytes: 12 * GB,
                        need_bytes: 15 * GB,
                        budget_bytes: 10 * GB,
                        eviction_order: EvictionOrder::HostReported,
                    },
                    precondition: Precondition::ResidentPresent {
                        identifier: "darkmux:idle".into(),
                        at_ctx: Some(8_000),
                    },
                },
                load_action("m", 8_000),
            ]
        );
    }

    #[test]
    fn resident_bytes_unknown_degrades_loud() {
        // Accounting degradation is visible, never silent: an unknown-size
        // darkmux resident under an active budget warns and counts as 0.
        let f = Facts {
            residents: vec![resident("darkmux:idle", "idle", 8_000, None)],
            budget: Budget { max_darkmux_bytes: Some(10 * GB) },
            ..Default::default()
        };
        let plan =
            plan_acquire(&[placement("m", 8_000)], &f, additive_auto(), &est_map(&[("m", 8 * GB)]));
        assert_eq!(
            plan,
            Plan {
                actions: vec![load_action("m", 8_000)],
                warnings: vec![Warning::ResidentBytesUnknown { identifier: "darkmux:idle".into() }],
                ..Default::default()
            }
        );
    }

    // ── #1279 release rows ───────────────────────────────────────────────

    #[test]
    fn release_dedup_1279() {
        // Two seats resolved to the SAME identifier release exactly ONE
        // Unload — the batch is the planning unit, by construction.
        let f = facts(vec![resident("darkmux:m", "m", 8_000, None)]);
        let releasing =
            vec![placement_seat("m", 8_000, "probe:a"), placement_seat("m", 8_000, "probe:b")];
        let plan = plan_release(&releasing, &[], &f);
        assert_eq!(
            plan,
            Plan {
                actions: vec![PlannedAction {
                    action: Action::Unload {
                        target: OwnedTarget::claim("darkmux:m", None).unwrap(),
                    },
                    reason: Reason::LastWanterReleased {
                        seats: vec!["probe:a".into(), "probe:b".into()],
                    },
                    precondition: Precondition::ResidentPresent {
                        identifier: "darkmux:m".into(),
                        at_ctx: Some(8_000),
                    },
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn release_refcount_still_wanted() {
        // A wanter remains (the judge) — zero actions until the last one
        // releases.
        let f = facts(vec![resident("darkmux:m", "m", 8_000, None)]);
        let plan = plan_release(
            &[placement_seat("m", 8_000, "probe")],
            &[placement_seat("m", 8_000, "judge")],
            &f,
        );
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn release_not_resident_is_silent() {
        // A phantom unload is never issued — the op MockHost would reject
        // with NotResident never enters the plan.
        let plan = plan_release(&[placement("m", 8_000)], &[], &facts(vec![]));
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn release_respects_alias_namespace_guard() {
        // The recorded alias-release asymmetry: an explicit un-namespaced
        // alias is ours to acquisition but skipped by release (funnel
        // cycler no-op parity) — see plan_release docs for the deferred fix.
        let f = facts(vec![resident("custom", "m", 8_000, None)]);
        let plan = plan_release(&[aliased("m", 8_000, "custom")], &[], &f);
        assert_eq!(plan, Plan::default());
    }

    // ── cross-cutting invariants ─────────────────────────────────────────

    #[test]
    fn plan_total_equality_determinism() {
        // The property every other row relies on: same input ⇒ identical
        // Plan, including across a budget-arm fixture with evictions.
        let f = Facts {
            residents: vec![
                resident("darkmux:idle", "idle", 8_000, Some(20 * GB)),
                resident("user-thing", "user-thing", 8_000, Some(5 * GB)),
                resident("darkmux:reused", "reused", 32_000, Some(3 * GB)),
            ],
            catalog: Some(vec![CatalogFact { model_key: "m".into(), size_bytes: Some(GB) }]),
            budget: Budget { max_darkmux_bytes: Some(30 * GB) },
            ..Default::default()
        };
        let desired = vec![placement("reused", 16_000), placement("m", 8_000)];
        let est = est_map(&[("m", 15 * GB)]);
        let a = plan_acquire(&desired, &f, additive_auto(), &est);
        let b = plan_acquire(&desired.clone(), &f.clone(), additive_auto(), &est);
        assert_eq!(a, b);
    }

    #[test]
    fn mutating_actions_always_carry_preconditions() {
        // The global executor contract: every mutating action in ANY
        // produced plan carries a non-None precondition to re-verify —
        // asserted across a battery of fixture plans covering fresh loads,
        // reconciles, pass-1 unloads, budget evictions, adoption, and
        // release.
        let assert_invariant = |plan: &Plan, label: &str| {
            for pa in &plan.actions {
                if pa.action.is_mutating() {
                    assert_ne!(
                        pa.precondition,
                        Precondition::None,
                        "{label}: mutating action without a precondition: {pa:?}"
                    );
                }
            }
        };

        let battery: Vec<(Plan, &str)> = vec![
            (
                plan_acquire(
                    &[placement("m", 8_000)],
                    &Facts::default(),
                    additive_auto(),
                    &no_est(),
                ),
                "fresh load",
            ),
            (
                plan_acquire(
                    &[placement("m", 32_000)],
                    &facts(vec![resident("darkmux:m", "m", 4_096, None)]),
                    additive_auto(),
                    &no_est(),
                ),
                "reconcile",
            ),
            (
                plan_acquire(
                    &[placement("new", 8_000)],
                    &facts(vec![resident("darkmux:old", "old", 8_000, None)]),
                    opts(CallerIntent::OperatorExplicit, AcquireScope::Exclusive),
                    &no_est(),
                ),
                "exclusive pass-1 unload",
            ),
            (
                plan_acquire(
                    &[placement("m", 8_000)],
                    &Facts {
                        residents: vec![resident("darkmux:idle", "idle", 8_000, Some(20 * GB))],
                        budget: Budget { max_darkmux_bytes: Some(30 * GB) },
                        ..Default::default()
                    },
                    additive_auto(),
                    &est_map(&[("m", 15 * GB)]),
                ),
                "budget eviction",
            ),
            (
                plan_acquire(
                    &[placement("m", 32_000)],
                    &facts(vec![resident("m-manual", "m", 4_096, None)]),
                    AcquireOpts {
                        intent: CallerIntent::Auto,
                        scope: AcquireScope::Additive,
                        foreign_policy: ForeignPolicy::AdoptPer408,
                    },
                    &no_est(),
                ),
                "adopt-per-408 reconcile",
            ),
            (
                plan_release(
                    &[placement("m", 8_000)],
                    &[],
                    &facts(vec![resident("darkmux:m", "m", 8_000, None)]),
                ),
                "release",
            ),
        ];
        for (plan, label) in &battery {
            assert!(
                plan.actions.iter().any(|a| a.action.is_mutating()),
                "{label}: fixture must actually produce a mutating action"
            );
            assert_invariant(plan, label);
        }
    }
}
