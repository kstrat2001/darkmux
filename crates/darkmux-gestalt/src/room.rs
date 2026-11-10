//! What a plan asks of the machine's one memory budget.
//!
//! The budget is the machine's, whatever engine holds a model (ENGINES.md,
//! open question 2): an LM Studio load counts darkmux's idle engine servers
//! (Tensorfold, rec's Flint) against the memory it loads into, and the
//! caller stops them, least recently started first, until the plan fits
//! (#3506). This module is the plan's half of that sum, pure like the rest
//! of the core: the memory its loads take and the memory its own unloads
//! free (#3508). The
//! machine's half (what is free now, which servers are darkmux's and idle)
//! is read and acted on by the caller, which has the I/O.

use crate::estimator::FootprintEstimator;
use crate::facts::Facts;
use crate::plan::{Action, Plan};
use crate::planner::resident_bytes;

/// A plan's memory: what its loads take and what its unloads free first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanRoom {
    /// What every model the plan loads takes at its context (`est`'s
    /// figure: weights, the KV cache at that context and a margin).
    pub need_bytes: u64,
    /// The footprint LM Studio reports for every resident the plan unloads
    /// before it loads (an unknown size frees 0).
    pub freed_bytes: u64,
}

impl PlanRoom {
    /// Whether the plan's loads fit in `free` bytes once its unloads ran.
    pub fn fits(&self, free: u64) -> bool {
        free.saturating_add(self.freed_bytes) >= self.need_bytes
    }
}

/// The memory `plan` needs, priced by `est`: `None` when it loads nothing,
/// refuses (a plan with a `Block` loads nothing), or loads a model `est`
/// cannot price, since a sum with an unknown term is no figure at all.
pub fn plan_room(plan: &Plan, facts: &Facts, est: &dyn FootprintEstimator) -> Option<PlanRoom> {
    if plan.actions.iter().any(|a| matches!(a.action, Action::Block { .. })) {
        return None;
    }
    let loads: Vec<(&str, u32)> = plan
        .actions
        .iter()
        .filter_map(|a| match &a.action {
            Action::Load { model_key, min_ctx, .. } => Some((model_key.as_str(), *min_ctx)),
            _ => None,
        })
        .collect();
    if loads.is_empty() {
        return None;
    }
    let mut need_bytes = 0u64;
    for (key, ctx) in loads {
        need_bytes = need_bytes.saturating_add(est.estimate_bytes(key, ctx, facts.catalog.as_deref())?);
    }
    let freed_bytes = plan
        .actions
        .iter()
        .filter_map(|a| match &a.action {
            Action::Unload { target } => resident_bytes(facts, target.identifier()),
            _ => None,
        })
        .sum();
    Some(PlanRoom { need_bytes, freed_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::FixedEstimator;
    use crate::facts::ResidentFact;
    use crate::plan::{OwnedTarget, PlannedAction, Precondition, Reason};

    fn load(key: &str) -> PlannedAction {
        PlannedAction {
            action: Action::Load { model_key: key.into(), identifier: format!("darkmux:{key}"), min_ctx: 8_000 },
            reason: Reason::NoResident,
            precondition: Precondition::NoResidentForModelKey { model_key: key.into() },
        }
    }

    fn unload(id: &str) -> PlannedAction {
        PlannedAction {
            action: Action::Unload { target: OwnedTarget::claim(id, None).unwrap() },
            reason: Reason::NoLongerDesired,
            precondition: Precondition::ResidentPresent { identifier: id.into(), at_ctx: None },
        }
    }

    fn priced(pairs: &[(&str, u64)]) -> FixedEstimator {
        FixedEstimator(pairs.iter().map(|(k, b)| (k.to_string(), *b)).collect())
    }

    fn resident(id: &str, bytes: Option<u64>) -> ResidentFact {
        ResidentFact { identifier: id.into(), model_key: id.trim_start_matches("darkmux:").into(), ctx: 8_000, est_bytes: bytes, parallel: 0 }
    }

    /// The loads' estimates are the need; the plan's own unloads free their
    /// reported footprint (an unknown one frees nothing).
    #[test]
    fn a_plan_needs_its_loads_weights_less_what_its_unloads_free() {
        let plan = Plan { actions: vec![unload("darkmux:old"), unload("darkmux:unsized"), load("a"), load("b")], ..Default::default() };
        let facts = Facts {
            residents: vec![resident("darkmux:old", Some(5)), resident("darkmux:unsized", None)],
            ..Default::default()
        };
        let room = plan_room(&plan, &facts, &priced(&[("a", 10), ("b", 20)])).expect("a figure");
        assert_eq!(room, PlanRoom { need_bytes: 30, freed_bytes: 5 });
        assert!(room.fits(25), "25 free plus 5 freed holds 30");
        assert!(!room.fits(24), "24 free plus 5 freed does not");
    }

    /// No figure: nothing loads, the plan refuses, or a load the estimator
    /// cannot price.
    #[test]
    fn a_plan_has_no_figure_without_a_sized_load() {
        let facts = Facts::default();
        let est = priced(&[("a", 10)]);
        let none = |actions: Vec<PlannedAction>| plan_room(&Plan { actions, ..Default::default() }, &facts, &est);
        assert_eq!(none(vec![unload("darkmux:x")]), None, "loads nothing");
        let block = PlannedAction {
            action: Action::Block { model_key: "a".into(), resident_identifier: None },
            reason: Reason::UnknownModelKey { nearest: Vec::new() },
            precondition: Precondition::None,
        };
        assert_eq!(none(vec![block, load("a")]), None, "refuses");
        assert_eq!(none(vec![load("a"), load("absent")]), None, "a load the estimator cannot price");
        assert_eq!(none(vec![load("a")]), Some(PlanRoom { need_bytes: 10, freed_bytes: 0 }));
    }

    /// The pure core performs no I/O, reads no clock and no environment
    /// (`lib.rs`'s crate rules): everything outside the ports and the mock
    /// host, comments aside, names none of the calls that would.
    #[test]
    fn the_planning_core_performs_no_io() {
        let core = [
            ("desired.rs", include_str!("desired.rs")),
            ("estimator.rs", include_str!("estimator.rs")),
            ("facts.rs", include_str!("facts.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("ownership.rs", include_str!("ownership.rs")),
            ("plan.rs", include_str!("plan.rs")),
            ("planner.rs", include_str!("planner.rs")),
            ("residency.rs", include_str!("residency.rs")),
            ("room.rs", include_str!("room.rs")),
            ("waves.rs", include_str!("waves.rs")),
        ];
        let forbidden = ["std::process", "std::fs", "std::env", "std::net", "SystemTime", "Instant::now", "Command::new"];
        for (name, src) in core {
            // Production code only: a test module may read a fixture.
            let production = src.split("#[cfg(test)]\nmod tests").next().unwrap_or_default();
            for (n, line) in production.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                for f in forbidden.iter().filter(|f| code.contains(**f)) {
                    panic!("{name}:{}: the pure core names `{f}`: {line}", n + 1);
                }
            }
        }
        let listed: Vec<&str> = core.iter().map(|(n, _)| *n).collect();
        let modules = include_str!("lib.rs").lines().filter_map(|l| l.strip_prefix("pub mod ")).map(|m| m.trim_end_matches(';'));
        for m in modules.filter(|m| !["mock", "ports"].contains(m)) {
            assert!(listed.contains(&format!("{m}.rs").as_str()), "module `{m}` is outside this check: list it");
        }
    }
}
