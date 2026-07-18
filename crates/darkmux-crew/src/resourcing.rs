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

/// (#1475 packet 2) The three distinct PROBE roles the role→profile flip
/// staffs. The crew's recall diversity now falls out of three distinct
/// role→profile→model bindings, not k draws of one scored model. All three
/// SHARE the frozen probe persona (#1256): only the bound profile (and thus
/// model) differs. The seat FAMILY key in [`ResolvedCrew::seats`] stays
/// [`REVIEW_PROBE_ROLE`] (`"review-probe"`, what `validate_review_crew`
/// keys on); each of the three staffings under it carries its own distinct
/// `role_id` (high/mid/low) so role→profile resolution + the envelope
/// snapshot can name which role bound which profile.
pub const REVIEW_PROBE_HIGH_ROLE: &str = "review-probe-high";
pub const REVIEW_PROBE_MID_ROLE: &str = "review-probe-mid";
pub const REVIEW_PROBE_LOW_ROLE: &str = "review-probe-low";
/// The three probe roles in graph order (high → mid → low) — the order the
/// probe stage expands them into tasks, and the order the review driver
/// stamps their per-seat config.
pub const REVIEW_PROBE_ROLES: [&str; 3] =
    [REVIEW_PROBE_HIGH_ROLE, REVIEW_PROBE_MID_ROLE, REVIEW_PROBE_LOW_ROLE];

/// Probe-seat draw breadth default (matches the retired `SeatStaffing`
/// default `k`, so an unpinned probe behaves as `review-deep`'s did).
const DEFAULT_PROBE_K: u32 = 3;
/// Judge-seat consensus depth default (double-confirm — matches the retired
/// `SeatStaffing` default `passes`).
const DEFAULT_JUDGE_PASSES: u32 = 2;

/// (#1426 ship-2 / #44) How a seat's model was chosen. The resolver stamps
/// this on every staffing so the envelope's staffing snapshot answers "where
/// did this decision come from" directly — the operator never has to wonder
/// whether a seat was scored or pinned (operator sovereignty #44: system
/// proposes, operator overrides, record shows truth AND why).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StaffingProvenance {
    /// `"scored"` (capability scoring against the roster profile) or
    /// `"pinned"` (an explicit launch-param model id). A plain string, not an
    /// enum, so snapshot consumers stay lenient to future kinds.
    pub kind: String,
    /// Scored: what it was scored against (role + roster profile). Pinned:
    /// which launch param pinned it.
    pub detail: String,
}

impl StaffingProvenance {
    fn scored(role_id: &str, roster: &str) -> Self {
        StaffingProvenance {
            kind: "scored".to_string(),
            detail: format!(
                "select_model capability scoring for role \"{role_id}\" against roster \
                 profile \"{roster}\""
            ),
        }
    }
    fn pinned(param: &str, model_id: &str) -> Self {
        StaffingProvenance {
            kind: "pinned".to_string(),
            detail: format!("pinned by launch param {param}={model_id}"),
        }
    }
    /// (#1475 packet 2) A seat staffed by the role→profile flip: the role was
    /// resolved through the machine-local `role_profiles` map (`source =
    /// "role_profiles map"`) or fell through to `default_profile` (`source =
    /// "default_profile fallback"`, the fresh-user floor). Names the whole
    /// role→profile→model chain so the envelope answers "where did this seat's
    /// model come from" directly (operator sovereignty #44).
    pub fn role_profile(role_id: &str, profile_name: &str, source: &str) -> Self {
        StaffingProvenance {
            kind: "role-profile".to_string(),
            detail: format!(
                "role \"{role_id}\" → profile \"{profile_name}\" ({source})"
            ),
        }
    }
}

/// A seat staffing resolved to a concrete model — the resolver's per-seat
/// output. (Migrated verbatim from the retired `darkmux_profiles::crews`,
/// plus the #44 `provenance` stamp; the review driver consumes it unchanged.)
#[derive(Debug, Clone)]
pub struct ResolvedSeatStaffing {
    /// The roster [`Profile`](darkmux_types::Profile) name this staffing
    /// dispatches through.
    pub name: String,
    /// (#1475 packet 2) The review ROLE this seat was staffed for, when it
    /// came from the role→profile flip — `review-probe-high`/`-mid`/`-low`,
    /// `review-judge`, or `review-verify`. `None` for the legacy
    /// roster-scoring resolver's staffings (which key seats by family, not by
    /// a distinct per-seat role) and for hand-built test staffings. The
    /// envelope snapshot records it so a run names which role bound each seat.
    pub role_id: Option<String>,
    pub pm: ProfileModel,
    /// Probe-seat draw BREADTH (a union over draws — recall). Ignored by the
    /// judge/verify seats.
    pub k: u32,
    /// Judge-seat consensus DEPTH (agreement across independent judgments —
    /// precision). Ignored by the probe/verify seats.
    pub passes: u32,
    pub max_tokens: Option<u32>,
    pub selector: Option<BundleSelector>,
    /// (#1426 ship-2 / #44) Scored-vs-pinned, stamped by the resolver; `None`
    /// only for hand-built staffings (tests, synthetic paths).
    pub provenance: Option<StaffingProvenance>,
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
    /// (#1266) Judge consensus DEPTH (`passes`) override. `None` =>
    /// [`DEFAULT_JUDGE_PASSES`] (double-confirm). Governs the JUDGE seat only
    /// (the sole consumer); the probe/verify seats ignore it. Sourced from the
    /// `passes` launch input (see `review.json`), validated `>= 1` here at
    /// resolution time (contract 7 — lenient on read, loud at resolution).
    pub passes: Option<u32>,
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

    // (#1426 ship-2 gate CONSIDER) `k=0` is a loud error, never a silent
    // clamp: a zero draw count guarantees a degenerate run (zero probe
    // flags), and clamping it to 1 would hide the misconfiguration from the
    // operator (sovereignty #44 — surface, never silently substitute).
    // review-bench's clap layer already rejects 0; this covers the
    // `mission launch review --param k=0` path.
    if ov.k == Some(0) {
        bail!(
            "darkmux: review resourcing: k must be >= 1 (got 0) — a zero probe draw count \
             guarantees a degenerate run. Omit k for the default ({DEFAULT_PROBE_K}). (#1426 ship-2)"
        );
    }
    let probe_k = ov.k.unwrap_or(DEFAULT_PROBE_K);

    // (#1266) Judge consensus depth (`passes`), sourced from config, validated
    // `>= 1` here — a zero-pass judge would run no judgment at all. Same
    // surface-never-silently-clamp posture as `k` above (sovereignty #44).
    if ov.passes == Some(0) {
        bail!(
            "darkmux: review resourcing: passes must be >= 1 (got 0) — a zero-pass judge \
             makes no ruling. Omit passes for the default ({DEFAULT_JUDGE_PASSES}, \
             double-confirm). (#1266)"
        );
    }
    let judge_passes = ov.passes.unwrap_or(DEFAULT_JUDGE_PASSES);

    let mut seats: BTreeMap<String, Vec<ResolvedSeatStaffing>> = BTreeMap::new();

    // 3. Probe seat: explicit pins (one staffing each) or one scored staffing.
    //    Every staffing carries its #44 provenance stamp (scored vs pinned).
    let probes: Vec<ResolvedSeatStaffing> = if ov.probe_models.is_empty() {
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            role_id: None,
            pm: scored_pm(REVIEW_PROBE_ROLE, "review-probe")?,
            k: probe_k,
            passes: DEFAULT_JUDGE_PASSES,
            max_tokens: None,
            selector: None,
            provenance: Some(StaffingProvenance::scored(REVIEW_PROBE_ROLE, &profile_name)),
        }]
    } else {
        ov.probe_models
            .iter()
            .map(|id| {
                Ok(ResolvedSeatStaffing {
                    name: profile_name.clone(),
                    role_id: None,
                    pm: pm_for(id, "review-probe")?,
                    k: probe_k,
                    passes: DEFAULT_JUDGE_PASSES,
                    max_tokens: None,
                    selector: None,
                    provenance: Some(StaffingProvenance::pinned("probe_models", id)),
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    seats.insert(REVIEW_PROBE_ROLE.to_string(), probes);

    // 4. Judge seat (exactly one). Draws once; carries the consensus depth.
    let (judge_pm, judge_prov) =
        match ov.judge_model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => (pm_for(id, "review-judge")?, StaffingProvenance::pinned("judge_model", id)),
            None => (
                scored_pm(REVIEW_JUDGE_ROLE, "review-judge")?,
                StaffingProvenance::scored(REVIEW_JUDGE_ROLE, &profile_name),
            ),
        };
    seats.insert(
        REVIEW_JUDGE_ROLE.to_string(),
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            role_id: None,
            pm: judge_pm,
            k: 1,
            passes: judge_passes,
            max_tokens: None,
            selector: None,
            provenance: Some(judge_prov),
        }],
    );

    // 5. Verify seat (exactly one). Adjudicates each double-confirmed finding
    //    once; a single-pass seat, scored by default or pinned.
    let (verify_pm, verify_prov) =
        match ov.verify_model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => {
                (pm_for(id, "review-verify")?, StaffingProvenance::pinned("verify_model", id))
            }
            None => (
                scored_pm(REVIEW_VERIFY_ROLE, "review-verify")?,
                StaffingProvenance::scored(REVIEW_VERIFY_ROLE, &profile_name),
            ),
        };
    seats.insert(
        REVIEW_VERIFY_ROLE.to_string(),
        vec![ResolvedSeatStaffing {
            name: profile_name.clone(),
            role_id: None,
            pm: verify_pm,
            k: 1,
            passes: 1,
            max_tokens: None,
            selector: None,
            provenance: Some(verify_prov),
        }],
    );

    Ok(ResolvedCrew {
        name: profile_name,
        seats,
        request_changes: ov.request_changes,
    })
}

// ─── (#1475 packet 2) role→profile staffing — THE FLIP ──────────────────────

/// (#1475 packet 2) Launch-time knobs for the role→profile review crew — the
/// slice of [`ReviewResourcing`] that survives the flip. The per-seat MODEL is
/// no longer chosen here (roster capability-scoring is retired for review);
/// each seat's model resolves from its role's `role_profiles` binding. Only the
/// non-model knobs remain: the judge's consensus DEPTH and the render's
/// blocking-vs-advisory choice. (The three roster/seat model pins —
/// `probe_models`/`judge_model`/`verify_model` — are gone here; the general
/// per-run role→profile override lands in a later packet.)
#[derive(Debug, Default, Clone)]
pub struct ReviewRoleStaffing {
    /// (#1266) Judge consensus DEPTH (`passes`) override. `None` =>
    /// [`DEFAULT_JUDGE_PASSES`] (double-confirm). Validated `>= 1` here (a
    /// zero-pass judge makes no ruling); governs the JUDGE seat only.
    pub passes: Option<u32>,
    /// Whether confirmed findings render as a blocking `REQUEST_CHANGES` review
    /// (`true`) or an advisory `COMMENT` (`false`, the default).
    pub request_changes: bool,
}

/// (#1475 packet 2) THE FLIP — staff the review crew from the machine-local
/// role→profile map instead of roster capability-scoring. Each of the five
/// review roles (`review-probe-high`/`-mid`/`-low`, `review-judge`,
/// `review-verify`) resolves INDEPENDENTLY through
/// [`darkmux_profiles::profiles::resolve_role_profile`]: role → `role_profiles`
/// map → profile → model. An UNMAPPED role falls through to `default_profile`
/// (the fresh-user floor — a bare machine with an empty map runs every seat on
/// one model: works, no diversity; the operator populates the map for the real
/// heterogeneous crew). A role MAPPED to a profile absent from the registry is
/// a loud resolution error (packet 1's `resolve_role_profile_with` owns that
/// message; `darkmux doctor` surfaces the same dangling binding).
///
/// Hands back the SAME [`ResolvedCrew`] shape the review driver + envelope
/// snapshot already consume — only the PRODUCER changed (role→profile, not
/// roster scoring). The three probe staffings land under the
/// [`REVIEW_PROBE_ROLE`] family key (what `validate_review_crew` reads), each
/// carrying its own distinct `role_id`; judge/verify each get their single
/// seat. Operator sovereignty (#44) is intact: the resolved truth — role →
/// profile → model → binding-source — is stamped on every seat's provenance.
pub fn resolve_review_role_crew(reg: &ProfileRegistry, ov: &ReviewRoleStaffing) -> Result<ResolvedCrew> {
    resolve_review_role_crew_with(reg, ov, &|role| darkmux_types::config_access::role_profile(role))
}

/// (#1475 packet 2) Pure core of [`resolve_review_role_crew`] — the role→profile
/// binding lookup is INJECTED (`mapped(role_id)` = the `config.json` binding, or
/// `None` = unmapped), so the whole flip is unit-testable against a temp map
/// without the process-wide `config()`. Mirrors packet 1's
/// `resolve_role_profile_with` seam exactly.
pub fn resolve_review_role_crew_with(
    reg: &ProfileRegistry,
    ov: &ReviewRoleStaffing,
    mapped: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedCrew> {
    // Same surface-never-silently-clamp posture as the roster resolver
    // (sovereignty #44): a zero-pass judge is a loud error, not a clamp.
    if ov.passes == Some(0) {
        bail!(
            "darkmux: review resourcing: passes must be >= 1 (got 0) — a zero-pass judge \
             makes no ruling. Omit passes for the default ({DEFAULT_JUDGE_PASSES}, \
             double-confirm). (#1266)"
        );
    }
    let judge_passes = ov.passes.unwrap_or(DEFAULT_JUDGE_PASSES);

    let mut seats: BTreeMap<String, Vec<ResolvedSeatStaffing>> = BTreeMap::new();

    // Probe seat family: one staffing per distinct probe role. Recall
    // diversity is the three distinct role→profile→model bindings — no k
    // draw-fanout of one model (k=1 per probe role).
    let mut probes = Vec::with_capacity(REVIEW_PROBE_ROLES.len());
    for role_id in REVIEW_PROBE_ROLES {
        probes.push(staff_review_role(reg, role_id, mapped, 1, DEFAULT_JUDGE_PASSES)?);
    }
    seats.insert(REVIEW_PROBE_ROLE.to_string(), probes);

    // Judge seat: exactly one, carrying the consensus depth.
    seats.insert(
        REVIEW_JUDGE_ROLE.to_string(),
        vec![staff_review_role(reg, REVIEW_JUDGE_ROLE, mapped, 1, judge_passes)?],
    );

    // Verify seat: exactly one; adjudicates each confirmed finding once.
    seats.insert(
        REVIEW_VERIFY_ROLE.to_string(),
        vec![staff_review_role(reg, REVIEW_VERIFY_ROLE, mapped, 1, 1)?],
    );

    Ok(ResolvedCrew { name: role_crew_name(&seats), seats, request_changes: ov.request_changes })
}

/// (#1475 packet 2) Resolve ONE review role to a concrete seat staffing via the
/// role→profile binding. A LOCAL seat's model must declare `n_ctx` (it's loaded
/// at that context) — a missing window is a named resolution error; a REMOTE
/// (endpoint-bearing) model needs none. The seat carries its `role_id` +
/// role→profile provenance so the envelope snapshot is self-describing.
fn staff_review_role(
    reg: &ProfileRegistry,
    role_id: &str,
    mapped: &dyn Fn(&str) -> Option<String>,
    k: u32,
    passes: u32,
) -> Result<ResolvedSeatStaffing> {
    let bound = mapped(role_id);
    let resolved =
        darkmux_profiles::profiles::resolve_role_profile_with(role_id, bound.as_deref(), reg)
            .with_context(|| format!("staffing review role \"{role_id}\" via its role→profile binding"))?;
    let model_id = resolved.profile.default_model_id().map(str::to_string).ok_or_else(|| {
        anyhow!(
            "darkmux: review role \"{role_id}\" resolved to profile \"{}\", which declares no \
             models — bind the role to a profile with at least one model \
             (`darkmux config set role_profiles.{role_id} <profile>`). (#1475)",
            resolved.profile_name
        )
    })?;
    let pm = resolved.profile.models.iter().find(|m| m.id == model_id).cloned().ok_or_else(|| {
        anyhow!(
            "darkmux: review role \"{role_id}\" profile \"{}\" names default model \"{model_id}\", \
             absent from its own models[]. (#1475)",
            resolved.profile_name
        )
    })?;
    if !pm.is_remote() {
        pm.require_n_ctx()
            .map_err(|e| anyhow!("darkmux: review role \"{role_id}\" model \"{model_id}\": {e}"))?;
    }
    let source = match resolved.source {
        darkmux_profiles::profiles::RoleProfileSource::Mapped => "role_profiles map",
        darkmux_profiles::profiles::RoleProfileSource::DefaultFallback => "default_profile fallback",
    };
    Ok(ResolvedSeatStaffing {
        name: resolved.profile_name.clone(),
        role_id: Some(role_id.to_string()),
        pm,
        k,
        passes,
        max_tokens: None,
        selector: None,
        provenance: Some(StaffingProvenance::role_profile(role_id, &resolved.profile_name, source)),
    })
}

/// (#1475 packet 2) The role→profile crew's display identity: the DISTINCT
/// resolved profile names across all seats, sorted + `+`-joined. A homogeneous
/// fresh-user crew (every role → default_profile) reads as that one profile
/// name; a heterogeneous crew reads as `devstral+qwen27b+qwen35b+qwen4b`. Shown
/// on bookend records + the mission board — the honest set of profiles in play,
/// never a fabricated single "roster".
fn role_crew_name(seats: &BTreeMap<String, Vec<ResolvedSeatStaffing>>) -> String {
    let mut names: Vec<&str> = seats.values().flatten().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        "review".to_string()
    } else {
        names.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::{ModelEndpoint, Profile, ProfileModel};
    use tempfile::TempDir;

    /// RAII guard pointing `DARKMUX_CREW_DIR` at a TempDir for the test's
    /// duration (gate CONSIDER on ship-2): the resolver calls the REAL
    /// `load_roles`/`load_skills`, so without isolation these tests become
    /// environment-sensitive the day the operator's user role manifests carry
    /// capability vectors. Same pattern as `loader_tests::CrewDirGuard`;
    /// every test here is `#[serial_test::serial]` (env mutation).
    struct CrewDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl CrewDirGuard {
        fn empty() -> Self {
            Self::new(TempDir::new().expect("tempdir"))
        }
        fn new(tmp: TempDir) -> Self {
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };
            Self { prev, _tmp: tmp }
        }
        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

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

    #[serial_test::serial]
    #[test]
    fn scores_all_three_seats_from_the_default_profile() {
        let _guard = CrewDirGuard::empty();
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

    #[serial_test::serial]
    #[test]
    fn k_override_applies_to_probe_only() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { k: Some(5), ..Default::default() };
        let crew = resolve_review_resourcing(&reg, &ov).unwrap();
        assert_eq!(crew.seats.get("review-probe").unwrap()[0].k, 5);
        assert_eq!(crew.seats.get("review-judge").unwrap()[0].k, 1);
    }

    #[serial_test::serial]
    #[test]
    fn explicit_pins_win_and_multiple_probe_drawers_staff() {
        let _guard = CrewDirGuard::empty();
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

    #[serial_test::serial]
    #[test]
    fn pinning_a_missing_model_names_the_seat_and_the_model() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { judge_model: Some("ghost".into()), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("review-judge"), "names the seat: {err}");
        assert!(err.contains("ghost"), "names the model: {err}");
    }

    #[serial_test::serial]
    #[test]
    fn a_local_pin_without_n_ctx_fails_at_resolution() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with(
            "deep",
            vec![ProfileModel { id: "local-a".into(), ..Default::default() }],
        );
        let ov = ReviewResourcing { probe_models: vec!["local-a".into()], ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("review-probe"), "names the seat: {err}");
        assert!(err.contains("n_ctx"), "names the field: {err}");
    }

    #[serial_test::serial]
    #[test]
    fn no_roster_profile_is_a_named_error() {
        let _guard = CrewDirGuard::empty();
        let reg = ProfileRegistry { profiles: BTreeMap::new(), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ReviewResourcing::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("roster profile"), "got: {err}");
    }

    /// (#1426 ship-2 gate CONSIDER) `k=0` is a loud error, never a silent
    /// clamp to 1 — the mission-launch path has no clap range guard, so the
    /// resolver is the floor.
    #[serial_test::serial]
    #[test]
    fn k_zero_errors_loudly_instead_of_clamping() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { k: Some(0), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("k must be >= 1"), "got: {err}");
        assert!(err.contains("degenerate"), "names the consequence: {err}");
    }

    /// (#1266) `passes` sources the judge's consensus depth from config, and
    /// `passes: 0` is a loud resolution error — never a silent clamp (the same
    /// surface-never-substitute posture as `k`, sovereignty #44).
    #[serial_test::serial]
    #[test]
    fn passes_zero_errors_loudly_at_resolution() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000)]);
        let ov = ReviewResourcing { passes: Some(0), ..Default::default() };
        let err = resolve_review_resourcing(&reg, &ov).unwrap_err().to_string();
        assert!(err.contains("passes must be >= 1"), "got: {err}");
        assert!(err.contains("no ruling"), "names the consequence: {err}");
    }

    /// (#1266) An explicit `passes` reaches the JUDGE seat; absent falls back
    /// to the double-confirm default (2). The probe/verify seats ignore it.
    #[serial_test::serial]
    #[test]
    fn passes_sources_the_judge_consensus_depth_from_config() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000)]);

        let three = ReviewResourcing { passes: Some(3), ..Default::default() };
        let crew = resolve_review_resourcing(&reg, &three).unwrap();
        assert_eq!(
            crew.seats.get("review-judge").unwrap()[0].passes, 3,
            "explicit passes reaches the judge seat"
        );

        let default = ReviewResourcing::default();
        let crew = resolve_review_resourcing(&reg, &default).unwrap();
        assert_eq!(
            crew.seats.get("review-judge").unwrap()[0].passes, 2,
            "absent passes falls back to the double-confirm default"
        );
    }

    /// (#44) Every staffing carries its provenance stamp: a scored seat says
    /// so (naming the role + roster it was scored against), a pinned seat
    /// names the launch param that pinned it.
    #[serial_test::serial]
    #[test]
    fn provenance_records_scored_vs_pinned_per_seat() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("deep", vec![model("a", 40000), model("b", 40000)]);
        let ov = ReviewResourcing { judge_model: Some("b".into()), ..Default::default() };
        let crew = resolve_review_resourcing(&reg, &ov).unwrap();

        let probe_prov = crew.seats.get("review-probe").unwrap()[0].provenance.as_ref().unwrap();
        assert_eq!(probe_prov.kind, "scored");
        assert!(probe_prov.detail.contains("review-probe"), "{}", probe_prov.detail);
        assert!(probe_prov.detail.contains("deep"), "names the roster: {}", probe_prov.detail);

        let judge_prov = crew.seats.get("review-judge").unwrap()[0].provenance.as_ref().unwrap();
        assert_eq!(judge_prov.kind, "pinned");
        assert!(judge_prov.detail.contains("judge_model=b"), "{}", judge_prov.detail);

        let verify_prov = crew.seats.get("review-verify").unwrap()[0].provenance.as_ref().unwrap();
        assert_eq!(verify_prov.kind, "scored");
    }

    /// (gate coverage 13) A roster profile with an EMPTY `models[]` fails
    /// through `scored_pm` with the seat named — never a panic, never a
    /// silent empty staffing. (Normally unreachable via `load_registry`,
    /// which validates non-empty `models[]`, but the resolver takes any
    /// registry.)
    #[serial_test::serial]
    #[test]
    fn empty_models_roster_fails_scored_pm_with_the_seat_named() {
        let _guard = CrewDirGuard::empty();
        let reg = reg_with("empty", vec![]);
        // `{:#}` renders the full anyhow chain — the seat is the outer
        // context, the no-models cause is inner.
        let err = format!("{:#}", resolve_review_resourcing(&reg, &ReviewResourcing::default()).unwrap_err());
        assert!(err.contains("review-probe"), "names the first seat scored: {err}");
        assert!(
            err.contains("no models") || err.contains("no default model"),
            "names the actual problem: {err}"
        );
    }

    /// (gate coverage 9 — the resolver's headline) With a MIXED roster whose
    /// models declare capability vectors, and user role manifests whose
    /// skills request DIFFERENT capabilities per seat, probe and judge
    /// resolve to DIFFERENT models via real `select_model` scoring — not the
    /// shared default. Uses the guarded `DARKMUX_CREW_DIR` to install a
    /// synthetic role + skill fixture (user roles override built-ins by id).
    #[serial_test::serial]
    #[test]
    fn differentiated_staffing_probe_and_judge_score_to_different_models() {
        let guard = CrewDirGuard::empty();
        let roles_dir = guard.path().join("roles");
        let skills_dir = guard.path().join("skills");
        std::fs::create_dir_all(&roles_dir).unwrap();
        std::fs::create_dir_all(&skills_dir).unwrap();
        // Two skills demanding orthogonal capabilities.
        std::fs::write(
            skills_dir.join("code-heavy.json"),
            r#"{"id":"code-heavy","description":"code work","capabilities":{"code":1.0}}"#,
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("judgment-heavy.json"),
            r#"{"id":"judgment-heavy","description":"judgment work","capabilities":{"reasoning":1.0}}"#,
        )
        .unwrap();
        // User overrides for the review seats routing to those skills.
        let role = |id: &str, skill: &str| {
            format!(
                r#"{{"id":"{id}","role_family":"specialist","description":"t","skills":["{skill}"],
                    "tool_palette":{{"allow":[],"deny":[]}},"escalation_contract":"bail-with-explanation"}}"#
            )
        };
        std::fs::write(roles_dir.join("review-probe.json"), role("review-probe", "code-heavy")).unwrap();
        std::fs::write(roles_dir.join("review-judge.json"), role("review-judge", "judgment-heavy")).unwrap();
        std::fs::write(roles_dir.join("review-verify.json"), role("review-verify", "judgment-heavy")).unwrap();

        // A mixed roster: a coder-shaped model and a reasoner-shaped model.
        let mut coder = model("coder-model", 40000);
        coder.capabilities = [(darkmux_types::Capability::Code, 1.0f32)].into_iter().collect();
        let mut reasoner = model("reasoner-model", 40000);
        reasoner.capabilities =
            [(darkmux_types::Capability::Reasoning, 1.0f32)].into_iter().collect();
        let reg = reg_with("mixed", vec![coder, reasoner]);

        let crew = resolve_review_resourcing(&reg, &ReviewResourcing::default()).unwrap();
        let probe_model = &crew.seats.get("review-probe").unwrap()[0].pm.id;
        let judge_model = &crew.seats.get("review-judge").unwrap()[0].pm.id;
        assert_eq!(probe_model, "coder-model", "the code-demanding probe scores the coder");
        assert_eq!(judge_model, "reasoner-model", "the judgment-demanding judge scores the reasoner");
        assert_ne!(probe_model, judge_model, "a mixed roster differentiates the seats");
    }

    // ─── (#1475 packet 2) role→profile flip resolver ───────────────────────

    /// A multi-profile registry (no `default_profile` unless a test sets one),
    /// for exercising distinct role→profile bindings.
    fn multi_reg(profiles: Vec<(&str, Vec<ProfileModel>)>, default: Option<&str>) -> ProfileRegistry {
        let map = profiles
            .into_iter()
            .map(|(name, models)| (name.to_string(), Profile { models, ..Default::default() }))
            .collect();
        ProfileRegistry { profiles: map, default_profile: default.map(String::from), ..Default::default() }
    }

    /// A `mapped` closure over a fixed role→profile table — the injected
    /// `config.json` binding the pure resolver reads.
    fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(r, p)| (r.to_string(), p.to_string())).collect()
    }

    /// (#1475 packet 2, headline) Five roles → five bindings → a heterogeneous
    /// crew. Distinct probe roles resolve to distinct profiles/models straight
    /// from the map — no roster scoring, no launch pins.
    #[test]
    fn role_crew_staffs_five_roles_from_the_map() {
        let reg = multi_reg(
            vec![
                ("p27", vec![model("m27", 40000)]),
                ("pdev", vec![model("mdev", 40000)]),
                ("p4b", vec![model("m4b", 32000)]),
                ("p35", vec![model("m35", 60000)]),
            ],
            Some("p4b"),
        );
        let bindings = map_of(&[
            ("review-probe-high", "p27"),
            ("review-probe-mid", "pdev"),
            ("review-probe-low", "p4b"),
            ("review-judge", "p35"),
            ("review-verify", "p35"),
        ]);
        let crew =
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| bindings.get(r).cloned())
                .unwrap();

        let probes = crew.seats.get(REVIEW_PROBE_ROLE).unwrap();
        assert_eq!(probes.len(), 3, "one staffing per distinct probe role");
        assert_eq!(probes[0].role_id.as_deref(), Some("review-probe-high"));
        assert_eq!(probes[0].pm.id, "m27", "probe-high → p27 → m27");
        assert_eq!(probes[1].pm.id, "mdev", "probe-mid → pdev → mdev");
        assert_eq!(probes[2].pm.id, "m4b", "probe-low → p4b → m4b");
        assert_eq!(probes[0].k, 1, "one draw per probe role — diversity is role-borne, not k-borne");
        assert_eq!(crew.seats.get(REVIEW_JUDGE_ROLE).unwrap()[0].pm.id, "m35");
        assert_eq!(crew.seats.get(REVIEW_VERIFY_ROLE).unwrap()[0].pm.id, "m35");
        // The crew name is the distinct profile set (heterogeneous here).
        assert_eq!(crew.name, "p27+p35+p4b+pdev");
        // Provenance records role→profile, not scored/pinned.
        let prov = probes[0].provenance.as_ref().unwrap();
        assert_eq!(prov.kind, "role-profile");
        assert!(prov.detail.contains("review-probe-high"), "{}", prov.detail);
        assert!(prov.detail.contains("p27"), "{}", prov.detail);
        assert!(prov.detail.contains("role_profiles map"), "names the binding source: {}", prov.detail);
    }

    /// (#1475 packet 2) An UNMAPPED role falls through to `default_profile` —
    /// the fresh-user floor. An empty map runs every seat on one model.
    #[test]
    fn unmapped_roles_fall_back_to_default_profile() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let crew = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|_| None).unwrap();
        for seat in crew.seats.values().flatten() {
            assert_eq!(seat.pm.id, "only", "every unmapped seat → default_profile's model");
            assert!(
                seat.provenance.as_ref().unwrap().detail.contains("default_profile fallback"),
                "provenance names the fallback"
            );
        }
        assert_eq!(crew.name, "solo", "homogeneous crew reads as the one profile");
    }

    /// (#1475 packet 2) A role mapped to a profile absent from the registry is a
    /// loud resolution error (packet 1's message), never a silent fallback.
    #[test]
    fn role_mapped_to_missing_profile_errors_loudly() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let bindings = map_of(&[("review-judge", "ghost")]);
        // `{:#}` renders the full anyhow chain — the seat context is the outer
        // layer, packet 1's missing-profile message the inner cause.
        let err = format!(
            "{:#}",
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| bindings.get(r).cloned())
                .unwrap_err()
        );
        assert!(err.contains("review-judge"), "names the role: {err}");
        assert!(err.contains("ghost"), "names the missing profile: {err}");
    }

    /// (#1475 packet 2) `passes >= 1` is enforced here too (the mission-launch
    /// path has no clap guard) — a zero-pass judge is a loud error.
    #[test]
    fn role_crew_passes_zero_errors() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let ov = ReviewRoleStaffing { passes: Some(0), ..Default::default() };
        let err = resolve_review_role_crew_with(&reg, &ov, &|_| None).unwrap_err().to_string();
        assert!(err.contains("passes must be >= 1"), "got: {err}");
    }

    /// (#1475 packet 2) A LOCAL seat whose resolved model declares no `n_ctx`
    /// fails at resolution, naming the role — same guard the roster resolver
    /// applies (a local model is loaded at its declared context).
    #[test]
    fn role_crew_local_seat_without_n_ctx_fails() {
        let reg = multi_reg(
            vec![("solo", vec![ProfileModel { id: "no-ctx".into(), ..Default::default() }])],
            Some("solo"),
        );
        let err = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("n_ctx"), "names the field: {err}");
    }

    /// (#1475 packet 2) A remote-bound seat needs no `n_ctx` — nothing is
    /// loaded locally.
    #[test]
    fn role_crew_remote_seat_needs_no_n_ctx() {
        let reg = multi_reg(
            vec![("local", vec![model("m", 32000)]), ("cloud", vec![remote("gpt")])],
            Some("local"),
        );
        let bindings = map_of(&[("review-verify", "cloud")]);
        let crew =
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| bindings.get(r).cloned())
                .unwrap();
        let verify = &crew.seats.get(REVIEW_VERIFY_ROLE).unwrap()[0];
        assert_eq!(verify.pm.id, "gpt");
        assert!(verify.pm.is_remote(), "a remote-bound verify seat needs no n_ctx");
    }
}
