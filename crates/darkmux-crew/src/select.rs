//! Model-selection for crew dispatches (#450 / #590).
//!
//! Capability-scored selection over the profile's `models[]`: matches the
//! role's requested capability vector ([`crate::types::Role::capabilities`])
//! against each model's offered vector. Falls back to the profile's default
//! worker ([`darkmux_types::Profile::default_model_id`]) when there's no basis
//! to differentiate (no requested capabilities, or no model carries an offer
//! vector) — the reality until operators populate `capabilities`.
//!
//! ## Why a function (not a method on Profile)
//!
//! `select_model` consumes BOTH a role's capability needs AND the machine's
//! bound models. Living as a free function keeps the signature symmetric
//! (role × profile → model id) and gives the scoring engine a natural home
//! that doesn't pile onto either struct's impl block.
//!
//! ## Why no fallback to `probe_loaded_model` here
//!
//! Falling back to "whatever LMStudio happens to have loaded" is the
//! anti-pattern documented in `feedback_model_unload_load_authority`
//! (memory note from 2026-05-26 dispatch-contamination incident).
//! `select_model` returns a clear error when no model is configured; the
//! dispatch path decides whether back-compat fallback is acceptable, with a
//! loud deprecation warning so the misconfiguration is operator-visible.

use crate::types::{Role, Skill};
use anyhow::{anyhow, Result};
use darkmux_types::{CapabilityProfile, Profile, ProfileModel};

/// Pick which model the dispatch should target for `role` given the active
/// `profile`'s model bindings — by matching the role's requested capabilities
/// against each model's offered capability vector (#450 phase 2).
///
/// **Behavior-preserving until vectors are populated.** When the role requests
/// no capabilities OR no model in the profile carries an offer vector — the
/// reality for every shipped profile today — there's no basis to
/// differentiate, so selection falls back to the profile's default worker
/// (`default_model_id()`: the explicit `default_model`, or the first model).
/// Real scoring only activates once an operator characterizes models with
/// `capabilities` from lab results.
///
/// **Scoring** is a weighted dot product of the role's requested vector
/// against each model's offered vector; a model with no declared vector scores
/// as a 0.5-everywhere generalist (#450) — neutral, not penalized. Highest
/// score wins; a flat tie breaks toward the default worker model
/// (`default_model_id()`), then first-declared.
///
/// `skill_lookup` resolves a skill id → [`Skill`] so the role's requested
/// vector composes via [`crate::types::Role::capabilities`]. A lookup that
/// returns `None` (skills unavailable) yields an empty request → the
/// default-worker fallback, which is safe.
///
/// **Precedence note:** operator-pin precedence sits ABOVE this in the
/// dispatch path (a later slice of #590).
///
/// **Errors** with an operator-actionable message when the profile has no
/// models at all. The caller decides whether to bail or fall back
/// (`dispatch_internal::dispatch` probes for back-compat with a loud
/// deprecation warning).
pub(crate) fn select_model<'a, F>(role: &Role, profile: &Profile, skill_lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<&'a Skill>,
{
    let request = role.capabilities(skill_lookup);
    // (#590) The profile's `models[]` are worker-only — the compactor moved to
    // the registry's `internal.utility` binding, so there's no util model to
    // exclude from the candidate set.
    let any_offers = profile.models.iter().any(|m| !m.capabilities.is_empty());

    // Nothing to differentiate on (no requested capabilities, or no model
    // offers a vector) → the deterministic default worker. This is the path
    // every shipped profile takes until operators populate `capabilities`.
    if request.is_empty() || !any_offers {
        return default_or_error(profile);
    }

    // Capability scoring: highest weighted-dot-product wins; a flat tie breaks
    // toward the default worker model, then first-declared.
    let default_id = profile.default_model_id();
    let mut best: Option<(&ProfileModel, f32)> = None;
    for m in &profile.models {
        let s = score(&request, m);
        let take = match best {
            None => true,
            Some((bm, bs)) => {
                s > bs
                    || (s == bs
                        && Some(m.id.as_str()) == default_id
                        && Some(bm.id.as_str()) != default_id)
            }
        };
        if take {
            best = Some((m, s));
        }
    }
    // `any_offers` ⇒ models is non-empty, so `best` is always `Some` here.
    best.map(|(m, _)| m.id.clone()).ok_or_else(no_default_error)
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

fn default_or_error(profile: &Profile) -> Result<String> {
    profile
        .default_model_id()
        .map(String::from)
        .ok_or_else(no_default_error)
}

fn no_default_error() -> anyhow::Error {
    anyhow!(
        "active profile has no models configured. Add at least one model to the \
         profile's `models[]` (and optionally set `default_model` to pick the \
         default worker). (#590)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EscalationContract, Role, Skill, ToolPalette};
    use darkmux_types::{Capability, Profile, ProfileModel};

    fn make_role(id: &str, skill_ids: &[&str]) -> Role {
        Role {
            id: id.into(),
            description: format!("test role {id}"),
            skills: skill_ids.iter().map(|s| s.to_string()).collect(),
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
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

    fn model_with(id: &str, caps: &[(Capability, f32)]) -> ProfileModel {
        ProfileModel {
            id: id.into(),
            n_ctx: 100_000,
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

    /// (#590) A profile with no models AND no basis to score fails loudly with a
    /// config-pointer error. The dispatch path decides whether to bail or fall
    /// back; this layer refuses to invent. (Replaces the old "no Primary-role
    /// model" case — there's no role distinction anymore, so the only
    /// no-default failure mode is an empty `models[]`.)
    #[test]
    fn select_errors_when_profile_has_no_models() {
        let profile = Profile {
            models: vec![],
            ..Default::default()
        };
        let result = select_model(&make_role("coder", &[]), &profile, no_skills);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("no models configured"),
            "error must name the missing models"
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
    /// model wins on score — even over the default-worker model.
    #[test]
    fn select_scores_best_capability_fit_over_default() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9), (Capability::Reasoning, 0.3)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                // Default worker (first), weak on code: 0.9*0.2 + 0.3*0.9 = 0.45
                model_with("reasoner",
                    &[(Capability::Code, 0.2), (Capability::Reasoning, 0.9)]),
                // Strong on code: 0.9*0.9 + 0.3*0.4 = 0.93
                model_with("codestar",
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
                model_with("weak-coder", &[(Capability::Code, 0.2)]), // 1.0*0.2 = 0.2
                model_with("generalist", &[]),                        // 1.0*0.5 = 0.5
            ],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "generalist");
    }

    /// Phase-2 tie-break: a non-empty request but all-empty offers resolves the
    /// flat tie to the profile's `default_model` (behavior-preserving — the
    /// designation moved from the old Primary role to the `default_model` field
    /// in #590).
    #[test]
    fn select_ties_to_default_model_when_offers_all_empty() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![model_with("first", &[]), model_with("the-default", &[])],
            // Explicit default_model, so the tie does NOT fall to first-declared.
            default_model: Some("the-default".into()),
            ..Default::default()
        };
        // No model offers a vector → fallback path → default_model_id = "the-default".
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "the-default");
    }

    /// Phase-2 tie-break, implicit default: with no `default_model` set, an
    /// all-empty-offers tie resolves to the first-declared model (mirrors the
    /// old Primary-is-first convention).
    #[test]
    fn select_ties_to_first_model_when_no_default_set() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![model_with("first", &[]), model_with("second", &[])],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup).unwrap(), "first");
    }
}
