//! The darkmux ownership boundary, as pure string predicates.
//!
//! Canonical home going forward for the namespace helpers that today also
//! live in `darkmux_profiles::swap` (the #52 namespace convention). Packet 3
//! re-points swap.rs at this module (a thin delegating wrapper for the
//! `&ProfileModel` form, `pub use` for the rest) so the funnel's `LmsCycler`
//! and the crew dispatch path keep exactly ONE definition — the #1271
//! discipline. Until that cutover lands, the root-crate
//! `tests/gestalt_parity.rs` asserts these functions agree with swap's over
//! swap's own test vectors, so the duplication window cannot fork.

/// Prefix attached to identifiers darkmux uses for its own host loads.
/// Anything visible in host residency starting with this prefix is owned by
/// darkmux and eligible for gestalt-planned mutation; anything else is user
/// state and off-limits by construction (#52, operator sovereignty applied
/// at model-state level).
pub const DARKMUX_NAMESPACE: &str = "darkmux:";

/// Compute the identifier a placement loads under.
///
/// An explicit `identifier` passes through VERBATIM — the documented
/// namespace opt-out for operators with special cases. Otherwise the model
/// key is wrapped under the `darkmux:` namespace so ownership filtering can
/// distinguish darkmux's loads from user-managed ones. Same semantics as
/// `darkmux_profiles::swap::namespaced_identifier(&ProfileModel)`, with the
/// two inputs that function reads off the model made explicit parameters.
///
/// Normalize guard: a `model_key` ALREADY carrying the `darkmux:` prefix is
/// returned as-is, never double-prefixed — the same dual-form tolerance
/// swap.rs applies one layer up in `utility_load_target` (operators store
/// either the bare LMStudio key or the namespaced identifier; a
/// `darkmux:darkmux:…` identifier would escape every ownership filter's
/// unload scope while still matching `is_darkmux_owned`).
pub fn namespaced_identifier(model_key: &str, explicit: Option<&str>) -> String {
    if let Some(explicit) = explicit {
        return explicit.to_string();
    }
    if model_key.starts_with(DARKMUX_NAMESPACE) {
        return model_key.to_string();
    }
    format!("{DARKMUX_NAMESPACE}{model_key}")
}

/// `true` if this identifier was minted by darkmux (begins with our
/// namespace). A pure prefix check — the namespace IS the ownership record;
/// there is no separate ledger to go stale.
pub fn is_darkmux_owned(identifier: &str) -> bool {
    identifier.starts_with(DARKMUX_NAMESPACE)
}

/// Does a model already loaded at `loaded_ctx` satisfy a placement that
/// wants `wanted_n_ctx`? `n_ctx` is a **minimum**, not an exact size (#600):
/// a model loaded with *at least* the wanted context satisfies the request,
/// so planning keeps it rather than reloading it smaller — a larger context
/// is strictly more capable, and the operator who loaded it bigger has the
/// RAM for it. Only an *insufficient* load triggers a reconcile.
///
/// (#906) Compared in u64 — truncating a very large loaded context to u32
/// before the check could wrap it below the wanted minimum and trigger a
/// needless reload.
pub fn ctx_sufficient(loaded_ctx: u64, wanted_n_ctx: u32) -> bool {
    loaded_ctx >= u64::from(wanted_n_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors mirroring `darkmux_profiles::swap`'s own tests — the
    // in-crate half of the anti-fork guard (the cross-crate half lives in
    // the root crate's tests/gestalt_parity.rs).

    #[test]
    fn namespaced_identifier_wraps_bare_key() {
        assert_eq!(
            namespaced_identifier("qwen3.6-35b-a3b", None),
            "darkmux:qwen3.6-35b-a3b"
        );
    }

    #[test]
    fn namespaced_identifier_passes_through_explicit_alias() {
        // Explicit override wins — operator opted out of the auto-namespace.
        assert_eq!(
            namespaced_identifier("qwen3.6-35b-a3b", Some("my-custom-alias")),
            "my-custom-alias"
        );
    }

    #[test]
    fn namespaced_identifier_never_double_prefixes() {
        // The double-prefix hazard: a pre-namespaced key (operators store
        // either form — swap.rs's utility_load_target dual-form tolerance)
        // must come back unchanged. `darkmux:darkmux:…` would pass
        // is_darkmux_owned yet match no resident any consumer ever loaded.
        assert_eq!(
            namespaced_identifier("darkmux:qwen3-4b-instruct-2507", None),
            "darkmux:qwen3-4b-instruct-2507"
        );
        // The explicit-alias passthrough stays verbatim even when the alias
        // itself is namespaced — operator intent, not a normalize target.
        assert_eq!(
            namespaced_identifier("qwen3-4b-instruct-2507", Some("darkmux:qwen3-4b-instruct-2507")),
            "darkmux:qwen3-4b-instruct-2507"
        );
    }

    #[test]
    fn is_darkmux_owned_detects_namespace() {
        assert!(is_darkmux_owned("darkmux:qwen3.6-35b-a3b"));
        assert!(is_darkmux_owned("darkmux:anything-after"));
        // Non-namespaced ids are user state — off-limits.
        assert!(!is_darkmux_owned("qwen3.6-35b-a3b"));
        assert!(!is_darkmux_owned("user-loaded-model"));
        assert!(!is_darkmux_owned("my-custom-alias"));
        // Partial match isn't enough.
        assert!(!is_darkmux_owned("dark:foo"));
        assert!(!is_darkmux_owned("predarkmux:foo"));
    }

    #[test]
    fn ctx_sufficient_treats_n_ctx_as_a_minimum() {
        // The motivating case: a model loaded LARGER than the placement
        // wants is kept — no reload-down.
        assert!(ctx_sufficient(200_000, 64_000));
        // Exactly enough is fine.
        assert!(ctx_sufficient(64_000, 64_000));
        // Only an insufficient load triggers a reconcile.
        assert!(!ctx_sufficient(64_000, 200_000));
    }
}
