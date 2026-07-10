//! Plan vocabulary (#1274): Load/Unload/Reuse/Reconcile/Block actions, each
//! carrying a typed [`Reason`] (Display renders the operator string; tests
//! assert the variant) and a machine-checkable [`Precondition`] the packet-3
//! executor re-verifies immediately before executing.

use crate::ownership::is_darkmux_owned;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Proof-carrying mutation target — the #1284 namespace contract made
/// structural rather than reviewed-for.
///
/// Every Unload / Reconcile-stale target holds an `OwnedTarget`, and
/// [`crate::ports::ModelHost::unload`] accepts nothing else. The field is
/// private and the only constructors are the claim gates below, so a plan
/// that mutates a foreign resident is unrepresentable except through the
/// explicitly-named #408 authority path. `Serialize`-only (no `Deserialize`):
/// serialized plans are artifacts for humans and run records, and cannot be
/// deserialized back into executable form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnedTarget {
    identifier: String,
}

/// Refusal from [`OwnedTarget::claim`]: the identifier is user/operator
/// state (not darkmux-namespaced, not this call's own alias).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignTargetError {
    pub identifier: String,
}

impl fmt::Display for ForeignTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\"{}\" is not darkmux-owned (user/operator state) — refused as a mutation target",
            self.identifier
        )
    }
}

impl std::error::Error for ForeignTargetError {}

impl OwnedTarget {
    /// Claim `identifier` as a mutation target. Succeeds when the identifier
    /// is darkmux-namespaced, or equals `own_alias` — the exact explicit
    /// alias THIS call loads under (the documented namespace opt-out, whose
    /// resident is darkmux's own load). Anything else is foreign and is
    /// refused: user state is off-limits by construction, not by careful
    /// coding.
    pub fn claim(identifier: &str, own_alias: Option<&str>) -> Result<Self, ForeignTargetError> {
        if is_darkmux_owned(identifier) || own_alias == Some(identifier) {
            Ok(Self { identifier: identifier.to_string() })
        } else {
            Err(ForeignTargetError { identifier: identifier.to_string() })
        }
    }

    /// The ONE deliberate exception to the claim gate: #408 standing
    /// operator authority ("standing permission to unload/load when the
    /// resident would poison the dispatch — surface, don't ask"). Exists
    /// solely for [`crate::planner::ForeignPolicy::AdoptPer408`], which
    /// preserves the pre-gestalt dispatch path's behavior of reconciling
    /// even a foreign undersized resident; every use is paired with
    /// [`Warning::ForeignResidentAdopted`] so the adoption is surfaced,
    /// never silent. A call site invoking this constructor is naming the
    /// authority it acts under.
    pub fn claim_foreign_per_408(identifier: &str) -> Self {
        Self { identifier: identifier.to_string() }
    }

    pub fn identifier(&self) -> &str {
        &self.identifier
    }
}

/// Precondition the executor re-verifies against live host state immediately
/// before executing an action (#1274 staleness decision: facts are
/// snapshots; drift aborts-and-replans — no transactional machinery).
/// Machine-checkable so the executor never re-derives intent from the
/// action's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Precondition {
    /// No resident shares this modelKey (guards a Load — a resident
    /// appearing since plan time means the plan is stale).
    NoResidentForModelKey { model_key: String },
    /// The named resident is still present (guards an Unload / Reconcile);
    /// `at_ctx` pins the observed ctx when known.
    ResidentPresent { identifier: String, at_ctx: Option<u64> },
    /// No precondition — only ever carried by non-mutating actions
    /// (Reuse/Block). Every mutating action carries a real one (invariant,
    /// asserted globally in the planner tests).
    None,
}

/// The #1274 action vocabulary — decide_residency's shape (PR #1275) plus
/// batch-level Unload. Reconcile is ONE action (an unload-then-load pair)
/// so one desired placement ⇒ at most one action ⇒ one-row assertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Action {
    Load { model_key: String, identifier: String, min_ctx: u32 },
    Unload { target: OwnedTarget },
    Reuse { identifier: String, resident_ctx: u64, min_ctx: u32 },
    Reconcile {
        stale: OwnedTarget,
        stale_ctx: u64,
        model_key: String,
        identifier: String,
        min_ctx: u32,
    },
    Block { model_key: String, resident_identifier: Option<String> },
}

impl Action {
    /// Does executing this action mutate host state? Mutating actions must
    /// carry a non-[`Precondition::None`] precondition (asserted globally in
    /// the planner tests) — the executor's re-verify contract keys on this.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Action::Load { .. } | Action::Unload { .. } | Action::Reconcile { .. })
    }
}

/// Eviction ordering fact carried on [`Reason::BudgetEvict`], named honestly:
/// no last-used fact exists anywhere today (host residency reports carry no
/// timestamps), so eviction walks idle darkmux-owned residents in
/// host-reported order — deterministic, documented, and NOT LRU. Real
/// recency arrives with #1257 load provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvictionOrder {
    HostReported,
}

/// Typed per-action reason — tests assert the variant; [`fmt::Display`]
/// renders the operator string ("why did darkmux unload X?" always has an
/// answer, #1274).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// Load: no resident shares the modelKey.
    NoResident,
    /// Reuse: darkmux-owned/alias resident with ctx >= minimum (never
    /// reload down — #600 n_ctx-as-minimum).
    SufficientCtxResident,
    /// Reconcile: resident undersized — the #1135 class; a second concurrent
    /// load of the same weights is the #1271 OOM class, so unload-then-load.
    InsufficientCtx,
    /// Block: a foreign (user/operator) resident shares the weights — never
    /// touched.
    ForeignResident,
    /// Block: model key absent from the host catalog — the #1276 fast-fail
    /// before any load attempt can hang. `nearest` = closest catalog keys
    /// for the fix hint.
    UnknownModelKey { nearest: Vec<String> },
    /// Unload (Exclusive-scope pass 1): darkmux-owned but not in the desired
    /// set.
    NoLongerDesired,
    /// Unload (release): every wanter of this identifier released — the
    /// #1279 refcount, seats listed (sorted) for provenance.
    LastWanterReleased { seats: Vec<String> },
    /// Unload (budget/headroom): deterministic host-reported-order eviction
    /// of an idle darkmux-owned resident to fit a pending load (#1243 auto
    /// arm / #1140 headroom arm). `budget_bytes` is the binding limit — the
    /// #1243 cap in the budget arm, the pool's available-bytes snapshot in
    /// the headroom arm.
    BudgetEvict {
        freeing_bytes: u64,
        need_bytes: u64,
        budget_bytes: u64,
        eviction_order: EvictionOrder,
    },
    /// Block: the load cannot be satisfied within the AI RAM budget by any
    /// eviction of darkmux-owned residents (#1243) — including the flat case
    /// where the model alone exceeds the whole budget, which is refused for
    /// BOTH caller intents.
    BudgetRefuse { est_bytes: u64, budget_bytes: u64 },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NoResident => {
                write!(f, "no resident shares this modelKey — loading fresh")
            }
            Reason::SufficientCtxResident => write!(
                f,
                "a darkmux-managed resident already satisfies the context minimum — reusing it (a larger load is never reloaded down)"
            ),
            Reason::InsufficientCtx => write!(
                f,
                "the resident context is below the required minimum — unloading it and reloading at the required context (silently reusing the wrong context is the #1135 bug class)"
            ),
            Reason::ForeignResident => write!(
                f,
                "a resident sharing these weights is not darkmux-owned (user/operator state) — blocked, never touched; free it yourself (`darkmux model eject` if it is stale darkmux state under a legacy identifier, or `lms unload <identifier>`), then re-plan"
            ),
            Reason::UnknownModelKey { nearest } => {
                write!(
                    f,
                    "the model key is not in the host catalog — refused before any load attempt could hang or prompt a download (#1276)"
                )?;
                if !nearest.is_empty() {
                    write!(f, "; nearest catalog keys: {}", nearest.join(", "))?;
                }
                Ok(())
            }
            Reason::NoLongerDesired => write!(
                f,
                "this darkmux-owned resident is not in the desired set — unloading (exclusive-scope reconciliation, pass 1)"
            ),
            Reason::LastWanterReleased { seats } => write!(
                f,
                "every seat wanting this resident has released it (seats: {}) — unloading once (#1279 refcount)",
                seats.join(", ")
            ),
            Reason::BudgetEvict { freeing_bytes, need_bytes, budget_bytes, eviction_order } => {
                let order = match eviction_order {
                    EvictionOrder::HostReported => "host-reported order (no recency fact exists yet — this is not LRU)",
                };
                write!(
                    f,
                    "evicting an idle darkmux-owned resident in {order} to free {freeing_bytes} bytes toward {need_bytes} bytes of pending loads under a {budget_bytes}-byte limit (#1243/#1140)"
                )
            }
            Reason::BudgetRefuse { est_bytes, budget_bytes } => write!(
                f,
                "an estimated {est_bytes}-byte load cannot be satisfied within the {budget_bytes}-byte AI RAM budget by any eviction of darkmux-owned residents — refused (#1243, applies to every caller intent)"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedAction {
    pub action: Action,
    pub reason: Reason,
    pub precondition: Precondition,
}

/// Non-blocking findings the caller surfaces (CLI print, doctor, flow note).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Warning {
    /// #1243 operator-explicit over-budget: the Load is still emitted
    /// (operator intent wins), the numbers are loud.
    BudgetExceededOperatorOverride {
        est_new_bytes: u64,
        darkmux_resident_bytes: u64,
        budget_bytes: u64,
    },
    /// Reuse at a bigger ctx than requested — the cross-profile
    /// declared-vs-actual breadcrumb, typed (interim provenance until
    /// #1257).
    CtxDivergence { identifier: String, requested: u32, resident: u64 },
    /// Budget accounting degraded: a darkmux-owned resident has unknown
    /// bytes (counted as 0 against the cap — visible, never silent).
    ResidentBytesUnknown { identifier: String },
    /// An Exclusive-scope pass-1 unload is about to evict the registry's
    /// standing utility binding (#1280 guard): the caller either forgot to
    /// include the utility seat in the desired set, or genuinely means to
    /// evict the compactor — either way, loudly.
    UtilityBindingEvicted { identifier: String },
    /// A foreign resident was adopted under
    /// [`crate::planner::ForeignPolicy::AdoptPer408`] (#408 standing
    /// authority) — surfaced, never silent.
    ForeignResidentAdopted { identifier: String, model_key: String },
    /// The estimator could not price a pending load (missing catalog size)
    /// — budget/headroom math counts it as 0 (documented degradation).
    LoadEstimateUnknown { model_key: String },
}

/// #1243 "serialize" arm: every pending load fits the limit alone but not
/// together — the executor runs them one at a time, releasing between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExecHint {
    #[default]
    Concurrent,
    Sequential,
}

/// TOTAL-EQUALITY, DETERMINISTIC-ORDER plan. Ordering contract (tested):
///
/// 1. all Unload actions (Exclusive pass-1 + budget/headroom evictions), in
///    host-reported resident order
/// 2. per-desired decisions (Load/Reuse/Reconcile/Block), in desired-input
///    order
///
/// Unloads-before-loads preserves the RAM-headroom two-pass shape of
/// `swap::swap`. Same input ⇒ identical Plan, always.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Plan {
    pub actions: Vec<PlannedAction>,
    /// #1282 direction: malformed / remote entries, named, never batch-fatal.
    /// [`crate::planner::plan_acquire`] takes already-ingested placements, so
    /// it leaves this empty — callers thread [`crate::desired::ingest`]'s
    /// quarantine list in when assembling the full picture.
    pub quarantined: Vec<crate::desired::Quarantined>,
    /// `SwapResult.user_state_respected` parity — foreign residents
    /// deliberately left alone and unused, surfaced (operator sovereignty,
    /// #44). Populated by Exclusive-scope acquisition (the swap shape) in
    /// host-reported order; Additive plans leave it empty.
    pub user_state_respected: Vec<String>,
    /// Emission order: per-desired decision warnings first (in desired-input
    /// order), then pass-1 warnings, then budget/headroom warnings.
    pub warnings: Vec<Warning>,
    pub exec_hint: ExecHint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_target_claims_namespaced() {
        let t = OwnedTarget::claim("darkmux:m", None).expect("namespaced claims");
        assert_eq!(t.identifier(), "darkmux:m");
    }

    #[test]
    fn owned_target_claims_exact_alias() {
        let t = OwnedTarget::claim("custom", Some("custom")).expect("own alias claims");
        assert_eq!(t.identifier(), "custom");
    }

    #[test]
    fn owned_target_refuses_foreign() {
        // The structural half of the #1284 namespace contract: user state
        // cannot become a mutation target through the normal gate.
        let err = OwnedTarget::claim("their-model", None).unwrap_err();
        assert_eq!(err.identifier, "their-model");
        // A DIFFERENT alias is not this call's own alias — still foreign.
        assert!(OwnedTarget::claim("their-model", Some("custom")).is_err());
    }

    #[test]
    fn owned_target_per_408_claim_is_explicit() {
        // The named authority path exists and produces a working target —
        // its use is gated by ForeignPolicy::AdoptPer408 at the call site.
        let t = OwnedTarget::claim_foreign_per_408("their-model");
        assert_eq!(t.identifier(), "their-model");
    }

    #[test]
    fn budget_evict_display_is_honest_about_ordering() {
        // (#1243 blocking resolution) The eviction order is named honestly:
        // host-reported, never "LRU".
        let r = Reason::BudgetEvict {
            freeing_bytes: 20,
            need_bytes: 15,
            budget_bytes: 30,
            eviction_order: EvictionOrder::HostReported,
        };
        let s = r.to_string();
        assert!(s.contains("host-reported order"), "{s}");
        assert!(!s.contains("LRU eviction"), "{s}");
    }

    #[test]
    fn reason_display_renders_every_variant() {
        // Display is the operator surface — every variant renders non-empty
        // prose, and the Block variants carry their fix hints.
        let all = vec![
            Reason::NoResident,
            Reason::SufficientCtxResident,
            Reason::InsufficientCtx,
            Reason::ForeignResident,
            Reason::UnknownModelKey { nearest: vec!["qwen3-4b".into()] },
            Reason::NoLongerDesired,
            Reason::LastWanterReleased { seats: vec!["a".into(), "b".into()] },
            Reason::BudgetEvict {
                freeing_bytes: 1,
                need_bytes: 2,
                budget_bytes: 3,
                eviction_order: EvictionOrder::HostReported,
            },
            Reason::BudgetRefuse { est_bytes: 22, budget_bytes: 8 },
        ];
        for r in &all {
            assert!(!r.to_string().is_empty(), "{r:?} renders");
        }
        assert!(Reason::ForeignResident.to_string().contains("lms unload"));
        assert!(
            Reason::UnknownModelKey { nearest: vec!["qwen3-4b".into()] }
                .to_string()
                .contains("qwen3-4b")
        );
    }

    #[test]
    fn plan_serializes_for_artifacts() {
        // Serialize-only: a plan can land in a run artifact (OwnedTarget's
        // private identifier included); the absence of Deserialize on
        // Plan/Action/OwnedTarget is a compile-time property.
        let plan = Plan {
            actions: vec![PlannedAction {
                action: Action::Unload { target: OwnedTarget::claim("darkmux:m", None).unwrap() },
                reason: Reason::NoLongerDesired,
                precondition: Precondition::ResidentPresent {
                    identifier: "darkmux:m".into(),
                    at_ctx: Some(8_000),
                },
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&plan).expect("plan serializes");
        assert!(json.contains("darkmux:m"), "{json}");
    }
}
