//! Shared profile↔loaded-model matching (#544).
//!
//! Whether a `lms ps`-reported [`LoadedModel`] corresponds to a profile's
//! declared [`ProfileModel`], and whether their context windows have
//! diverged, are questions asked in three places — the lab
//! profile-envelope warning (`darkmux-lab`), the coding-task dispatch
//! pre-flight (`darkmux-lab`), and `darkmux doctor`'s profile-match check
//! (`darkmux-doctor`). Before #544 each had its own copy with subtly
//! different rules (doctor couldn't even match a `darkmux:`-namespaced
//! load). These two functions are the single source of truth so the three
//! surfaces always agree about the same loaded state.

use crate::swap::namespaced_identifier;
use darkmux_types::{LoadedModel, ProfileModel};

/// Does this loaded model correspond to the profile's declared model?
///
/// Matches on the bare model key first — the canonical dispatch
/// resolution per the darkmux namespace convention (`identifier =
/// darkmux:foo`, `modelKey = foo`; dispatchers call with the bare `foo`
/// and resolve via `modelKey`). Also accepts the bare or namespaced
/// `identifier` (`namespaced_identifier` honors an explicit operator
/// override) and any explicit `identifier` the operator set in the
/// profile matched against the loaded model key.
pub fn loaded_matches(lm: &LoadedModel, pm: &ProfileModel) -> bool {
    let namespaced = namespaced_identifier(pm);
    lm.model == pm.id
        || lm.identifier == pm.id
        || lm.identifier == namespaced
        || pm
            .identifier
            .as_deref()
            .is_some_and(|id| id == lm.identifier || id == lm.model)
}

/// Declared vs loaded context windows differ enough to flag?
///
/// A 5% tolerance absorbs benign rounding (a profile declaring `262000`
/// against a `262144` power-of-two load) while still catching a real
/// envelope swap (e.g. `262000` declared vs `101000` loaded). A loaded
/// context of `0` means `lms ps` didn't report one — we can't tell, so we
/// don't flag.
pub fn ctx_diverges(declared: u64, loaded: u64) -> bool {
    if loaded == 0 {
        return false;
    }
    declared.abs_diff(loaded) * 20 > declared
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::ModelRole;

    fn pm(id: &str, ident: Option<&str>) -> ProfileModel {
        ProfileModel {
            id: id.to_string(),
            n_ctx: 262000,
            role: ModelRole::Primary,
            capabilities: Default::default(),
            identifier: ident.map(|s| s.to_string()),
        }
    }

    fn lm(identifier: &str, model: &str, context: u64) -> LoadedModel {
        LoadedModel {
            identifier: identifier.to_string(),
            model: model.to_string(),
            status: "idle".to_string(),
            size: "1.00 GB".to_string(),
            context,
        }
    }

    #[test]
    fn matches_on_bare_model_key() {
        // namespaced load: identifier=darkmux:foo, modelKey=foo
        assert!(loaded_matches(
            &lm("darkmux:qwen-35b", "qwen-35b", 262000),
            &pm("qwen-35b", None)
        ));
    }

    #[test]
    fn matches_on_namespaced_identifier() {
        assert!(loaded_matches(
            &lm("darkmux:qwen-35b", "different-key", 262000),
            &pm("qwen-35b", None)
        ));
    }

    #[test]
    fn matches_on_bare_identifier() {
        assert!(loaded_matches(
            &lm("qwen-35b", "other", 262000),
            &pm("qwen-35b", None)
        ));
    }

    #[test]
    fn matches_on_operator_identifier_override() {
        // Operator pinned an explicit identifier; the load reports it.
        assert!(loaded_matches(
            &lm("my-custom-id", "whatever", 262000),
            &pm("qwen-35b", Some("my-custom-id"))
        ));
    }

    #[test]
    fn no_match_on_unrelated_model() {
        assert!(!loaded_matches(
            &lm("darkmux:other", "other", 262000),
            &pm("qwen-35b", None)
        ));
    }

    #[test]
    fn ctx_tolerates_power_of_two_rounding() {
        assert!(!ctx_diverges(262000, 262144));
    }

    #[test]
    fn ctx_flags_real_envelope_swap() {
        assert!(ctx_diverges(262000, 101000));
    }

    #[test]
    fn ctx_unknown_load_does_not_flag() {
        assert!(!ctx_diverges(262000, 0));
    }
}
