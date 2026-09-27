//! The darkmux ownership boundary, as pure string predicates.
//!
//! The one definition of the #52 namespace convention: `darkmux_profiles::
//! swap` delegates to these helpers (its `&ProfileModel` wrapper and
//! `DARKMUX_LMS_NAMESPACE` re-export) rather than carrying its own copy —
//! the #1271 one-definition discipline.

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

/// The loadable model key for a value that may carry the darkmux namespace:
/// `darkmux:foo` becomes `foo`; anything without the prefix (a bare key, or
/// a user's own identifier) comes back unchanged. The inverse of
/// [`namespaced_identifier`]'s default wrap. Strips ONE prefix: a doubled
/// `darkmux:darkmux:…` is never minted (see the normalize guard above), so
/// it is not normalized here either.
///
/// The namespace is a load-time decoration, never part of the key: `lms ps`
/// reports a darkmux load as `identifier=darkmux:foo, modelKey=foo`, so a
/// comparison or load against a prefixed string always misses.
pub fn bare_model_key(value: &str) -> &str {
    value.strip_prefix(DARKMUX_NAMESPACE).unwrap_or(value)
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

    #[test]
    fn bare_model_key_strips_only_a_leading_namespace() {
        assert_eq!(bare_model_key("darkmux:qwen3-4b"), "qwen3-4b");
        // Inverse: anything not carrying the prefix comes back unchanged.
        assert_eq!(bare_model_key("qwen3-4b"), "qwen3-4b");
        assert_eq!(bare_model_key("my-alias"), "my-alias");
        assert_eq!(bare_model_key("xdarkmux:qwen"), "xdarkmux:qwen");
        assert_eq!(bare_model_key(""), "");
        // One prefix only.
        assert_eq!(bare_model_key("darkmux:darkmux:q"), "darkmux:q");
        // Round trip with the default wrap.
        assert_eq!(bare_model_key(&namespaced_identifier("q", None)), "q");
    }

    // Golden vectors mirroring `darkmux_profiles::swap`'s own tests.

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
