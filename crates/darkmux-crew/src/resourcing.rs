//! (#1475, dissolved-probe-role refactor #1512) The review resourcing
//! resolver — the single planning step that staffs a review's roles via the
//! role→profile flip: each review role resolves INDEPENDENTLY through the
//! machine-local `role_profiles` map (with a per-run launch override on
//! top, and `default_profile` as the fresh-user floor). It hands the review
//! driver a [`ResolvedCrew`] whose `staffing` snapshot records what
//! resolved and WHY (role → profile → model → binding-source), so the run's
//! envelope shows truth (operator sovereignty #44).
//!
//! **There is no "probe role" concept (#1512).** A probe is a TASK that
//! carries one `role_id` and a probe-kind step — "probe" is emergent from
//! that composition, never a family this module enumerates. The probe role
//! ids this resolver staffs are supplied by the CALLER
//! (`darkmux_crew::mission_config::discover_review_probe_role_ids`, read
//! straight off `review.json`'s `review-dedup-task.depends_on`), never a
//! Rust-side constant array — a config edit that adds/removes a probe task
//! changes the probe count with zero Rust changes.
//!
//! A crew is a DERIVED VIEW of a mission's resourcing, never a declared entity:
//! nobody keeps a registry of pre-formed crews awaiting missions. There is a
//! corps (the profile registry), there is planning, and crew assignment is an
//! OUTPUT. The roster-scoring resolver (`select_model` per seat against one
//! roster profile) it replaced was deleted in #1475 packet 3 — recall diversity
//! now falls out of distinct probe role→profile bindings, not `k` draws
//! of one scored model.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_profiles::profiles::RoleBinding;
use darkmux_types::{BundleSelector, ProfileModel, ProfileRegistry};
use std::collections::BTreeMap;

/// Canonical review seat ids the review driver's `validate_review_crew`
/// reads in [`ResolvedCrew::seats`] — judge and verify are single, named
/// roles (each maps to exactly one seat).
pub const REVIEW_JUDGE_ROLE: &str = "review-judge";
pub const REVIEW_VERIFY_ROLE: &str = "review-verify";
/// The seats-map family key every resolved PROBE staffing lands under,
/// regardless of how many distinct probe roles were staffed. Unlike
/// [`REVIEW_JUDGE_ROLE`]/[`REVIEW_VERIFY_ROLE`], this is a plain
/// aggregation label, NOT a role id and NOT a count (#1512) — however many
/// probe roles `review.json` declares land under it, discovered by the
/// caller via `mission_config::discover_review_probe_role_ids`, never
/// enumerated here.
const REVIEW_PROBE_FAMILY_KEY: &str = "review-probe";

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
    /// Historically the probe-seat draw BREADTH (a union over multiple
    /// dispatches of the same role). (#1512) `build_review_graph` no longer
    /// multiplies a probe role's task by `k` — one role is one task is one
    /// dispatch; recall breadth is now a review.json edit (declare another
    /// probe role), never a per-run draw multiplier. The field survives for
    /// back-compat (envelope staffing snapshots, `review-bench --k`
    /// reporting) and is always `1` for every seat resourcing.rs resolves.
    /// Ignored by the judge/verify seats regardless.
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

/// (#1475, #1512) THE FLIP — staff the review crew from the machine-local
/// role→profile map, with a per-run launch override on top. `probe_role_ids`
/// names the probe roles to staff, one staffing each — supplied by the
/// CALLER (via `mission_config::discover_review_probe_role_ids`, reading
/// `review.json`'s own declared probe tasks), never hardcoded here (#1512:
/// there is no Rust-side enumeration of "the probe roles" — however many
/// `review.json` declares, that many get staffed). Every named role
/// (probes + judge + verify) resolves INDEPENDENTLY through
/// [`darkmux_profiles::profiles::resolve_role_profile_with`]: role →
/// binding → profile → model. Binding precedence (packet 3):
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
/// consume. Every resolved probe staffing lands under the `"review-probe"`
/// family key (what `validate_review_crew` reads), each carrying its own
/// distinct `role_id`; judge/verify each get their single seat. Operator
/// sovereignty (#44) is intact: the resolved truth — role → profile → model →
/// binding-source — is stamped on every seat's provenance.
pub fn resolve_review_role_crew(
    reg: &ProfileRegistry,
    ov: &ReviewRoleStaffing,
    overrides: &BTreeMap<String, String>,
    probe_role_ids: &[String],
) -> Result<ResolvedCrew> {
    resolve_review_role_crew_with(reg, ov, probe_role_ids, &|role| {
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

/// (#1475, #1512) Pure core of [`resolve_review_role_crew`] — the role→profile
/// binding lookup is INJECTED (`mapped(role_id)` = the resolved
/// [`RoleBinding`] for that role: override / map / unmapped), so the whole
/// flip is unit-testable against a temp table without the process-wide
/// `config()`. Mirrors packet 1's `resolve_role_profile_with` seam.
pub fn resolve_review_role_crew_with(
    reg: &ProfileRegistry,
    ov: &ReviewRoleStaffing,
    probe_role_ids: &[String],
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
    if probe_role_ids.is_empty() {
        bail!(
            "darkmux: review resourcing: no probe roles to staff — \"review\" mission config's \
             \"review-dedup-task\" must depend on at least one probe task (#1512)"
        );
    }
    let judge_passes = ov.passes.unwrap_or(DEFAULT_JUDGE_PASSES);

    let mut seats: BTreeMap<String, Vec<ResolvedSeatStaffing>> = BTreeMap::new();

    // Probe seat family: one staffing per distinct probe role, whatever
    // `probe_role_ids` names (#1512 — config-driven, not a Rust constant).
    // Recall diversity is the distinct role→profile→model bindings — no k
    // draw-fanout of one model (k=1 per probe role).
    let mut probes = Vec::with_capacity(probe_role_ids.len());
    for role_id in probe_role_ids {
        probes.push(staff_review_role(reg, role_id, mapped, 1, DEFAULT_JUDGE_PASSES)?);
    }
    seats.insert(REVIEW_PROBE_FAMILY_KEY.to_string(), probes);

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

// ─── (#1512) config-driven probe role discovery ─────────────────────────────

/// (#1512) Discover the review's probe role ids directly off the "review"
/// mission config's declared graph shape — never a Rust-side enumeration.
/// The probe stage's own doctrine (see review.json's top-level
/// `description`): `"review-dedup-task"` depends on exactly the probe
/// tasks, each carrying its own `role_id`. Reading that `depends_on` list
/// IS discovering "the probe roles" — no other source of truth exists.
/// Order follows `review-dedup-task.depends_on` verbatim, which is also the
/// order [`resolve_review_role_crew`] staffs them in.
///
/// This is what makes the probe count config-driven end to end (#1512's
/// payoff): editing `review.json` to add/remove a probe task (with its own
/// `role_id`, wired into `review-dedup-task.depends_on`) changes the probe
/// count for every caller of this function — `mission_launch_review.rs`,
/// `review_bench.rs`'s `--funnel` — with zero Rust changes.
pub fn discover_review_probe_role_ids(
    config: &crate::mission_config::MissionConfig,
) -> Result<Vec<String>> {
    let all_tasks: BTreeMap<&str, &crate::mission_config::TaskConfig> =
        config.phases.iter().flat_map(|p| p.tasks.iter()).map(|t| (t.id.as_str(), t)).collect();
    let dedup = all_tasks.get("review-dedup-task").ok_or_else(|| {
        anyhow!(
            "darkmux: \"review\" mission config has no \"review-dedup-task\" task — cannot \
             discover the probe roles (#1512)"
        )
    })?;
    dedup
        .depends_on
        .iter()
        .map(|dep_id| {
            let task = all_tasks.get(dep_id.as_str()).ok_or_else(|| {
                anyhow!(
                    "darkmux: \"review\" mission config: \"review-dedup-task\" depends on \
                     \"{dep_id}\", which doesn't exist"
                )
            })?;
            task.role_id.clone().ok_or_else(|| {
                anyhow!(
                    "darkmux: \"review\" mission config: probe task \"{dep_id}\" has no \
                     role_id — every probe task \"review-dedup-task\" depends on must declare \
                     one (#1512)"
                )
            })
        })
        .collect()
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

    /// (#1512) The default three-probe-role set — what `review.json` ships
    /// today. Tests exercising other counts build their own `Vec<String>`.
    fn default_probe_role_ids() -> Vec<String> {
        vec!["review-probe-high".to_string(), "review-probe-mid".to_string(), "review-probe-low".to_string()]
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
        let crew = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &default_probe_role_ids(),
            &|r| map_binding(&bindings, r),
        )
        .unwrap();

        let probes = crew.seats.get("review-probe").unwrap();
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
        let crew = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &default_probe_role_ids(),
            &|r| {
                // Precedence encoded exactly as the production closure does it.
                if let Some(p) = overrides.get(r) {
                    RoleBinding::Overridden(p.clone())
                } else {
                    map_binding(&map, r)
                }
            },
        )
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
        let crew = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &default_probe_role_ids(),
            &|_| RoleBinding::Unmapped,
        )
        .unwrap();
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
            resolve_review_role_crew_with(
                &reg,
                &ReviewRoleStaffing::default(),
                &default_probe_role_ids(),
                &|r| map_binding(&bindings, r),
            )
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
        let err = resolve_review_role_crew_with(&reg, &ov, &default_probe_role_ids(), &|_| RoleBinding::Unmapped)
            .unwrap_err()
            .to_string();
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
        let err = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &default_probe_role_ids(),
            &|_| RoleBinding::Unmapped,
        )
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
        let crew = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &default_probe_role_ids(),
            &|r| map_binding(&bindings, r),
        )
        .unwrap();
        let verify = &crew.seats.get(REVIEW_VERIFY_ROLE).unwrap()[0];
        assert_eq!(verify.pm.id, "gpt");
        assert!(verify.pm.is_remote(), "a remote-bound verify seat needs no n_ctx");
    }

    // ─── (#1512) config-driven probe count — the payoff ──────────────────────

    /// A minimal, hand-built "review" mission config JSON naming exactly
    /// `n` probe tasks, each with its own `role_id`, all depended on by
    /// `review-dedup-task` — the shape [`discover_review_probe_role_ids`]
    /// reads. Mirrors the real `review.json`'s structure without the prose.
    fn review_config_with_n_probes(n: usize) -> crate::mission_config::MissionConfig {
        let probe_ids: Vec<String> = (0..n).map(|i| format!("probe-{i}")).collect();
        let probe_tasks: Vec<serde_json::Value> = probe_ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": format!("review-probe-{id}-task"),
                    "role_id": id,
                    "depends_on": ["review-bundle-task"],
                    "steps": [{"id": format!("review-probe-{id}-step"), "kind": "dispatch.map"}]
                })
            })
            .collect();
        let dedup_depends_on: Vec<String> =
            probe_ids.iter().map(|id| format!("review-probe-{id}-task")).collect();
        let mut tasks = vec![serde_json::json!({
            "id": "review-bundle-task",
            "depends_on": [],
            "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]
        })];
        tasks.extend(probe_tasks);
        tasks.push(serde_json::json!({
            "id": "review-dedup-task",
            "depends_on": dedup_depends_on,
            "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]
        }));
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [{"id": "investigate", "tasks": tasks}]
        });
        serde_json::from_value(doc).expect("hand-built review config parses")
    }

    /// The headline #1512 assertion: discovery reads the probe role ids
    /// straight off `review-dedup-task.depends_on`, in order — no Rust
    /// constant involved. Three probes today (review.json's default), but
    /// this function doesn't know that number; it counts whatever the
    /// document declares.
    #[test]
    fn discover_review_probe_role_ids_reads_dedups_depends_on_in_order() {
        let config = review_config_with_n_probes(3);
        let ids = discover_review_probe_role_ids(&config).unwrap();
        assert_eq!(ids, vec!["probe-0", "probe-1", "probe-2"]);
    }

    /// (#1512 payoff) A ONE-probe config — the Studio 32GB case named in
    /// the issue — discovers exactly one role id. Changing the probe count
    /// is purely a document edit; this function's behavior doesn't change.
    #[test]
    fn discover_review_probe_role_ids_supports_a_one_probe_config() {
        let config = review_config_with_n_probes(1);
        let ids = discover_review_probe_role_ids(&config).unwrap();
        assert_eq!(ids, vec!["probe-0"]);
    }

    /// A five-probe config (the issue's other named example) discovers all
    /// five, still with zero Rust changes.
    #[test]
    fn discover_review_probe_role_ids_supports_a_five_probe_config() {
        let config = review_config_with_n_probes(5);
        let ids = discover_review_probe_role_ids(&config).unwrap();
        assert_eq!(ids.len(), 5);
    }

    /// A task `review-dedup-task` depends on but that carries no `role_id`
    /// is a loud, named resolution error — never a silently-skipped probe.
    #[test]
    fn discover_review_probe_role_ids_errors_loudly_on_a_roleless_dependency() {
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [{"id": "investigate", "tasks": [
                {"id": "review-dedup-task", "depends_on": ["review-bundle-task"], "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]},
                {"id": "review-bundle-task", "depends_on": [], "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]}
            ]}]
        });
        let config: crate::mission_config::MissionConfig = serde_json::from_value(doc).unwrap();
        let err = discover_review_probe_role_ids(&config).unwrap_err().to_string();
        assert!(err.contains("review-bundle-task"), "names the roleless task: {err}");
        assert!(err.contains("role_id"), "{err}");
    }

    /// A config whose `review-dedup-task` is missing entirely is a loud,
    /// named error — never a panic or an empty-probe-count silent pass.
    #[test]
    fn discover_review_probe_role_ids_errors_loudly_when_dedup_task_is_missing() {
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [{"id": "investigate", "tasks": []}]
        });
        let config: crate::mission_config::MissionConfig = serde_json::from_value(doc).unwrap();
        let err = discover_review_probe_role_ids(&config).unwrap_err().to_string();
        assert!(err.contains("review-dedup-task"), "{err}");
    }

    /// Feeding a resolver an empty `probe_role_ids` slice (a config whose
    /// dedup task depends on nothing) is a loud error — a review with zero
    /// probes is not a degraded review, it's a misconfigured one.
    #[test]
    fn resolve_review_role_crew_with_empty_probe_role_ids_errors_loudly() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let err = resolve_review_role_crew_with(&reg, &ReviewRoleStaffing::default(), &[], &|_| {
            RoleBinding::Unmapped
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no probe roles"), "{err}");
    }

    /// (#1512 payoff, end to end) A crew resolved from a ONE-probe-role
    /// list carries exactly one probe staffing — the resolver doesn't
    /// silently pad to three.
    #[test]
    fn resolve_review_role_crew_with_one_probe_role_staffs_exactly_one() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let crew = resolve_review_role_crew_with(
            &reg,
            &ReviewRoleStaffing::default(),
            &["review-probe-only".to_string()],
            &|_| RoleBinding::Unmapped,
        )
        .unwrap();
        let probes = crew.seats.get("review-probe").unwrap();
        assert_eq!(probes.len(), 1, "exactly one probe role staffed, matching what was asked");
        assert_eq!(probes[0].role_id.as_deref(), Some("review-probe-only"));
    }
}
