//! Model-selection for dispatches (#450 / #590).
//!
//! Capability-scored selection over the profile's `models[]`: matches the
//! role's requested capability vector ([`crate::types::Role::capabilities`])
//! against each model's offered vector. Falls back to the profile's default
//! model ([`darkmux_types::Profile::default_model_id`]) when there's no basis
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
//! ## Why no fallback to "whatever LMStudio has loaded"
//!
//! That is the dispatch-contamination anti-pattern (a user-loaded model has
//! unknown load configuration, the #1135 ghost). `select_model` returns a
//! clear error when no model is configured, and the dispatch path surfaces
//! it as a hard error.

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
/// differentiate, so selection falls back to the profile's default model
/// (`default_model_id()`: the explicit `default_model`, or the first model).
/// Real scoring only activates once an operator characterizes models with
/// `capabilities` from lab results.
///
/// **Scoring** is a weighted dot product of the role's requested vector
/// against each model's offered vector; a model with no declared vector scores
/// as a 0.5-everywhere generalist (#450) — neutral, not penalized. Highest
/// score wins; a flat tie breaks toward the default model
/// (`default_model_id()`), then first-declared.
///
/// `skill_lookup` resolves a skill id → [`Skill`] so the role's requested
/// vector composes via [`crate::types::Role::capabilities`]. A lookup that
/// returns `None` (skills unavailable) yields an empty request → the
/// default-model fallback, which is safe.
///
/// **Precedence note:** operator-pin precedence sits ABOVE this in the
/// dispatch path (a later slice of #590).
///
/// **`utility_model`** (#2914) is the machine's utility model
/// (`internal.utility`), which is NEVER a task's model: it is set aside
/// before anything else happens, so a profile that still lists it (a
/// pre-4.0 leftover `darkmux doctor` flags) can neither default to it nor
/// score it. Compared on the bare model key, so a namespaced binding
/// matches a bare profile entry and vice versa. `None` means nothing is
/// set aside — the lab's benchmark opt-in (`DispatchOpts::
/// allow_utility_model`), which is how a candidate utility model gets
/// measured through a profile before it is registered.
///
/// **Errors** with an operator-actionable message when the profile has no
/// work models: none at all, or only the utility model. The caller decides
/// whether to bail or fall back (`dispatch_internal::dispatch` probes for
/// back-compat with a loud deprecation warning).
pub(crate) fn select_model<'a, F>(
    role: &Role,
    profile: &Profile,
    skill_lookup: F,
    utility_model: Option<&str>,
) -> Result<String>
where
    F: Fn(&str) -> Option<&'a Skill>,
{
    let request = role.capabilities(skill_lookup);
    let candidates = work_models(profile, utility_model);
    if candidates.is_empty() {
        return Err(match utility_model {
            Some(util) if !profile.models.is_empty() => utility_only_error(util),
            _ => no_default_error(),
        });
    }
    let any_offers = candidates.iter().any(|m| !m.capabilities.is_empty());

    // The deterministic default among the WORK models: the declared
    // `default_model` when it is one of them, else the first work model
    // (a declared default that names the utility model is set aside with
    // it).
    let default_id = profile
        .default_model_id()
        .filter(|d| candidates.iter().any(|m| m.id == *d))
        .or_else(|| candidates.first().map(|m| m.id.as_str()));

    // Nothing to differentiate on (no requested capabilities, or no model
    // offers a vector) → the deterministic default model. This is the path
    // every shipped profile takes until operators populate `capabilities`.
    if request.is_empty() || !any_offers {
        return default_id.map(String::from).ok_or_else(no_default_error);
    }

    // Capability scoring: highest weighted-dot-product wins; a flat tie breaks
    // toward the default model, then first-declared.
    let mut best: Option<(&ProfileModel, f32)> = None;
    for m in candidates {
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
    // `candidates` is non-empty (checked above), so `best` is always `Some`.
    best.map(|(m, _)| m.id.clone()).ok_or_else(no_default_error)
}

/// (#2914) A profile's WORK models: its `models[]` with the machine's
/// utility model set aside (matched on the bare model key, either spelling).
/// The one predicate every selection path and the mission launcher's
/// staffing refusal share, so "which models may a task run on" has one
/// answer. `None` sets nothing aside.
pub fn work_models<'a>(profile: &'a Profile, utility_model: Option<&str>) -> Vec<&'a ProfileModel> {
    profile
        .models
        .iter()
        .filter(|m| !is_local_utility_model(m, utility_model))
        .collect()
}

/// (#2914 review, C2) Whether a profile model IS the machine's local
/// utility instance: a LOCAL model (no endpoint) whose bare id matches the
/// binding. A hosted model that happens to share the id is served
/// elsewhere and is never the utility instance, so it stays a work model.
pub fn is_local_utility_model(model: &ProfileModel, utility_model: Option<&str>) -> bool {
    model.is_managed() && utility_model.is_some_and(|u| names_utility_model(&model.id, u))
}

/// (#2914) Whether `candidate` names the machine's utility model, in either
/// spelling: the bare model key or the `darkmux:`-namespaced identifier on
/// either side (`internal.utility` accepts both, #1615, and a step's
/// `config.model` carries the namespaced wire id). The ONE comparison every
/// utility-model check uses.
pub fn names_utility_model(candidate: &str, utility_model: &str) -> bool {
    let bare = darkmux_gestalt::bare_model_key;
    bare(candidate) == bare(utility_model)
}

/// (#2914) The error for a profile whose only model is the utility model.
/// Public so the mission launcher refuses a task staffed on such a profile
/// with the same words the dispatch would have used.
pub fn utility_only_error(utility_model: &str) -> anyhow::Error {
    anyhow!(
        "the profile lists only `{utility_model}`, the machine's utility model \
         (`internal.utility`), and a task never runs on the utility model. Add a work \
         model to the profile's `models[]` (and drop `{utility_model}` from it: its window \
         is declared in `internal.utility`). (#2914)"
    )
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

fn no_default_error() -> anyhow::Error {
    anyhow!(
        "active profile has no models configured. Add at least one model to the \
         profile's `models[]` (and optionally set `default_model` to pick the \
         default model). (#590)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EscalationContract, Role, Skill, ToolPalette};
    use darkmux_types::{Capability, Profile, ProfileModel};

    fn make_role(id: &str, skill_ids: &[&str]) -> Role {
        Role {
            output_schema: None,
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
                endpoint: None,
                extras: Default::default(),
                id: model_id.into(),
                n_ctx: Some(100_000),
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
            endpoint: None,
            extras: Default::default(),
            id: id.into(),
            n_ctx: Some(100_000),
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

        let id = select_model(&role, &profile, no_skills, None).unwrap();
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

        assert_eq!(select_model(&coder, &profile, no_skills, None).unwrap(), "darkmux:test-model");
        assert_eq!(select_model(&reviewer, &profile, no_skills, None).unwrap(), "darkmux:test-model");
        assert_eq!(select_model(&analyst, &profile, no_skills, None).unwrap(), "darkmux:test-model");
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
        let result = select_model(&make_role("coder", &[]), &profile, no_skills, None);
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
        let result = select_model(&make_role("coder", &[]), &Profile::default(), no_skills, None);
        assert!(result.is_err());
    }

    /// Phase-2 scoring: once models carry capability vectors, the best-fit
    /// model wins on score — even over the default model.
    #[test]
    fn select_scores_best_capability_fit_over_default() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9), (Capability::Reasoning, 0.3)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let role = make_role("coder", &["coding"]);
        let profile = Profile {
            models: vec![
                // Default model (first), weak on code: 0.9*0.2 + 0.3*0.9 = 0.45
                model_with("reasoner",
                    &[(Capability::Code, 0.2), (Capability::Reasoning, 0.9)]),
                // Strong on code: 0.9*0.9 + 0.3*0.4 = 0.93
                model_with("codestar",
                    &[(Capability::Code, 0.9), (Capability::Reasoning, 0.4)]),
            ],
            ..Default::default()
        };
        assert_eq!(select_model(&role, &profile, lookup, None).unwrap(), "codestar");
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
        assert_eq!(select_model(&role, &profile, lookup, None).unwrap(), "generalist");
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
        assert_eq!(select_model(&role, &profile, lookup, None).unwrap(), "the-default");
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
        assert_eq!(select_model(&role, &profile, lookup, None).unwrap(), "first");
    }

    // ─── #2914: the machine utility model is never a task's model ──────

    /// (#2914) The machine's utility model is excluded from selection even
    /// when a profile still lists it (a pre-4.0 leftover, which doctor
    /// flags) and even when it is the profile's declared default.
    #[test]
    fn select_excludes_the_machine_utility_model_even_as_the_declared_default() {
        let profile = Profile {
            default_model: Some("util-4b".into()),
            models: vec![model_with("util-4b", &[]), model_with("worker-35b", &[])],
            ..Default::default()
        };
        // Namespaced binding, bare profile entry: matched on the bare key.
        let id = select_model(&make_role("coder", &[]), &profile, no_skills, Some("darkmux:util-4b")).unwrap();
        assert_eq!(id, "worker-35b", "the utility model is not a work model");
    }

    /// (#2914) Scoring never picks it either: a utility model with a perfect
    /// capability fit still loses to the only work model.
    #[test]
    fn select_scoring_never_picks_the_utility_model() {
        let coding = skill_with("coding", &[(Capability::Code, 1.0)]);
        let lookup = |id: &str| (id == "coding").then_some(&coding);
        let profile = Profile {
            models: vec![
                model_with("util-4b", &[(Capability::Code, 1.0)]),
                model_with("worker-35b", &[(Capability::Code, 0.1)]),
            ],
            ..Default::default()
        };
        let id = select_model(&make_role("coder", &["coding"]), &profile, lookup, Some("util-4b")).unwrap();
        assert_eq!(id, "worker-35b");
    }

    /// (#2914) A profile that lists ONLY the utility model has no work model:
    /// a loud error naming the model, the profile's fix, and the binding.
    #[test]
    fn select_errors_when_a_profile_lists_only_the_utility_model() {
        let profile = profile_with_primary("util-4b");
        let err = select_model(&make_role("coder", &[]), &profile, no_skills, Some("util-4b")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("util-4b"), "names the model: {msg}");
        assert!(msg.contains("utility model"), "says why: {msg}");
        assert!(msg.contains("internal.utility"), "names the fix: {msg}");
    }

    /// (#2914) The lab's benchmark opt-in: with no exclusion supplied, a
    /// candidate utility model IS selectable through a profile that lists
    /// it. This is how a candidate gets measured before it is registered.
    #[test]
    fn select_allows_the_utility_model_when_no_exclusion_is_supplied() {
        let profile = profile_with_primary("util-4b");
        assert_eq!(select_model(&make_role("coder", &[]), &profile, no_skills, None).unwrap(), "util-4b");
    }

    /// (#2914 review, C2) A HOSTED profile model whose bare id happens to
    /// match the utility id is never the local utility instance: it stays a
    /// work model, is selectable, and is not set aside.
    #[test]
    fn a_hosted_model_sharing_the_utility_id_is_still_a_work_model() {
        let hosted = ProfileModel {
            endpoint: Some(darkmux_types::ModelEndpoint { url: Some("https://provider.example/v1".into()), ..Default::default() }),
            ..model_with("util-4b", &[])
        };
        let profile = Profile { models: vec![hosted], ..Default::default() };
        assert_eq!(work_models(&profile, Some("util-4b")).len(), 1, "a hosted model is never the local utility instance");
        assert_eq!(select_model(&make_role("coder", &[]), &profile, no_skills, Some("util-4b")).unwrap(), "util-4b");
    }

    /// (#2914) The pure predicate the mission launcher refuses on: a profile
    /// with no work model once the utility model is set aside.
    #[test]
    fn work_models_sets_the_utility_model_aside() {
        let profile = Profile {
            models: vec![model_with("darkmux:util-4b", &[]), model_with("worker-35b", &[])],
            ..Default::default()
        };
        let ids: Vec<&str> = work_models(&profile, Some("util-4b")).iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["worker-35b"]);
        assert!(work_models(&profile_with_primary("util-4b"), Some("util-4b")).is_empty());
        assert_eq!(work_models(&profile, None).len(), 2, "no binding, nothing to set aside");
    }
}
