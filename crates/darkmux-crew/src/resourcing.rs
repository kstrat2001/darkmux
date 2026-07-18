//! (#1475) The review resourcing resolver — the single planning step that
//! staffs a review's seats via the role→profile flip: each review role
//! (`review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`)
//! resolves INDEPENDENTLY through the machine-local `role_profiles` map (with a
//! per-run launch override on top, and `default_profile` as the fresh-user
//! floor). It hands the review driver a [`ResolvedCrew`] whose `staffing`
//! snapshot records what resolved and WHY (role → profile → model →
//! binding-source), so the run's envelope shows truth (operator sovereignty
//! #44).
//!
//! A crew is a DERIVED VIEW of a mission's resourcing, never a declared entity:
//! nobody keeps a registry of pre-formed crews awaiting missions. There is a
//! corps (the profile registry), there is planning, and crew assignment is an
//! OUTPUT. The roster-scoring resolver (`select_model` per seat against one
//! roster profile) it replaced was deleted in #1475 packet 3 — recall diversity
//! now falls out of three distinct probe role→profile bindings, not `k` draws
//! of one scored model.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_profiles::profiles::RoleBinding;
use darkmux_types::{BundleSelector, ProfileModel, ProfileRegistry};
use std::collections::BTreeMap;

/// Canonical review seat FAMILY role ids — the keys the review driver's
/// `validate_review_crew` reads in [`ResolvedCrew::seats`]. The probe family
/// key stays [`REVIEW_PROBE_ROLE`] even though three distinct probe roles staff
/// under it (each carrying its own `role_id`).
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

/// Judge-seat consensus depth default (double-confirm). Governs the JUDGE seat
/// only; the probe/verify seats ignore `passes`.
const DEFAULT_JUDGE_PASSES: u32 = 2;

/// (#1426 ship-2 / #44) How a seat's model was chosen. The resolver stamps
/// this on every staffing so the envelope's staffing snapshot answers "where
/// did this decision come from" directly — the operator never has to wonder
/// whether a seat was scored or pinned (operator sovereignty #44: system
/// proposes, operator overrides, record shows truth AND why).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StaffingProvenance {
    /// `"role-profile"` today (a seat staffed by the role→profile flip). A plain
    /// string, not an enum, so snapshot consumers stay lenient to future kinds.
    pub kind: String,
    /// Names the whole role→profile→model→binding-source chain.
    pub detail: String,
}

impl StaffingProvenance {
    /// (#1475) A seat staffed by the role→profile flip: the role was resolved by
    /// a per-run launch override (`source = "launch override"`), through the
    /// machine-local `role_profiles` map (`source = "role_profiles map"`), or
    /// fell through to `default_profile` (`source = "default_profile fallback"`,
    /// the fresh-user floor). Names the whole role→profile→model chain so the
    /// envelope answers "where did this seat's model come from" directly
    /// (operator sovereignty #44).
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
/// output. The review driver + envelope snapshot consume it unchanged.
#[derive(Debug, Clone)]
pub struct ResolvedSeatStaffing {
    /// The [`Profile`](darkmux_types::Profile) name this seat's role resolved to
    /// (via the role→profile flip) and dispatches through.
    pub name: String,
    /// (#1475) The review ROLE this seat was staffed for —
    /// `review-probe-high`/`-mid`/`-low`, `review-judge`, or `review-verify`.
    /// `None` only for hand-built test staffings. The envelope snapshot records
    /// it so a run names which role bound each seat.
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
    /// (#1475 / #44) The role→profile→model→binding-source chain, stamped by the
    /// resolver; `None` only for hand-built staffings (tests, synthetic paths).
    pub provenance: Option<StaffingProvenance>,
}

/// A fully-resolved review crew: every seat bound to a concrete model, keyed
/// by seat FAMILY role id. The review driver + envelope snapshot consume this.
#[derive(Debug, Clone)]
pub struct ResolvedCrew {
    /// The derived crew's addressable identity — the distinct set of resolved
    /// profile names across all seats (see [`role_crew_name`]); there is no
    /// declared crew name (#1426 ship-2).
    pub name: String,
    pub seats: BTreeMap<String, Vec<ResolvedSeatStaffing>>,
    /// Whether confirmed findings render as a blocking `REQUEST_CHANGES` review
    /// (`true`) or a non-blocking `COMMENT` review (`false`, the default).
    pub request_changes: bool,
}

// ─── (#1475) role→profile staffing — THE FLIP ───────────────────────────────

/// (#1475) Launch-time knobs for the role→profile review crew. The per-seat
/// MODEL is not chosen here — each seat's model resolves from its role's
/// binding (launch override > `role_profiles` map > `default_profile`). Only the
/// non-model knobs live here: the judge's consensus DEPTH and the render's
/// blocking-vs-advisory choice. Per-run PROFILE overrides are a SEPARATE arg to
/// [`resolve_review_role_crew`] (`overrides: role → profile`), not a field here.
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

/// (#1475) THE FLIP — staff the review crew from the machine-local role→profile
/// map, with a per-run launch override on top. Each of the five review roles
/// (`review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`) resolves
/// INDEPENDENTLY through [`darkmux_profiles::profiles::resolve_role_profile_with`]:
/// role → binding → profile → model. Binding precedence (packet 3):
///
/// 1. `overrides` — a per-run `--param <role>=<profile>` launch override.
/// 2. the machine-local `role_profiles` map in `config.json`.
/// 3. `default_profile` — the fresh-user floor (a bare machine with an empty map
///    runs every seat on one model: works, no diversity; the operator populates
///    the map, or overrides per run, for the real heterogeneous crew).
///
/// A role bound (by override OR map) to a profile absent from the registry is a
/// loud resolution error (`resolve_role_profile_with` owns that message; the
/// override case names the launch line, the map case names `config set`).
///
/// Hands back the [`ResolvedCrew`] shape the review driver + envelope snapshot
/// consume. The three probe staffings land under the [`REVIEW_PROBE_ROLE`]
/// family key (what `validate_review_crew` reads), each carrying its own
/// distinct `role_id`; judge/verify each get their single seat. Operator
/// sovereignty (#44) is intact: the resolved truth — role → profile → model →
/// binding-source — is stamped on every seat's provenance.
pub fn resolve_review_role_crew(
    reg: &ProfileRegistry,
    ov: &ReviewRoleStaffing,
    overrides: &BTreeMap<String, String>,
) -> Result<ResolvedCrew> {
    resolve_review_role_crew_with(reg, ov, &|role| {
        // Precedence: launch override > role_profiles map > unmapped (default).
        if let Some(p) = overrides.get(role).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            RoleBinding::Overridden(p.to_string())
        } else if let Some(p) = darkmux_types::config_access::role_profile(role) {
            RoleBinding::Mapped(p)
        } else {
            RoleBinding::Unmapped
        }
    })
}

/// (#1475) Pure core of [`resolve_review_role_crew`] — the role→profile binding
/// lookup is INJECTED (`mapped(role_id)` = the resolved [`RoleBinding`] for that
/// role: override / map / unmapped), so the whole flip is unit-testable against
/// a temp table without the process-wide `config()`. Mirrors packet 1's
/// `resolve_role_profile_with` seam.
pub fn resolve_review_role_crew_with(
    reg: &ProfileRegistry,
    ov: &ReviewRoleStaffing,
    mapped: &dyn Fn(&str) -> RoleBinding,
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
    mapped: &dyn Fn(&str) -> RoleBinding,
    k: u32,
    passes: u32,
) -> Result<ResolvedSeatStaffing> {
    let binding = mapped(role_id);
    let resolved =
        darkmux_profiles::profiles::resolve_role_profile_with(role_id, &binding, reg)
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
        darkmux_profiles::profiles::RoleProfileSource::Overridden => "launch override",
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

    // ─── (#1475) role→profile flip resolver ─────────────────────────────────

    /// A multi-profile registry (no `default_profile` unless a test sets one),
    /// for exercising distinct role→profile bindings.
    fn multi_reg(profiles: Vec<(&str, Vec<ProfileModel>)>, default: Option<&str>) -> ProfileRegistry {
        let map = profiles
            .into_iter()
            .map(|(name, models)| (name.to_string(), Profile { models, ..Default::default() }))
            .collect();
        ProfileRegistry { profiles: map, default_profile: default.map(String::from), ..Default::default() }
    }

    /// A fixed role→profile table for building injected bindings in a test.
    fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(r, p)| (r.to_string(), p.to_string())).collect()
    }

    /// Resolve a role against a fixed table into the `Mapped`/`Unmapped`
    /// [`RoleBinding`] the injected `mapped` closure yields (the `config.json`
    /// map tier). `Overridden` is exercised by its own test.
    fn map_binding(bindings: &BTreeMap<String, String>, role: &str) -> RoleBinding {
        match bindings.get(role) {
            Some(p) => RoleBinding::Mapped(p.clone()),
            None => RoleBinding::Unmapped,
        }
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
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| map_binding(&bindings, r))
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

    /// (#1475 packet 3) A per-run launch override wins over the `role_profiles`
    /// map, and the seat's provenance names the override tier. Here the map binds
    /// review-judge → p35, but the override binds it → pdev; the override wins.
    #[test]
    fn launch_override_wins_over_map_and_records_provenance() {
        let reg = multi_reg(
            vec![
                ("p35", vec![model("m35", 60000)]),
                ("pdev", vec![model("mdev", 40000)]),
                ("p4b", vec![model("m4b", 32000)]),
            ],
            Some("p4b"),
        );
        let map = map_of(&[("review-judge", "p35")]);
        let overrides = map_of(&[("review-judge", "pdev")]);
        let crew = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| {
            // Precedence encoded exactly as the production closure does it.
            if let Some(p) = overrides.get(r) {
                RoleBinding::Overridden(p.clone())
            } else {
                map_binding(&map, r)
            }
        })
        .unwrap();
        let judge = &crew.seats.get(REVIEW_JUDGE_ROLE).unwrap()[0];
        assert_eq!(judge.pm.id, "mdev", "override → pdev → mdev beats the map's p35");
        let prov = judge.provenance.as_ref().unwrap();
        assert!(prov.detail.contains("launch override"), "provenance names the tier: {}", prov.detail);
        assert!(prov.detail.contains("pdev"), "names the overriding profile: {}", prov.detail);
    }

    /// (#1475 packet 2) An UNMAPPED role falls through to `default_profile` —
    /// the fresh-user floor. An empty map runs every seat on one model.
    #[test]
    fn unmapped_roles_fall_back_to_default_profile() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let crew = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped).unwrap();
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
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| map_binding(&bindings, r))
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
        let err = resolve_review_role_crew_with(&reg, &ov, &|_| RoleBinding::Unmapped).unwrap_err().to_string();
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
        let err = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
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
            resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &|r| map_binding(&bindings, r))
                .unwrap();
        let verify = &crew.seats.get(REVIEW_VERIFY_ROLE).unwrap()[0];
        assert_eq!(verify.pm.id, "gpt");
        assert!(verify.pm.is_remote(), "a remote-bound verify seat needs no n_ctx");
    }
}
