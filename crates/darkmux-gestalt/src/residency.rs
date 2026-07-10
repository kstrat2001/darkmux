//! The four-way residency split — the validated miniature the gestalt core
//! generalizes (#1274).
//!
//! Absorbed byte-semantically from the review funnel's `decide_residency`
//! (darkmux-lab funnel.rs, PR #1275), now `pub` and fact-typed. The
//! root-crate `tests/gestalt_parity.rs` proves arm-for-arm agreement against
//! the funnel's own test fixtures — the absorption is proven, not asserted.

use crate::desired::Placement;
use crate::facts::ResidentFact;
use crate::ownership::{ctx_sufficient, is_darkmux_owned};
use serde::{Deserialize, Serialize};

/// What acquiring `Placement` should do about the current residents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResidencyDecision {
    /// No resident shares this placement's modelKey — load fresh.
    LoadFresh,
    /// A resident darkmux may manage already satisfies the ctx requirement —
    /// nothing to load. Carries the resident's identity + actual ctx so the
    /// caller can leave a declared-vs-actual breadcrumb when they diverge
    /// (interim provenance until #1257 lands).
    Reuse { identifier: String, resident_ctx: u64 },
    /// A resident darkmux may manage shares the modelKey but is loaded at an
    /// insufficient ctx — unload it, then load fresh at the required ctx.
    /// Silently reusing the wrong ctx is the #1135 bug class and is not an
    /// option; attempting a second concurrent load of the same weights is
    /// the #1271 bug class (OOM) and isn't either.
    Reconcile { stale_identifier: String, stale_ctx: u64 },
    /// A resident shares the modelKey but is NOT one darkmux may manage —
    /// operator state the plan must never touch. Fail loud before spending
    /// a load attempt the host's own guardrail would refuse anyway.
    Blocked { resident_identifier: String },
}

/// Inspect `residents` for one sharing `p`'s modelKey and decide what
/// acquisition should do. Matching on modelKey rather than the namespaced
/// identifier is the point: two different profiles/crews can reference the
/// SAME catalog model under different identifiers (or different `n_ctx`),
/// and a RAM-constrained host can't hold two full concurrent loads of the
/// same weights — the identifier-only check missed that collision and let a
/// doomed second load reach the host's own OOM guardrail (#1271).
///
/// A resident counts as darkmux's own when its identifier is in the
/// `darkmux:` namespace OR equals the exact identifier THIS placement loads
/// under — the second arm covers an explicit alias, the documented namespace
/// opt-out, whose resident must not misclassify as foreign user state and
/// get Blocked against darkmux's own load.
///
/// Multiple residents sharing the modelKey: the FIRST match (in host-reported
/// order) decides. A first-match user-owned resident blocks even when a
/// darkmux-owned instance also sits further down the list — the operator's
/// copy of the weights is resident either way, and any load attempt in that
/// state still risks the double-footprint the host's guardrail refuses.
pub fn decide_residency(residents: &[ResidentFact], p: &Placement) -> ResidencyDecision {
    let Some(found) = residents.iter().find(|r| r.model_key == p.model_key) else {
        return ResidencyDecision::LoadFresh;
    };
    let ours = is_darkmux_owned(&found.identifier) || found.identifier == p.identifier;
    if !ours {
        return ResidencyDecision::Blocked { resident_identifier: found.identifier.clone() };
    }
    if ctx_sufficient(found.ctx, p.min_ctx) {
        ResidencyDecision::Reuse {
            identifier: found.identifier.clone(),
            resident_ctx: found.ctx,
        }
    } else {
        ResidencyDecision::Reconcile {
            stale_identifier: found.identifier.clone(),
            stale_ctx: found.ctx,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Golden arm-for-arm fixtures lifted from the funnel's own residency
    //! tests (the #1271 `LmsCycler` suite) — same residents, same wanted
    //! ctx, same expected arm. The cross-crate copy of these vectors lives
    //! in the root crate's tests/gestalt_parity.rs.

    use super::*;

    fn resident(identifier: &str, model_key: &str, ctx: u64) -> ResidentFact {
        ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx,
            est_bytes: Some(14_000_000_000),
        }
    }

    fn placement(model_key: &str, min_ctx: u32, explicit: Option<&str>) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: crate::ownership::namespaced_identifier(model_key, explicit),
            min_ctx,
            seat: "test".to_string(),
        }
    }

    #[test]
    fn load_fresh_when_no_resident_shares_model_key() {
        // Funnel fixture (d): empty residents → plain load.
        let p = placement("devstral", 32_768, None);
        assert_eq!(decide_residency(&[], &p), ResidencyDecision::LoadFresh);
    }

    #[test]
    fn reuse_when_darkmux_owned_sufficient() {
        // Funnel fixture (b): darkmux:devstral @ 40960 satisfies 32768.
        let residents = vec![resident("darkmux:devstral", "devstral", 40_960)];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reuse { identifier: "darkmux:devstral".into(), resident_ctx: 40_960 }
        );
    }

    #[test]
    fn reconcile_when_darkmux_owned_insufficient() {
        // Funnel fixture (a): darkmux:devstral @ 20000 is undersized for
        // 32768 — the exact #1271 repro shape.
        let residents = vec![resident("darkmux:devstral", "devstral", 20_000)];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reconcile {
                stale_identifier: "darkmux:devstral".into(),
                stale_ctx: 20_000,
            }
        );
    }

    #[test]
    fn blocked_when_foreign_shares_model_key() {
        // Funnel fixture (c): devstral-manual is operator state — blocked,
        // never touched, regardless of its ctx.
        let residents = vec![resident("devstral-manual", "devstral", 40_960)];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Blocked { resident_identifier: "devstral-manual".into() }
        );
    }

    #[test]
    fn explicit_alias_sufficient_reuses_not_blocked() {
        // The funnel's explicit-alias fixture: a resident under the
        // placement's own alias is OURS (the namespace opt-out), never
        // foreign.
        let residents = vec![resident("custom", "devstral", 32_768)];
        let p = placement("devstral", 32_768, Some("custom"));
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reuse { identifier: "custom".into(), resident_ctx: 32_768 }
        );
    }

    #[test]
    fn explicit_alias_insufficient_reconciles() {
        let residents = vec![resident("custom", "devstral", 20_000)];
        let p = placement("devstral", 32_768, Some("custom"));
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reconcile { stale_identifier: "custom".into(), stale_ctx: 20_000 }
        );
    }

    #[test]
    fn first_match_foreign_blocks_despite_later_darkmux() {
        // Funnel multi-resident fixture: user-owned listed ahead of a
        // darkmux-stale instance → Blocked; order-dependence is asserted
        // behavior, not an implementation detail.
        let residents = vec![
            resident("devstral-manual", "devstral", 40_960),
            resident("darkmux:devstral", "devstral", 20_000),
        ];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Blocked { resident_identifier: "devstral-manual".into() }
        );
    }

    #[test]
    fn first_match_darkmux_stale_reconciles_despite_later_foreign() {
        // Mirror ordering: darkmux-stale first → Reconcile, touching ONLY
        // the darkmux instance.
        let residents = vec![
            resident("darkmux:devstral", "devstral", 20_000),
            resident("devstral-manual", "devstral", 40_960),
        ];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reconcile {
                stale_identifier: "darkmux:devstral".into(),
                stale_ctx: 20_000,
            }
        );
    }
}
