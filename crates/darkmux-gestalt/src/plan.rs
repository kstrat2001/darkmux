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
/// Every Unload target holds an `OwnedTarget`, and
/// [`crate::ports::ModelHost::unload`] accepts nothing else. The field is
/// private and the only constructor is the claim gate below, so a plan that
/// mutates a foreign resident is UNREPRESENTABLE — absolute namespace
/// ownership (operator decision 2026-07-10, #1274): user state is
/// structurally unnameable in plan actions, with no exception path (the
/// former #408 adoption constructor is deleted, superseded by that
/// decision). `Serialize`-only (no `Deserialize`): serialized plans are
/// artifacts for humans and run records, and cannot be deserialized back
/// into executable form.
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
    /// No resident shares this modelKey (guards a fresh Load — a resident
    /// appearing since plan time means the plan is stale).
    NoResidentForModelKey { model_key: String },
    /// No darkmux-owned resident — nor one under `identifier`, the exact
    /// alias this load uses — shares this modelKey (guards a reconcile
    /// reload after its free-phase unload, and a load-alongside Load whose
    /// foreign duplicate is EXPECTED to remain resident; an OWNED copy
    /// appearing since plan time means the plan is stale).
    NoOwnedResidentForModelKey { model_key: String, identifier: String },
    /// The named resident is still present (guards an Unload);
    /// `at_ctx` pins the observed ctx when known.
    ResidentPresent { identifier: String, at_ctx: Option<u64> },
    /// No precondition — only ever carried by non-mutating actions
    /// (Reuse/Block). Every mutating action carries a real one (invariant,
    /// asserted globally in the planner tests).
    None,
}

/// The #1274 action vocabulary — decide_residency's shape (PR #1275) plus
/// batch-level Unload. A reconcile is TWO actions — its Unload half rides
/// the free phase and its Load half the load phase (both carrying
/// [`Reason::InsufficientCtx`]) — so ALL frees precede ALL loads, the
/// `swap::swap` RAM-headroom shape (see the [`Plan`] ordering contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Action {
    Load { model_key: String, identifier: String, min_ctx: u32 },
    Unload { target: OwnedTarget },
    Reuse { identifier: String, resident_ctx: u64, min_ctx: u32 },
    Block { model_key: String, resident_identifier: Option<String> },
}

impl Action {
    /// Does executing this action mutate host state? Mutating actions must
    /// carry a non-[`Precondition::None`] precondition (asserted globally in
    /// the planner tests) — the executor's re-verify contract keys on this.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Action::Load { .. } | Action::Unload { .. })
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
    /// Reconcile pair: resident undersized — the #1135 class; a second
    /// concurrent load of the same weights is the #1271 OOM class, so
    /// unload-then-load. Carried by BOTH halves: the free-phase Unload of
    /// the stale and the load-phase reload at the required ctx.
    InsufficientCtx,
    /// Load (alongside): a foreign (user/operator) resident shares the
    /// weights, but its load configuration is unknown (the #1135 ghost) —
    /// never reused, never touched; darkmux loads its own namespaced copy
    /// beside it (absolute ownership, operator decision 2026-07-10, #1274).
    /// Always paired with [`Warning::ForeignDuplicateResident`].
    ForeignDuplicateLoadAlongside { foreign_identifier: String },
    /// Block: the pool cannot hold darkmux's own copy of the weights
    /// alongside the user-loaded duplicate, and that duplicate is user state
    /// darkmux may not free (absolute ownership, #1274). `limit_bytes` is
    /// the effective pool headroom after every planned free. Names the
    /// blocking instance and carries the eject-or-load-via-darkmux
    /// suggestion in its Display.
    ForeignDuplicateNoCapacity {
        foreign_identifier: String,
        foreign_bytes: Option<u64>,
        est_bytes: u64,
        limit_bytes: u64,
    },
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

/// Display-only GB rendering for the operator-facing suggestion strings
/// (the plan data itself stays exact bytes — floats never enter `Eq` types).
fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
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
                "the resident context is below the required minimum — the stale instance is unloaded in the free phase and reloaded at the required context in the load phase (silently reusing the wrong context is the #1135 bug class)"
            ),
            Reason::ForeignDuplicateLoadAlongside { foreign_identifier } => write!(
                f,
                "user-loaded instance \"{foreign_identifier}\" of the needed model is resident, but its load configuration is unknown (the #1135 ghost) — never reused; loading darkmux's own namespaced copy alongside it (absolute namespace ownership, #1274)"
            ),
            Reason::ForeignDuplicateNoCapacity {
                foreign_identifier,
                foreign_bytes,
                est_bytes,
                limit_bytes,
            } => {
                match foreign_bytes {
                    Some(b) => write!(
                        f,
                        "user-loaded instance \"{foreign_identifier}\" of the needed model occupies {}",
                        gb(*b)
                    )?,
                    None => write!(
                        f,
                        "user-loaded instance \"{foreign_identifier}\" of the needed model occupies an unknown amount of memory"
                    )?,
                }
                write!(
                    f,
                    "; eject it (`lms unload \"{foreign_identifier}\"`) or load it via darkmux — darkmux never touches user state (absolute namespace ownership, #1274), and its own estimated {} copy cannot fit alongside within the {} of pool headroom left after every planned free",
                    gb(*est_bytes),
                    gb(*limit_bytes)
                )
            }
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
    /// A user-loaded duplicate of a desired model is resident (absolute
    /// ownership, #1274): respected as pool consumption, never reused —
    /// darkmux plans its own namespaced copy alongside. Names the duplicate
    /// and its pool cost (`None` = the adapter could not size it).
    ForeignDuplicateResident { foreign_identifier: String, est_bytes: Option<u64> },
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
/// 1. FREE phase: every Unload — Exclusive pass-1 unloads, the unload
///    halves of reconciles, and budget/headroom evictions — in
///    host-reported resident order
/// 2. LOAD phase: per-desired decisions (Load/Reuse/Block), in
///    desired-input order
///
/// ALL frees precede ALL loads — the RAM-headroom two-pass shape of
/// `swap::swap` (free-then-load). An earlier draft claimed this parity while
/// carrying each reconcile's unload inside the load phase, interleaving a
/// free after other loads; the reconcile split into a free-phase Unload +
/// load-phase Load is the review MUST_FIX that restored the shape. Same
/// input ⇒ identical Plan, always.
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
        // The structural half of the #1284 namespace contract, absolute
        // under the 2026-07-10 operator decision (#1274): user state cannot
        // become a mutation target — the claim gate is the ONLY constructor
        // (the former #408 adoption bypass is deleted).
        let err = OwnedTarget::claim("their-model", None).unwrap_err();
        assert_eq!(err.identifier, "their-model");
        // A DIFFERENT alias is not this call's own alias — still foreign.
        assert!(OwnedTarget::claim("their-model", Some("custom")).is_err());
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
            Reason::ForeignDuplicateLoadAlongside { foreign_identifier: "m-manual".into() },
            Reason::ForeignDuplicateNoCapacity {
                foreign_identifier: "m-manual".into(),
                foreign_bytes: Some(30_000_000_000),
                est_bytes: 15_000_000_000,
                limit_bytes: 10_000_000_000,
            },
            Reason::ForeignDuplicateNoCapacity {
                foreign_identifier: "m-manual".into(),
                foreign_bytes: None,
                est_bytes: 15_000_000_000,
                limit_bytes: 10_000_000_000,
            },
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
        // The capacity Block names the foreign instance, its pool cost, and
        // the eject-or-load-via-darkmux suggestion (operator decision
        // 2026-07-10, #1274).
        let s = Reason::ForeignDuplicateNoCapacity {
            foreign_identifier: "m-manual".into(),
            foreign_bytes: Some(30_000_000_000),
            est_bytes: 15_000_000_000,
            limit_bytes: 10_000_000_000,
        }
        .to_string();
        assert!(s.contains("\"m-manual\""), "{s}");
        assert!(s.contains("occupies 30.0 GB"), "{s}");
        assert!(s.contains("lms unload"), "{s}");
        assert!(s.contains("load it via darkmux"), "{s}");
        assert!(
            Reason::ForeignDuplicateLoadAlongside { foreign_identifier: "m-manual".into() }
                .to_string()
                .contains("never reused")
        );
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
