//! Model-selection for crew dispatches (E14 refactor 1b, #450).
//!
//! Phase 1 stub: trivially returns the active profile's Primary-role
//! model. Phase 2+ extends this into capability-scored selection over
//! all `models[]` candidates, with the role's
//! [`crate::types::Role::capabilities`] vector as the input.
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

use crate::types::{Role, Skill};
use anyhow::{anyhow, Result};
use darkmux_types::{CapabilityProfile, ModelRole, Profile, ProfileModel};

/// Pick which model the dispatch should target for `role` given the active
/// `profile`'s model bindings — by matching the role's requested capabilities
/// against each model's offered capability vector (#450 phase 2).
///
/// **Behavior-preserving until vectors are populated.** When the role requests
/// no capabilities OR no model in the profile carries an offer vector — the
/// reality for every shipped profile today — there's no basis to
/// differentiate, so selection falls back to the profile's Primary-role model
/// (`primary_model_id()`), exactly the pre-phase-2 result. Real scoring only
/// activates once an operator characterizes models with `capabilities` from
/// lab results.
///
/// **Scoring** is a weighted dot product of the role's requested vector
/// against each model's offered vector; a model with no declared vector scores
/// as a 0.5-everywhere generalist (#450) — neutral, not penalized. Highest
/// score wins; a flat tie breaks toward the Primary-role model (so the
/// all-empty case yields exactly `primary_model_id()`), then first-declared.
///
/// `skill_lookup` resolves a skill id → [`Skill`] so the role's requested
/// vector composes via [`crate::types::Role::capabilities`]. A lookup that
/// returns `None` (skills unavailable) yields an empty request → the
/// Primary-fallback path, which is safe.
///
/// **Precedence note:** operator-pin precedence sits ABOVE this in the
/// dispatch path (a later slice of #590); a `default_model` fallback replaces
/// `primary_model_id()` once `ModelRole` is removed (#590 S-δ).
///
/// **Errors** with an operator-actionable message when the fallback path is
/// taken and the profile has no Primary-role model. The caller decides whether
/// to bail or fall back (`dispatch_internal::dispatch` probes for back-compat
/// with a loud deprecation warning).
pub(crate) fn select_model<'a, F>(role: &Role, profile: &Profile, skill_lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<&'a Skill>,
{
    let request = role.capabilities(skill_lookup);
    // Worker candidates only — the Compactor is util infrastructure (its model
    // is the registered util model, #590 / S-γ), never a worker-role match.
    // (Once `ModelRole` is removed in S-δ the profile's `models[]` are
    // worker-only and this filter becomes a no-op.)
    let candidates: Vec<&ProfileModel> = profile
        .models
        .iter()
        .filter(|m| !matches!(m.role, ModelRole::Compactor))
        .collect();
    let any_offers = candidates.iter().any(|m| !m.capabilities.is_empty());

    // Nothing to differentiate on (no requested capabilities, or no candidate
    // offers a vector) → today's deterministic pick. This is the path every
    // shipped profile takes until operators populate `capabilities`.
    if request.is_empty() || !any_offers {
        return primary_or_error(profile);
    }

    // Capability scoring: highest weighted-dot-product wins; a flat tie breaks
    // toward the Primary-role model, then first-declared.
    let mut best: Option<(&ProfileModel, f32)> = None;
    for &m in &candidates {
        let s = score(&request, m);
        let take = match best {
            None => true,
            Some((bm, bs)) => s > bs || (s == bs && is_primary(m) && !is_primary(bm)),
        };
        if take {
            best = Some((m, s));
        }
    }
    // `any_offers` ⇒ at least one candidate, so `best` is always `Some` here.
    best.map(|(m, _)| m.id.clone()).ok_or_else(no_primary_error)
}

/// Weighted dot product Σ `request[c] × offer[c]`. A model with no declared
/// capability vector is the 0.5-everywhere generalist (#450) — it offers 0.5
/// on every dimension the role requests, so it's neutral, never penalized.
fn score(request: &CapabilityProfile, model: &ProfileModel) -> f32 {
    const GENERALIST: f32 = 0.5;
    request
        .iter()
        .map(|(cap, req_w)| {
            let offer_w = if model.capabilities.is_empty() {
                GENERALIST
            } else {
                model.capabilities.get(cap).copied().unwrap_or(0.0)
            };
            // Defensive: a non-finite offer weight (operator typo / NaN)
            // contributes nothing rather than poisoning the whole sum to NaN
            // and locking out every other candidate. Mirrors the request-side
            // guard in `Role::capabilities`; load-time validation of offer
            // vectors is the thorough fix (tracked for follow-up).
            let offer_w = if offer_w.is_finite() { offer_w } else { 0.0 };
            req_w * offer_w
        })
        .sum()
}

fn is_primary(m: &ProfileModel) -> bool {
    matches!(m.role, ModelRole::Primary)
}

fn primary_or_error(profile: &Profile) -> Result<String> {
    profile
        .primary_model_id()
        .map(String::from)
        .ok_or_else(no_primary_error)
}

fn no_primary_error() -> anyhow::Error {
    anyhow!(
        "active profile has no Primary-role model configured. \
         Add a model with `role: \"primary\"` to the profile's \
         `models[]`. (#450, E14 refactor 1b)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EscalationContract, Role, Skill, ToolPalette};
    use darkmux_types::{Capability, ModelRole, Profile, ProfileModel};

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
            feedback_templates: None,
        }
    }

    fn profile_with_primary(model_id: &str) -> Profile {
        Profile {
            models: vec![ProfileModel {
                id: model_id.into(),
                n_ctx: 100_000,
                role: ModelRole::Primary,
                capabilities: Default::default(),
                identifier: None,
            }],
            ..Default::default()
        }
    }

    /// A skill lookup that finds nothing — drives the empty-request fallback
    /// path (the reality for every shipped profile, which carries no vectors).
    fn no_skills(_id: &str) -> Option<&'static Skill> {
        None
    }

    fn skill_with(id: &str, caps: &[(Capability, f32)]) -> Skill {
        Skill {
            id: id.into(),
            description: format!("test skill {id}"),
            keywords: vec![],
            capabilities: caps.iter().cloned().collect(),
        }
    }

    fn model_with(id: &str, role: ModelRole, caps: &[(Capability, f32)]) -> ProfileModel {
        ProfileModel {
            id: id.into(),
            n_ctx: 100_000,
            role,
            capabilities: caps.iter().cloned().collect(),
            identifier: None,
        }
    }

    /// Behavior-preserving: with no offer vectors (every shipped profile
    /// today), selection falls back to the Primary-role model — exactly the
    /// pre-phase-2 result, regardless of the role's skills.
    #[test]
    fn select_falls_back_to_primary_when_no_offers() {
        let profile = profile_with_primary("darkmux:qwen3.6-35b-a3b-turboquant-mlx");
        let role = make_role("coder", &["coding"]);

        let id = select_model(&role, &profile, no_skills).unwrap();
        assert_eq!(id, "darkmux:qwen3.6-35b-a3b-turboquant-mlx");
    }

    /// Behavior-preserving: with no offer vectors, every role resolves to the
    /// same Primary model (no capability differentiation possible yet).
    #[test]
    fn select_returns_primary_for_all_roles_when_no_offers() {
        let profile = profile_with_primary("darkmux:test-model");
        let coder = make_role("coder", &["coding"]);
        let reviewer = make_role("code-reviewer", &["code-reviewing"]);
        let analyst = make_role("analyst", &["analyzing"]);

        assert_eq!(select_model(&coder, &profile, no_skills).unwrap(), "darkmux:test-model");
        assert_eq!(select_model(&reviewer, &profile, no_skills).unwrap(), "darkmux:test-model");
        assert_eq!(select_model(&analyst, &profile, no_skills).unwrap(), "darkmux:test-model");
    }

    /// (#450) Profile with no Primary AND no basis to score fails loudly with a
    /// config-pointer error. The dispatch path decides whether to bail or fall
    /// back; this layer refuses to invent.
    #[test]
    fn select_errors_when_profile_has_no_primary() {
        // Only a Compactor (no Primary), no vectors → the fallback path.
        let profile = Profile {
            models: vec![model_with("compactor-only", ModelRole::Compactor, &[])],
            ..Default::default()
        };
        let result = select_model(&make_role("coder", &[]), &profile, no_skills);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("no Primary"),
            "error must name the missing Primary"
        );
    }

    /// (#450) An empty profile (no models) also fails with the missing-primary
    /// error. Pins the edge case.
    #[test]
    fn select_errors_when_profile_is_empty() {
        let result = select_model(&make_role("coder", &[]), &Profile::default(), no_skills);
        assert!(result.is_err());
    }

    /// Phase-2 scoring: once models carry capability vectors, the best-fit
    /// model wins on score — even over the Primary-role model.
    #[test]
    fn select_scores_best_capability_fit_over_primary() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9), (Capability::Reasoning, 0.3)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                // Primary, weak on code: 0.9*0.2 + 0.3*0.9 = 0.45
                model_with("reasoner", ModelRole::Primary,
                    &[(Capability::Code, 0.2), (Capability::Reasoning, 0.9)]),
                // Auxiliary, strong on code: 0.9*0.9 + 0.3*0.4 = 0.93
                model_with("codestar", ModelRole::Auxiliary,
                    &[(Capability::Code, 0.9), (Capability::Reasoning, 0.4)]),
            ],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "codestar");
    }

    /// Phase-2: an unvectored model is a 0.5-everywhere generalist — it beats a
    /// weakly-vectored model, so lacking a vector isn't a penalty.
    #[test]
    fn select_treats_unvectored_model_as_half_generalist() {
        let coding = skill_with("coding", &[(Capability::Code, 1.0)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                model_with("weak-coder", ModelRole::Primary, &[(Capability::Code, 0.2)]), // 1.0*0.2 = 0.2
                model_with("generalist", ModelRole::Auxiliary, &[]),                      // 1.0*0.5 = 0.5
            ],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "generalist");
    }

    /// Phase-2 tie-break: a non-empty request but all-empty offers resolves the
    /// flat tie to the Primary-role model (behavior-preserving).
    #[test]
    fn select_ties_to_primary_when_offers_all_empty() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                model_with("aux", ModelRole::Auxiliary, &[]),
                model_with("prim", ModelRole::Primary, &[]),
            ],
            ..Default::default()
        };
        // No model offers a vector → fallback path → primary_model_id = "prim".
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "prim");
    }

    /// Phase-2: the Compactor is excluded from worker candidates — even a
    /// Compactor with a strong vector never wins a worker-role match.
    #[test]
    fn select_excludes_the_compactor_from_worker_candidates() {
        let coding = skill_with("coding", &[(Capability::Code, 1.0)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                // Strong on code, but it's the Compactor → not a candidate.
                model_with("the-compactor", ModelRole::Compactor, &[(Capability::Code, 1.0)]),
                // Weaker, but a real worker → wins by being the only candidate.
                model_with("the-worker", ModelRole::Primary, &[(Capability::Code, 0.6)]),
            ],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "the-worker");
    }
}
