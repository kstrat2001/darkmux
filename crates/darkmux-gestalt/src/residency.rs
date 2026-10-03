//! The four-way residency split — the validated miniature the gestalt core
//! generalizes (#1274).
//!
//! Ported from the PR-review pipeline's `decide_residency` (PR #1275; that
//! pipeline and its copy are gone since #2310 P4d, so this is the only
//! one). The Reuse / Reconcile / LoadFresh arms were byte-semantic ports;
//! the Blocked-on-foreign arm DELIBERATELY DIVERGED under the
//! absolute-ownership decision (operator-approved 2026-07-10, #1274): a
//! foreign resident sharing the weights is a
//! [`ResidencyDecision::ForeignDuplicate`] fact — respected as pool
//! consumption, never a reuse candidate — and the planner decides
//! load-alongside vs Block-on-capacity. The dispatch preflight in
//! `darkmux_crew::dispatch_internal` (`decide_preflight`, shared with the lab's
//! pre-run warning) has its own exact-identifier reuse rule over the same
//! [`ctx_sufficient`] check; unifying it with this planner is #2985. The tests
//! below carry the ported fixtures.

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
    /// A foreign (user/operator) resident shares the modelKey and NO
    /// darkmux-owned/alias resident does. Under absolute ownership
    /// (operator decision 2026-07-10, #1274) this is a FACT, not a verdict:
    /// the duplicate's load config is unknown (the #1135 ghost) so it is
    /// never a reuse candidate, and it is user state so it is never a
    /// mutation target — the planner loads darkmux's own namespaced copy
    /// alongside when capacity fits, else Blocks naming this instance.
    ForeignDuplicate { foreign_identifier: String },
}

/// Inspect `residents` for one sharing `p`'s modelKey and decide what
/// acquisition should do. Matching on modelKey rather than the namespaced
/// identifier is the point: two different profiles/crews can reference the
/// SAME catalog model under different identifiers (or different `n_ctx`),
/// and a RAM-constrained host can't hold two full concurrent loads of the
/// same weights — the identifier-only check missed that collision and let a
/// doomed second load reach the host's own OOM guardrail (#1271).
///
/// A seat dispatches to its placement's own identifier, so only the resident
/// whose identifier equals it is judged for Reuse (#3074 B10): judging a
/// different `darkmux:` copy of the same modelKey would report Reuse for an
/// instance the dispatch never reaches. With no exact copy, another
/// darkmux-owned copy of the key is never reusable and is never left beside
/// the new load (two full copies of the same weights are the #1271 OOM
/// class), so it is reconciled: unloaded, then the placement loads its own
/// identifier. The claimed-resident guard still refuses to unload one a live
/// lease holds.
///
/// A resident counts as darkmux's own when its identifier is in the
/// `darkmux:` namespace OR equals the exact identifier THIS placement loads
/// under, which covers an explicit alias, the documented namespace opt-out.
///
/// Ownership partitions BEFORE matching (absolute ownership, 2026-07-10,
/// #1274): foreign residents are never candidates, only facts, and a foreign
/// copy listed ahead of a darkmux copy does not shadow it. Only when no
/// owned resident shares the modelKey does a foreign one surface, as
/// [`ResidencyDecision::ForeignDuplicate`].
pub fn decide_residency(residents: &[ResidentFact], p: &Placement) -> ResidencyDecision {
    let same_key = || residents.iter().filter(|r| r.model_key == p.model_key);
    let exact = same_key().find(|r| r.identifier == p.identifier);
    let other_owned = same_key().find(|r| is_darkmux_owned(&r.identifier));
    if let Some(found) = exact {
        return if ctx_sufficient(found.ctx, p.min_ctx) {
            ResidencyDecision::Reuse {
                identifier: found.identifier.clone(),
                resident_ctx: found.ctx,
            }
        } else {
            ResidencyDecision::Reconcile {
                stale_identifier: found.identifier.clone(),
                stale_ctx: found.ctx,
            }
        };
    }
    if let Some(other) = other_owned {
        return ResidencyDecision::Reconcile {
            stale_identifier: other.identifier.clone(),
            stale_ctx: other.ctx,
        };
    }
    match same_key().next() {
        Some(foreign) => {
            ResidencyDecision::ForeignDuplicate { foreign_identifier: foreign.identifier.clone() }
        }
        None => ResidencyDecision::LoadFresh,
    }
}

#[cfg(test)]
mod tests {
    //! Golden arm-for-arm fixtures lifted from the PR-review pipeline's
    //! residency tests (the #1271 `LmsCycler` suite, since deleted) — same
    //! residents, same wanted ctx, same expected arm.

    use super::*;

    fn resident(identifier: &str, model_key: &str, ctx: u64) -> ResidentFact {
        ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx,
            est_bytes: Some(14_000_000_000),
            parallel: 0,
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
        // Review fixture (d): empty residents → plain load.
        let p = placement("devstral", 32_768, None);
        assert_eq!(decide_residency(&[], &p), ResidencyDecision::LoadFresh);
    }

    #[test]
    fn reuse_when_darkmux_owned_sufficient() {
        // Review fixture (b): darkmux:devstral @ 40960 satisfies 32768.
        let residents = vec![resident("darkmux:devstral", "devstral", 40_960)];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reuse { identifier: "darkmux:devstral".into(), resident_ctx: 40_960 }
        );
    }

    #[test]
    fn reconcile_when_darkmux_owned_insufficient() {
        // Review fixture (a): darkmux:devstral @ 20000 is undersized for
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
    fn foreign_duplicate_when_foreign_shares_model_key() {
        // Review fixture (c), UPDATED: named divergence, absolute-ownership
        // decision, operator-approved 2026-07-10, #1274. devstral-manual is
        // operator state — never touched, never reused (even at sufficient
        // ctx: its load config is the #1135 ghost); surfaced as a
        // ForeignDuplicate fact for the planner's load-alongside-or-Block
        // call. The review Blocked outright here.
        let residents = vec![resident("devstral-manual", "devstral", 40_960)];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::ForeignDuplicate { foreign_identifier: "devstral-manual".into() }
        );
    }

    #[test]
    fn explicit_alias_sufficient_reuses_not_blocked() {
        // The review's explicit-alias fixture: a resident under the
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
    fn foreign_first_no_longer_shadows_darkmux_copy() {
        // Review multi-resident fixture, UPDATED: named divergence,
        // absolute-ownership decision, operator-approved 2026-07-10, #1274.
        // Ownership partitions before matching: a user-owned copy listed
        // ahead of a darkmux-stale instance no longer shadows it — OUR copy
        // reconciles (ours to fix), the user copy stays untouched pool
        // consumption. The review Blocked on the first-match foreign here.
        let residents = vec![
            resident("devstral-manual", "devstral", 40_960),
            resident("darkmux:devstral", "devstral", 20_000),
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

    #[test]
    fn foreign_duplicate_only_when_no_owned_resident() {
        // The partition boundary from the other side: an owned SUFFICIENT
        // copy behind a foreign one reuses — ForeignDuplicate surfaces only
        // when no owned/alias resident shares the modelKey at all.
        let residents = vec![
            resident("devstral-manual", "devstral", 20_000),
            resident("darkmux:devstral", "devstral", 40_960),
        ];
        let p = placement("devstral", 32_768, None);
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reuse { identifier: "darkmux:devstral".into(), resident_ctx: 40_960 }
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

    #[test]
    fn other_darkmux_copy_is_not_a_reuse_candidate() {
        // #3074 B10 S1/S3: a different darkmux: copy of the key is not the
        // instance the seat addresses on the wire: it is replaced, never
        // reused, even when its ctx would suffice.
        let p = placement("m", 32_000, Some("darkmux:m"));
        let big_other = vec![resident("darkmux:other", "m", 100_000)];
        assert_eq!(
            decide_residency(&big_other, &p),
            ResidencyDecision::Reconcile {
                stale_identifier: "darkmux:other".into(),
                stale_ctx: 100_000
            }
        );
        let both = vec![resident("darkmux:other", "m", 100_000), resident("darkmux:m", "m", 4_096)];
        assert_eq!(
            decide_residency(&both, &p),
            ResidencyDecision::Reconcile { stale_identifier: "darkmux:m".into(), stale_ctx: 4_096 }
        );
    }

    #[test]
    fn exact_identifier_wins_over_earlier_darkmux_copy() {
        // #3074 B10 S2: the placement's own copy is judged even when another
        // darkmux: copy is listed first and is undersized.
        let p = placement("m", 32_000, Some("darkmux:m"));
        let residents =
            vec![resident("darkmux:other", "m", 4_096), resident("darkmux:m", "m", 100_000)];
        assert_eq!(
            decide_residency(&residents, &p),
            ResidencyDecision::Reuse { identifier: "darkmux:m".into(), resident_ctx: 100_000 }
        );
    }
}
