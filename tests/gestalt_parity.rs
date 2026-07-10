//! (#1274 packet 1) Cross-crate parity — the anti-fork guard for the
//! deliberate duplication window between `darkmux_gestalt::ownership` /
//! `::residency` and their canonical sources (`darkmux_profiles::swap`, the
//! funnel's private `decide_residency` in darkmux-lab).
//!
//! Packet 3 re-points swap.rs at the gestalt crate (a thin delegating
//! wrapper for the `&ProfileModel` form — a bare `pub use` can't bridge the
//! signature — and `pub use` for the rest), collapsing the duplication.
//! Until then these tests run BOTH implementations over the SAME vectors
//! swap.rs's own tests use, plus the funnel's residency fixtures copied
//! verbatim (the funnel's function is private by design — the vectors are
//! lifted, not the symbol), so the two cannot drift apart silently.

use darkmux_gestalt::{decide_residency, Placement, ResidencyDecision, ResidentFact};
use darkmux_profiles::swap;
use darkmux_types::ProfileModel;

fn profile_model(id: &str, n_ctx: u32, identifier: Option<&str>) -> ProfileModel {
    ProfileModel {
        id: id.to_string(),
        n_ctx,
        identifier: identifier.map(str::to_string),
        capabilities: Default::default(),
        endpoint: None,
        extras: Default::default(),
    }
}

/// The thin bridging wrapper the packet-3 cutover formalizes: gestalt's
/// two-parameter form fed from a ProfileModel, compared against swap's.
fn gestalt_namespaced(pm: &ProfileModel) -> String {
    darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref())
}

#[test]
fn ownership_parity_namespaced_identifier() {
    // The exact vectors from swap.rs's own tests.
    let bare = profile_model("qwen3.6-35b-a3b", 100_000, None);
    assert_eq!(swap::namespaced_identifier(&bare), gestalt_namespaced(&bare));
    assert_eq!(gestalt_namespaced(&bare), "darkmux:qwen3.6-35b-a3b");

    let aliased = profile_model("qwen3.6-35b-a3b", 100_000, Some("my-custom-alias"));
    assert_eq!(swap::namespaced_identifier(&aliased), gestalt_namespaced(&aliased));
    assert_eq!(gestalt_namespaced(&aliased), "my-custom-alias");
}

#[test]
fn ownership_parity_is_darkmux_owned() {
    // The exact vectors from swap.rs's own tests — positives, user state,
    // and the partial-prefix negatives.
    for identifier in [
        "darkmux:qwen3.6-35b-a3b",
        "darkmux:anything-after",
        "qwen3.6-35b-a3b",
        "user-loaded-model",
        "my-custom-alias",
        "dark:foo",
        "predarkmux:foo",
    ] {
        assert_eq!(
            swap::is_darkmux_owned(identifier),
            darkmux_gestalt::is_darkmux_owned(identifier),
            "ownership disagreement on {identifier:?}"
        );
    }
    assert_eq!(swap::DARKMUX_LMS_NAMESPACE, darkmux_gestalt::DARKMUX_NAMESPACE);
}

#[test]
fn ownership_parity_ctx_sufficient() {
    // The exact vectors from swap.rs's own tests (#600 minimum semantics,
    // #906 u64 compare), plus the u32::MAX boundary.
    for (loaded, wanted) in [
        (200_000_u64, 64_000_u32),
        (64_000, 64_000),
        (64_000, 200_000),
        (u64::from(u32::MAX) + 1, u32::MAX),
        (0, 1),
    ] {
        assert_eq!(
            swap::ctx_sufficient(loaded, wanted),
            darkmux_gestalt::ctx_sufficient(loaded, wanted),
            "ctx_sufficient disagreement on ({loaded}, {wanted})"
        );
    }
}

// ── decide_residency arm-for-arm parity ──────────────────────────────────
//
// Fixtures copied from the funnel's LmsCycler residency tests (#1271 suite,
// darkmux-lab funnel.rs): same residents (identifier/modelKey/ctx), same
// wanted n_ctx, and the arm each fixture asserts behaviorally there
// (reconcile = unload-then-reload, reuse = no-op, blocked = loud error
// naming the resident, load-fresh = plain load).

fn resident(identifier: &str, model_key: &str, ctx: u64) -> ResidentFact {
    ResidentFact {
        identifier: identifier.to_string(),
        model_key: model_key.to_string(),
        ctx,
        est_bytes: Some(14_000_000_000),
    }
}

fn placement_for(pm: &ProfileModel) -> Placement {
    Placement {
        model_key: pm.id.clone(),
        identifier: swap::namespaced_identifier(pm),
        min_ctx: pm.n_ctx,
        seat: "parity".to_string(),
    }
}

#[test]
fn decide_residency_funnel_arm_parity() {
    let pm = profile_model("devstral", 32_768, None);
    let pm_alias = profile_model("devstral", 32_768, Some("custom"));

    // (a) darkmux-owned, insufficient ctx → Reconcile (the #1271 repro).
    assert_eq!(
        decide_residency(&[resident("darkmux:devstral", "devstral", 20_000)], &placement_for(&pm)),
        ResidencyDecision::Reconcile {
            stale_identifier: "darkmux:devstral".into(),
            stale_ctx: 20_000,
        }
    );

    // (b) darkmux-owned, sufficient ctx → Reuse (no load, no unload).
    assert_eq!(
        decide_residency(&[resident("darkmux:devstral", "devstral", 40_960)], &placement_for(&pm)),
        ResidencyDecision::Reuse { identifier: "darkmux:devstral".into(), resident_ctx: 40_960 }
    );

    // (c) foreign resident shares the modelKey → Blocked, naming it.
    assert_eq!(
        decide_residency(&[resident("devstral-manual", "devstral", 40_960)], &placement_for(&pm)),
        ResidencyDecision::Blocked { resident_identifier: "devstral-manual".into() }
    );

    // (d) no resident shares the modelKey → LoadFresh.
    assert_eq!(decide_residency(&[], &placement_for(&pm)), ResidencyDecision::LoadFresh);

    // Explicit alias, sufficient → Reuse, never Blocked (the namespace
    // opt-out is darkmux's own load).
    assert_eq!(
        decide_residency(&[resident("custom", "devstral", 32_768)], &placement_for(&pm_alias)),
        ResidencyDecision::Reuse { identifier: "custom".into(), resident_ctx: 32_768 }
    );

    // Explicit alias, insufficient → Reconcile under the same alias.
    assert_eq!(
        decide_residency(&[resident("custom", "devstral", 20_000)], &placement_for(&pm_alias)),
        ResidencyDecision::Reconcile { stale_identifier: "custom".into(), stale_ctx: 20_000 }
    );

    // Multi-resident, user-owned FIRST → Blocked (first match decides even
    // with a darkmux instance later in host order).
    assert_eq!(
        decide_residency(
            &[
                resident("devstral-manual", "devstral", 40_960),
                resident("darkmux:devstral", "devstral", 20_000),
            ],
            &placement_for(&pm)
        ),
        ResidencyDecision::Blocked { resident_identifier: "devstral-manual".into() }
    );

    // Mirror ordering: darkmux-stale first → Reconcile touching ONLY the
    // darkmux instance.
    assert_eq!(
        decide_residency(
            &[
                resident("darkmux:devstral", "devstral", 20_000),
                resident("devstral-manual", "devstral", 40_960),
            ],
            &placement_for(&pm)
        ),
        ResidencyDecision::Reconcile {
            stale_identifier: "darkmux:devstral".into(),
            stale_ctx: 20_000,
        }
    );
}
