//! (#1426 ship-2) The resourcing resolver — the single planning step that
//! staffs a review's seats from the machine's roster (the active profile's
//! models). It absorbs what `select_model` scoring and the retired `crews` map
//! did as two separate mechanisms into ONE implementation, the same
//! one-implementation move the dispatch core (#1414) made for dispatch.
//!
//! A crew is now a DERIVED VIEW of a mission's resourcing, never a declared
//! entity: nobody keeps a registry of pre-formed crews awaiting missions.
//! There is a corps (the profile roster), there is planning, and crew
//! assignment is an OUTPUT. Concretely, the resolver:
//!
//! - SCORES a model per seat against the roster via [`crate::select::select_model`]
//!   (system proposes — capability scoring against the active profile's models);
//! - honors launch-param seat PINS (operator overrides — an explicit model id);
//! - and hands the review driver the SAME [`ResolvedCrew`] it always consumed,
//!   so the envelope's `staffing` snapshot still records what resolved (record
//!   shows truth).
//!
//! Operator sovereignty (#44) is intact end to end: every seat's default is
//! overridable, and the resolved truth is recorded in the run's envelope.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_profiles::profiles::get_profile;
use darkmux_types::{BundleSelector, ProfileModel, ProfileRegistry};
use std::collections::BTreeMap;

/// Canonical review seat role ids — each seat family the review graph staffs
/// maps to one role, scored against the roster.
pub const REVIEW_PROBE_ROLE: &str = "review-probe";
pub const REVIEW_JUDGE_ROLE: &str = "review-judge";
pub const REVIEW_VERIFY_ROLE: &str = "review-verify";

/// Probe-seat draw breadth default (matches the retired `SeatStaffing`
/// default `k`, so an unpinned probe behaves as `review-deep`'s did).
const DEFAULT_PROBE_K: u32 = 3;
/// Judge-seat consensus depth default (double-confirm — matches the retired
/// `SeatStaffing` default `passes`).
const DEFAULT_JUDGE_PASSES: u32 = 2;

/// A seat staffing resolved to a concrete model — the resolver's per-seat
/// output. (Migrated verbatim from the retired `darkmux_profiles::crews`; the
/// review driver consumes it unchanged.)
#[derive(Debug, Clone)]
pub struct ResolvedSeatStaffing {
    /// The roster [`Profile`](darkmux_types::Profile) name this staffing
    /// dispatches through.
    pub name: String,
    pub pm: ProfileModel,
    /// Probe-seat draw BREADTH (a union over draws — recall). Ignored by the
    /// judge/verify seats.
    pub k: u32,
    /// Judge-seat consensus DEPTH (agreement across independent judgments —
    /// precision). Ignored by the probe/verify seats.
    pub passes: u32,
    pub max_tokens: Option<u32>,
    pub selector: Option<BundleSelector>,
}

/// A fully-resolved review crew: every seat bound to a concrete model, keyed
/// by seat role id. The review driver + envelope snapshot consume this
/// UNCHANGED — only its PRODUCER changed (scored from the roster, not read from
/// a `crews` map).
#[derive(Debug, Clone)]
pub struct ResolvedCrew {
    /// The derived crew's addressable identity — the roster profile it was
    /// resourced from (there is no declared crew name any more, #1426 ship-2).
    pub name: String,
    pub seats: BTreeMap<String, Vec<ResolvedSeatStaffing>>,
    /// Whether confirmed findings render as a blocking `REQUEST_CHANGES` review
    /// (`true`) or a non-blocking `COMMENT` review (`false`, the default).
    pub request_changes: bool,
}

/// Launch-param inputs to the review resourcing resolver. Every field is an
/// override on top of the scored default: an empty field takes the scored
/// pick, a set field pins that seat to an explicit model id (which must exist
/// in the active profile). Shaped to fit the existing `--param k=v` launch
/// convention (`profile=`, `probe_models=id,id`, `judge_model=id`,
/// `verify_model=id`, `k=`).
#[derive(Debug, Default, Clone)]
pub struct ReviewResourcing {
    /// Roster profile name; `None` => the registry's `default_profile`.
    pub profile: Option<String>,
    /// Explicit probe seat model pins (one staffing each). Empty => one scored
    /// staffing (the `select_model` pick for the `review-probe` role).
    pub probe_models: Vec<String>,
    /// Explicit judge seat model pin. `None` => scored.
    pub judge_model: Option<String>,
    /// Explicit verify seat model pin. `None` => scored.
    pub verify_model: Option<String>,
    /// Probe draws-per-seat override (`k`). `None` => [`DEFAULT_PROBE_K`].
    pub k: Option<u32>,
    /// Whether confirmed findings render as a blocking `REQUEST_CHANGES`
    /// review. Defaults to `false` (advisory `COMMENT`).
    pub request_changes: bool,
}

/// Resolve the review's seat staffing from the roster + launch-param overrides.
///
/// Each of the three seat families (probe, judge, verify) gets a DEFAULT model
/// scored via [`crate::select::select_model`] against the active profile, or an
/// explicit launch-param pin. A LOCAL seat's model must declare `n_ctx` (it is
/// loaded at that context), so a missing window is a named resolution error
/// here; a REMOTE (endpoint-bearing) model needs none (nothing is loaded
/// locally). Every failure names the seat and the specific problem.
pub fn resolve_review_resourcing(
    reg: &ProfileRegistry,
    ov: &ReviewResourcing,
) -> Result<ResolvedCrew> {
    // 1. Roster profile: the named one, else the registry default.
    let profile_name = match ov.profile.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => p.to_string(),
        None => reg
            .default_profile
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "darkmux: review resourcing needs a roster profile — none named \
                     (--param profile=<name>) and no `default_profile` set in the registry. \
                     Set one, or pin per-seat models (--param judge_model=<id>, ...). (#1426 ship-2)"
                )
            })?,
    };
    // A quarantined profile surfaces its own parse error, never a "not found".
    if let Some(msg) = reg.quarantine_error_for(&profile_name) {
        bail!(msg);
    }
    let profile = get_profile(reg, &profile_name)
        .with_context(|| format!("resolving the review roster profile \"{profile_name}\""))?;

    // 2. Roles + skills so `select_model` can score capabilities per seat.
    //    Missing role manifests / skills degrade to the profile default model
    //    (behavior-preserving until operators populate capability vectors).
    let roles = crate::loader::load_roles().unwrap_or_default();
    let skills = crate::loader::load_skills().unwrap_or_default();
    let skill_index: std::collections::HashMap<String, crate::types::Skill> =
        skills.into_iter().map(|s| (s.id.clone(), s)).collect();

    // Bind an explicit model id (a pin) to a concrete `ProfileModel`, with the
    // same local-seat `n_ctx` requirement the old `resolve_crew` enforced.
    let pm_for = |model_id: &str, seat: &str| -> Result<ProfileModel> {
        let pm = profile.models.iter().find(|m| m.id == model_id).ok_or_else(|| {
            anyhow!(
                "darkmux: review seat \"{seat}\" pins model \"{model_id}\", which is not in \
                 profile \"{profile_name}\" (models: {}). (#1426 ship-2)",
                profile
                    .models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if !pm.is_remote() {
            pm.require_n_ctx()
                .map_err(|e| anyhow!("darkmux: review seat \"{seat}\" model \"{model_id}\": {e}"))?;
        }
        Ok(pm.clone())
    };
    // The scored default for a seat's role.
    let scored_pm = |role_id: &str, seat: &str| -> Result<ProfileModel> {
        let id = match roles.iter().find(|r| r.id == role_id) {
            Some(role) => crate::select::select_model(role, profile, |i| skill_index.get(i))
                .with_context(|| {
                    format!("scoring a model for review seat \"{seat}\" (role \"{role_id}\")")
                })?,
            None => profile.default_model_id().map(str::to_string).ok_or_else(|| {
                anyhow!(
                    "darkmux: review seat \"{seat}\": role \"{role_id}\" has no manifest and \
                     profile \"{profile_name}\" has no default model to fall back to. (#1426 ship-2)"
                )
            })?,
        };
        pm_for(&id, seat)
    };

    let probe_k = ov.k.unwrap_or(DEFAULT_PROBE_K).max(1);

    let mut seats: BTreeMap<String, Vec<ResolvedSeatStaffing>> = BTreeMap::new();

    // 3. Probe seat: explicit pins (one staffing each) or one scored staffing.
    let probes: Vec<ResolvedSeatStaffing> = if ov.probe_models.is_empty() {
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            pm: scored_pm(REVIEW_PROBE_ROLE, "review-probe")?,
            k: probe_k,
            passes: DEFAULT_JUDGE_PASSES,
            max_tokens: None,
            selector: None,
        }]
    } else {
        ov.probe_models
            .iter()
            .map(|id| {
                Ok(ResolvedSeatStaffing {
                    name: profile_name.clone(),
                    pm: pm_for(id, "review-probe")?,
                    k: probe_k,
                    passes: DEFAULT_JUDGE_PASSES,
                    max_tokens: None,
                    selector: None,
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    seats.insert(REVIEW_PROBE_ROLE.to_string(), probes);

    // 4. Judge seat (exactly one). Draws once; carries the consensus depth.
    let judge_pm = match ov.judge_model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => pm_for(id, "review-judge")?,
        None => scored_pm(REVIEW_JUDGE_ROLE, "review-judge")?,
    };
    seats.insert(
        REVIEW_JUDGE_ROLE.to_string(),
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            pm: judge_pm,
            k: 1,
            passes: DEFAULT_JUDGE_PASSES,
            max_tokens: None,
            selector: None,
        }],
    );

    // 5. Verify seat (exactly one). Adjudicates each double-confirmed finding
    //    once; a single-pass seat, scored by default or pinned.
    let verify_pm = match ov.verify_model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => pm_for(id, "review-verify")?,
        None => scored_pm(REVIEW_VERIFY_ROLE, "review-verify")?,
    };
    seats.insert(
        REVIEW_VERIFY_ROLE.to_string(),
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            pm: verify_pm,
            k: 1,
            passes: 1,
            max_tokens: None,
            selector: None,
        }],
    );

    Ok(ResolvedCrew {
        name: profile_name,
        seats,
        request_changes: ov.request_changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::{ModelEndpoint, Profile, ProfileModel};

    fn model(id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: Some(n_ctx), ..Default::default() }
    }
    fn remote(id: &str) -> ProfileModel {
        ProfileModel {
            id: id.to_string(),
            endpoint: Some(ModelEndpoint {
                url: Some("https://example.azure.com/openai".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn reg_with(profile_name: &str, models: Vec<ProfileModel>) -> ProfileRegistry {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            profile_name.to_string(),
            Profile { models, ..Default::default() },
        );
        ProfileRegistry {
            profiles,
            default_profile: Some(profile_name.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn scores_all_three_seats_from_the_default_profile() {
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let crew = resolve_review_resourcing(&reg, &ReviewResourcing::default()).unwrap();
        assert_eq!(crew.name, "deep", "the derived crew's identity is its roster profile");
        // All three seat families staffed by default (scored).
        assert_eq!(crew.seats.get("review-probe").unwrap().len(), 1);
        assert_eq!(crew.seats.get("review-probe").unwrap()[0].pm.id, "a");
        assert_eq!(crew.seats.get("review-probe").unwrap()[0].k, 3);
        assert_eq!(crew.seats.get("review-judge").unwrap().len(), 1);
        assert_eq!(crew.seats.get("review-judge").unwrap()[0].passes, 2);
        assert_eq!(crew.seats.get("review-verify").unwrap().len(), 1);
        assert!(!crew.request_changes);
    }

    #[test]
    fn k_override_applies_to_probe_only() {
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { k: Some(5), ..Default::default() };
        let crew = resolve_review_resourcing(&reg, &ov).unwrap();
        assert_eq!(crew.seats.get("review-probe").unwrap()[0].k, 5);
        assert_eq!(crew.seats.get("review-judge").unwrap()[0].k, 1);
    }

    #[test]
    fn explicit_pins_win_and_multiple_probe_drawers_staff() {
        let reg = reg_with(
            "deep",
            vec![model("small", 32000), model("big", 200000), remote("cloud")],
        );
        let ov = ReviewResourcing {
            probe_models: vec!["small".into(), "big".into()],
            judge_model: Some("big".into()),
            verify_model: Some("cloud".into()),
            ..Default::default()
        };
        let crew = resolve_review_resourcing(&reg, &ov).unwrap();
        let probes = crew.seats.get("review-probe").unwrap();
        assert_eq!(probes.len(), 2, "multiple probe drawers from explicit pins");
        assert_eq!(probes[0].pm.id, "small");
        assert_eq!(probes[1].pm.id, "big");
        assert_eq!(crew.seats.get("review-judge").unwrap()[0].pm.id, "big");
        let verify = &crew.seats.get("review-verify").unwrap()[0];
        assert_eq!(verify.pm.id, "cloud");
        assert!(verify.pm.is_remote(), "a remote pin needs no n_ctx");
    }

    #[test]
    fn pinning_a_missing_model_names_the_seat_and_the_model() {
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { judge_model: Some("ghost".into()), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("review-judge"), "names the seat: {err}");
        assert!(err.contains("ghost"), "names the model: {err}");
    }

    #[test]
    fn a_local_pin_without_n_ctx_fails_at_resolution() {
        let reg = reg_with(
            "deep",
            vec![ProfileModel { id: "local-a".into(), ..Default::default() }],
        );
        let ov = ReviewResourcing { probe_models: vec!["local-a".into()], ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("review-probe"), "names the seat: {err}");
        assert!(err.contains("n_ctx"), "names the field: {err}");
    }

    #[test]
    fn no_roster_profile_is_a_named_error() {
        let reg = ProfileRegistry { profiles: BTreeMap::new(), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ReviewResourcing::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("roster profile"), "got: {err}");
    }
}
