//! Model-selection for crew dispatches (E14 refactor 1b, #450).
//!
//! Phase 1 stub: trivially returns the active profile's Primary-role
//! model. Phase 2+ extends this into capability-scored selection over
//! all `models[]` candidates, with the role's
//! [`crate::crew::types::Role::capabilities`] vector as the input.
//!
//! ## Why a function (not a method on Profile)
//!
//! `select_model` consumes BOTH a role's capability needs AND the
//! machine's bound models. Today the role-side input is unused
//! (phase-1 trivial-return); phase 2 wires it. Living as a free
//! function keeps the signature symmetric (role × profile → model id)
//! and gives the scoring engine a natural home that doesn't pile onto
//! either struct's impl block.
//!
//! ## Why no fallback to `probe_loaded_model` here
//!
//! Falling back to "whatever LMStudio happens to have loaded" is the
//! anti-pattern documented in `feedback_model_unload_load_authority`
//! (memory note from 2026-05-26 dispatch-contamination incident).
//! `select_model` returns a clear error when configuration is missing;
//! the dispatch path decides whether back-compat fallback is
//! acceptable, with a loud deprecation warning so the misconfiguration
//! is operator-visible.
//!
//! ## Phase-1 vs Phase-2 boundary
//!
//! The selection FUNCTION exists as the architectural placeholder; the
//! BODY trivially returns Primary in phase 1. Phase 2 fills in the
//! scoring logic without changing the call signature or the call
//! sites. Operators reading the dispatch flow see "selection happens
//! via select_model" from day one, not "phase 1 hack to refactor
//! away later."

use crate::crew::types::Role;
use crate::types::Profile;
use anyhow::{anyhow, Result};

/// Pick which model the dispatch should target for `role` given the
/// active `profile`'s model bindings.
///
/// **Phase 1 stub**: returns the profile's Primary-role model id. The
/// `role` argument is captured for the signature symmetry but not yet
/// consumed — phase 2's scoring layer reads `role.capabilities(...)`
/// against each candidate model's capability vector and picks best
/// match.
///
/// **Phase-2 signature note**: when scoring activates, this function
/// will likely need a third argument — the skill lookup table the
/// `role.capabilities(skill_lookup)` derivation requires. Call sites
/// will need updating then. The phase-1 → phase-2 transition is not
/// fully signature-stable; the boundary is documented here so future-
/// you isn't surprised.
///
/// **Errors** with an operator-actionable message when the active
/// profile has no Primary model configured. The caller decides
/// whether to bail or fall back (`dispatch_internal::dispatch` falls
/// back to probing for back-compat with a loud deprecation warning).
pub fn select_model(role: &Role, profile: &Profile) -> Result<String> {
    // Phase 1: role unused (stub). Phase 2 reads
    // `role.capabilities(skill_lookup)` to score candidates.
    let _ = role;
    profile
        .primary_model_id()
        .map(String::from)
        .ok_or_else(|| {
            anyhow!(
                "active profile has no Primary-role model configured. \
                 Add a model with `role: \"primary\"` to the profile's \
                 `models[]`. (#450, E14 refactor 1b)"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::types::{EscalationContract, Role, ToolPalette};
    use crate::types::{ModelRole, Profile, ProfileModel};

    fn make_role(id: &str, skill_ids: &[&str]) -> Role {
        Role {
            id: id.into(),
            description: format!("test role {id}"),
            skills: skill_ids.iter().map(|s| s.to_string()).collect(),
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
        }
    }

    fn profile_with_primary(model_id: &str) -> Profile {
        Profile {
            models: vec![ProfileModel {
                id: model_id.into(),
                n_ctx: 100_000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            ..Default::default()
        }
    }

    /// (#450) Phase-1 stub returns the profile's Primary model id.
    /// Role capabilities are accepted but not consumed; behavior is
    /// trivial-return regardless of the role's skills.
    #[test]
    fn select_returns_profile_primary_in_phase_1() {
        let profile = profile_with_primary("darkmux:qwen3.6-35b-a3b-turboquant-mlx");
        let role = make_role("coder", &["coding"]);

        let id = select_model(&role, &profile).unwrap();
        assert_eq!(id, "darkmux:qwen3.6-35b-a3b-turboquant-mlx");
    }

    /// (#450) Phase-1 stub ignores the role's capabilities — any role
    /// in the same profile selects the same model under trivial
    /// scoring. This pins the phase-1 → phase-2 boundary: when the
    /// scoring engine activates, this test will need updating to
    /// reflect actual model-per-role selection.
    #[test]
    fn select_returns_same_model_for_different_roles_in_phase_1() {
        let profile = profile_with_primary("darkmux:test-model");
        let coder = make_role("coder", &["coding"]);
        let reviewer = make_role("code-reviewer", &["code-reviewing"]);
        let analyst = make_role("analyst", &["analyzing"]);

        let coder_pick = select_model(&coder, &profile).unwrap();
        let reviewer_pick = select_model(&reviewer, &profile).unwrap();
        let analyst_pick = select_model(&analyst, &profile).unwrap();

        assert_eq!(coder_pick, "darkmux:test-model");
        assert_eq!(reviewer_pick, "darkmux:test-model");
        assert_eq!(analyst_pick, "darkmux:test-model");
    }

    /// (#450) Profile with no Primary model fails loudly with a
    /// config-pointer error. The dispatch path decides whether to
    /// bail or fall back; this layer just refuses to invent.
    #[test]
    fn select_errors_when_profile_has_no_primary() {
        // Profile with only a Compactor (no Primary).
        let profile = Profile {
            models: vec![ProfileModel {
                id: "compactor-only".into(),
                n_ctx: 32_000,
                role: ModelRole::Compactor,
                identifier: None,
            }],
            ..Default::default()
        };
        let role = make_role("coder", &[]);

        let result = select_model(&role, &profile);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no Primary"),
            "error must name the missing Primary, got: {err}"
        );
    }

    /// (#450) An empty profile (no models at all) also fails with the
    /// same missing-primary error. Pins the edge case.
    #[test]
    fn select_errors_when_profile_is_empty() {
        let profile = Profile::default();
        let role = make_role("coder", &[]);

        let result = select_model(&role, &profile);
        assert!(result.is_err());
    }
}
