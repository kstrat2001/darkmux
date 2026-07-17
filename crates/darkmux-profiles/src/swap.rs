//! darkmux's LMStudio namespace helpers — the ownership contract for loaded
//! models (`darkmux:<model-id>` identifiers).
//!
//! (#1426 phase 3) This module used to hold the `darkmux swap` stack-swap
//! orchestration. That verb retired (gestalt is the one residency writer —
//! a dispatch loads what its staffing needs), and the whole swap executor
//! (`swap()`, `SwapOpts`/`SwapResult`, the desired-loads resolver, and the
//! `RegistryHooks` pre/post-swap runner — the hooks retired WITH the verb,
//! since swap was their only trigger) was deleted with it. What remains is
//! the namespace vocabulary every production consumer still uses:
//! [`DARKMUX_LMS_NAMESPACE`], [`namespaced_identifier`], and
//! [`is_darkmux_owned`].

use darkmux_types::ProfileModel;

/// Prefix attached to identifiers darkmux uses for its own LMStudio loads.
/// Anything visible via `lms ps` starting with this prefix is owned by darkmux
/// and safe for darkmux to unload; anything else is user state and off-limits.
///
/// (#1230 Packet 1 cutover) Re-exported from `darkmux_gestalt::ownership`,
/// which is now the canonical home for this constant — see that module's
/// doc comment ("Packet 3 re-points swap.rs at this module"). Kept as a
/// `pub const` alias here (not a bare re-export of a differently-named
/// item) so every existing `swap::DARKMUX_LMS_NAMESPACE` call site keeps
/// compiling unchanged.
///
/// See [issue #52](https://github.com/kstrat2001/darkmux/issues/52) for the
/// design rationale (operator-sovereignty applied at model-state level —
/// darkmux never touches state it didn't bring up).
pub const DARKMUX_LMS_NAMESPACE: &str = darkmux_gestalt::DARKMUX_NAMESPACE;

/// Compute the darkmux-namespaced LMStudio identifier for a profile model.
///
/// (#1230 Packet 1 cutover) Thin delegating wrapper over
/// `darkmux_gestalt::namespaced_identifier` — the `&ProfileModel` form this
/// crate's callers use, feeding gestalt's two-explicit-parameter form (a
/// bare `pub use` can't bridge the signature). ONE definition backs both
/// this wrapper and the review's `LmsCycler`.
///
/// If the profile sets an explicit `identifier`, it passes through as-is
/// (the documented namespace opt-out). Otherwise the model id is wrapped
/// under the `darkmux:` namespace so unload-filtering can distinguish
/// darkmux's loads from user-managed ones.
pub fn namespaced_identifier(m: &ProfileModel) -> String {
    darkmux_gestalt::namespaced_identifier(&m.id, m.identifier.as_deref())
}

/// `true` if this identifier was minted by darkmux (begins with our
/// namespace). Used to filter `lms ps` results into darkmux-managed vs
/// user state (`machine status`/`machine eject`, dispatch preflight).
///
/// (#1230 Packet 1 cutover) Delegates to `darkmux_gestalt::is_darkmux_owned`
/// — see `namespaced_identifier`'s doc above.
pub fn is_darkmux_owned(identifier: &str) -> bool {
    darkmux_gestalt::is_darkmux_owned(identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_identifier_uses_prefix_when_no_override() {
        let m = ProfileModel {
            endpoint: None,
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: Some(100_000),
            capabilities: Default::default(),
            identifier: None,
        };
        assert_eq!(namespaced_identifier(&m), "darkmux:qwen3.6-35b-a3b");
    }

    #[test]
    fn namespaced_identifier_passes_through_explicit_id() {
        let m = ProfileModel {
            endpoint: None,
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: Some(100_000),
            capabilities: Default::default(),
            identifier: Some("my-custom-alias".into()),
        };
        // Explicit override wins — operator opted out of the auto-namespace.
        assert_eq!(namespaced_identifier(&m), "my-custom-alias");
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
}
