//! (#1475, dissolved-probe-role refactor #1512, #1513 review finding) The
//! review resourcing resolver — the single planning step that staffs a
//! review's roles via the role→profile flip: each review role resolves
//! INDEPENDENTLY through the machine-local `role_profiles` map (with a
//! per-run launch override on top, and `default_profile` as the fresh-user
//! floor). It hands the review driver a [`ResolvedReviewRoles`] whose
//! `staffing` snapshot records what resolved and WHY (role → profile →
//! model → binding-source), so the run's envelope shows truth (operator
//! sovereignty #44).
//!
//! **There is no "probe role" concept, and no "crew" concept (#1512, #1513
//! review).** A probe is a TASK that carries one `role_id` and a probe-kind
//! step — "probe" is emergent from that composition, never a family this
//! module enumerates. [`resolve_review_roles`] is the ONE generic
//! resolution pass: it walks the "review" mission config's own declared
//! tasks, classifies each role-bearing task STRUCTURALLY by which Tier-3
//! step kind it carries (a `"review.judge"` step ⇒ the judge task; a
//! `"review.verify-render"` step ⇒ the verify task; anything else with a
//! `role_id` ⇒ a probe task), and resolves every one of them through the
//! SAME per-task primitive, [`resolve_task_role`]. There is no Rust-side
//! enumeration of "the probe roles" (no array, no magic-string heuristic
//! reading `review-dedup-task.depends_on`), no `seats: BTreeMap<String,
//! Vec<_>>` family grouping, and no separate "crew" type — the probe
//! COUNT and every role's identity fall out of whatever `review.json`
//! declares, read directly off the document each time.
//!
//! A `ResolvedReviewRoles` is a DERIVED VIEW of a mission's resourcing,
//! never a declared entity: nobody keeps a registry of pre-formed crews
//! awaiting missions. There is a corps (the profile registry), there is
//! planning, and staffing is an OUTPUT. The roster-scoring resolver
//! (`select_model` per seat against one roster profile) it replaced was
//! deleted in #1475 packet 3 — recall diversity now falls out of distinct
//! probe role→profile bindings, not `k` draws of one scored model.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_profiles::profiles::RoleBinding;
use darkmux_types::{BundleSelector, ProfileModel, ProfileRegistry};

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
    /// (#1475) The review ROLE this seat was staffed for — whatever
    /// `role_id` the owning task declares. `None` only for hand-built test
    /// staffings. The envelope snapshot records it so a run names which
    /// role bound each seat.
    pub role_id: Option<String>,
    pub pm: ProfileModel,
    /// Historically the probe-seat draw BREADTH (a union over multiple
    /// dispatches of the same role). (#1512) `build_review_graph` no longer
    /// multiplies a probe role's task by `k` — one role is one task is one
    /// dispatch; recall breadth is now a review.json edit (declare another
    /// probe role), never a per-run draw multiplier. The field survives for
    /// back-compat (envelope staffing snapshots, `review-bench --k`
    /// reporting) and is always `1` for every seat this module resolves.
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

/// (#1512, #1513 review) Every review role this run resolved: however many
/// probe roles `review.json` declares, the one judge role, and the OPTIONAL
/// verify role. **Not a "crew"** — no `seats: BTreeMap<String, Vec<_>>`
/// family grouping, no addressable "crew name" beyond the derived
/// [`distinct_profile_names`](Self::distinct_profile_names) display string.
/// This is simply the concrete return shape [`resolve_review_roles`]
/// produces and [`crate::mission_config`]-driven callers thread straight
/// into `build_review_graph`/`staffing_snapshot`/`run_review_graph`.
#[derive(Debug, Clone)]
pub struct ResolvedReviewRoles {
    pub probes: Vec<ResolvedSeatStaffing>,
    pub judge: ResolvedSeatStaffing,
    /// (#1260) Present iff `review.json` declares a task whose step carries
    /// the `"review.verify-render"` kind AND that role resolved. Genuinely
    /// optional — a review with no verify stage is a valid configuration.
    pub verify: Option<ResolvedSeatStaffing>,
    /// Whether confirmed findings render as a blocking `REQUEST_CHANGES` review
    /// (`true`) or a non-blocking `COMMENT` review (`false`, the default).
    pub request_changes: bool,
}

impl ResolvedReviewRoles {
    /// The run's display identity: the DISTINCT resolved profile names
    /// across every role, sorted + `+`-joined. A homogeneous fresh-user run
    /// (every role → default_profile) reads as that one profile name; a
    /// heterogeneous run reads as `devstral+qwen27b+qwen35b+qwen4b`. Shown
    /// on bookend records + the mission board — the honest set of profiles
    /// in play, never a fabricated single "roster" (#1426 ship-2: there is
    /// no declared crew name).
    pub fn distinct_profile_names(&self) -> String {
        let mut names: Vec<&str> = self
            .probes
            .iter()
            .map(|s| s.name.as_str())
            .chain(std::iter::once(self.judge.name.as_str()))
            .chain(self.verify.iter().map(|s| s.name.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() {
            "review".to_string()
        } else {
            names.join("+")
        }
    }
}

// ─── (#1475) role→profile staffing — THE FLIP ───────────────────────────────

/// (#1475) Launch-time knobs for the role→profile review resolution. The
/// per-seat MODEL is not chosen here — each seat's model resolves from its
/// role's binding (launch override > `role_profiles` map > `default_profile`).
/// Only the non-model knobs live here: the judge's consensus DEPTH and the
/// render's blocking-vs-advisory choice. Per-run PROFILE overrides are a
/// SEPARATE arg to [`resolve_review_roles`] (the injected `mapped` binding
/// lookup), not a field here.
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

/// (#1512, #1513 review) THE dissolution: staff every role-bearing task in
/// the "review" mission config through ONE generic per-task resolver — no
/// review-specific "crew"/"seat family" concept, no magic-string discovery.
///
/// A task's role is classified STRUCTURALLY, by which Tier-3 step kind it
/// carries (the same kind ids `build_review_graph` already keys its own
/// task lookups on — `"review.judge"` for the judge stage,
/// `"review.verify-render"` for the verify stage — never a fixed task id or
/// a `depends_on` heuristic):
///
/// - a task with a `"review.judge"` step ⇒ THE judge task (exactly one
///   required);
/// - a task with a `"review.verify-render"` step ⇒ THE verify task (at
///   most one; genuinely absent is a valid config — #1260);
/// - every OTHER task that declares a `role_id` ⇒ a probe task, in
///   document order (however many `review.json` lists — #1512's payoff:
///   the count is never a Rust-side constant).
///
/// Every classified role then resolves INDEPENDENTLY through
/// [`resolve_task_role`] (`role → binding → profile → model`, the SAME
/// primitive for every role, no parallel resolution path). Binding
/// precedence (packet 3):
///
/// 1. `mapped(role_id)` returning `Overridden` — a per-run
///    `--param <role>=<profile>` launch override.
/// 2. `Mapped` — the machine-local `role_profiles` map in `config.json`.
/// 3. `Unmapped` — falls through to `default_profile`, the fresh-user floor
///    (a bare machine with an empty map runs every seat on one model: works,
///    no diversity; the operator populates the map, or overrides per run,
///    for the real heterogeneous crew).
///
/// A role bound (by override OR map) to a profile absent from the registry is a
/// loud resolution error (`resolve_role_profile_with` owns that message; the
/// override case names the launch line, the map case names `config set`).
pub fn resolve_review_roles(
    reg: &ProfileRegistry,
    config: &crate::mission_config::MissionConfig,
    ov: &ReviewRoleStaffing,
    mapped: &dyn Fn(&str) -> RoleBinding,
) -> Result<ResolvedReviewRoles> {
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

    let mut judge_role: Option<String> = None;
    let mut verify_role: Option<String> = None;
    let mut probe_role_ids: Vec<String> = Vec::new();

    for task in config.phases.iter().flat_map(|p| p.tasks.iter()) {
        let Some(role_id) = &task.role_id else { continue };
        let is_judge = task.steps.iter().any(|s| s.kind == "review.judge");
        let is_verify = task.steps.iter().any(|s| s.kind == "review.verify-render");
        if is_judge {
            if judge_role.is_some() {
                bail!(
                    "darkmux: \"review\" mission config declares more than one task with a \
                     \"review.judge\" step — the judge stage is exactly one task (#1512)"
                );
            }
            judge_role = Some(role_id.clone());
        } else if is_verify {
            if verify_role.is_some() {
                bail!(
                    "darkmux: \"review\" mission config declares more than one task with a \
                     \"review.verify-render\" step — the verify stage is at most one task (#1512)"
                );
            }
            verify_role = Some(role_id.clone());
        } else {
            probe_role_ids.push(role_id.clone());
        }
    }

    if probe_role_ids.is_empty() {
        bail!(
            "darkmux: review resourcing: no probe roles to staff — \"review\" mission config \
             declares no role-bearing task outside the judge/verify stages (#1512)"
        );
    }
    let judge_role_id = judge_role.ok_or_else(|| {
        anyhow!(
            "darkmux: review resourcing: \"review\" mission config declares no judge task (a \
             task with a \"review.judge\" step) (#1512)"
        )
    })?;

    let mut probes = Vec::with_capacity(probe_role_ids.len());
    for role_id in &probe_role_ids {
        probes.push(resolve_task_role(reg, role_id, mapped, DEFAULT_JUDGE_PASSES)?);
    }
    let judge = resolve_task_role(reg, &judge_role_id, mapped, judge_passes)?;
    let verify = match verify_role {
        Some(role_id) => Some(resolve_task_role(reg, &role_id, mapped, 1)?),
        None => None,
    };

    Ok(ResolvedReviewRoles { probes, judge, verify, request_changes: ov.request_changes })
}

/// (#1475 packet 2, #1512, #1513 review) Resolve ONE task's role to a
/// concrete seat staffing via the role→profile binding — the SINGLE generic
/// per-task resolution primitive [`resolve_review_roles`] calls for every
/// role it classifies (probe, judge, or verify alike; no parallel
/// resolution path). A LOCAL seat's model must declare `n_ctx` (it's loaded
/// at that context) — a missing window is a named resolution error; a
/// REMOTE (endpoint-bearing) model needs none. The seat carries its
/// `role_id` + role→profile provenance so the envelope snapshot is
/// self-describing. `k` is always `1` (#1512 — see [`ResolvedSeatStaffing::k`]'s
/// doc); `passes` is the caller's job to pick per role (the judge's
/// consensus depth; `1` for probe/verify, which ignore it).
pub fn resolve_task_role(
    reg: &ProfileRegistry,
    role_id: &str,
    mapped: &dyn Fn(&str) -> RoleBinding,
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
        k: 1,
        passes,
        max_tokens: None,
        selector: None,
        provenance: Some(StaffingProvenance::role_profile(role_id, &resolved.profile_name, source)),
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
    fn map_of(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(r, p)| (r.to_string(), p.to_string())).collect()
    }

    /// Resolve a role against a fixed table into the `Mapped`/`Unmapped`
    /// [`RoleBinding`] the injected `mapped` closure yields (the `config.json`
    /// map tier). `Overridden` is exercised by its own test.
    fn map_binding(bindings: &std::collections::BTreeMap<String, String>, role: &str) -> RoleBinding {
        match bindings.get(role) {
            Some(p) => RoleBinding::Mapped(p.clone()),
            None => RoleBinding::Unmapped,
        }
    }

    /// A hand-built "review" mission config JSON declaring exactly `n`
    /// probe tasks (each its own `role_id`), one judge task (kind
    /// `"review.judge"`), and — when `verify` is `true` — one verify task
    /// (kind `"review.verify-render"`). Mirrors the real `review.json`'s
    /// structure without the prose; the SAME shape [`resolve_review_roles`]
    /// classifies structurally, by step kind, never by a fixed task id or a
    /// `depends_on` heuristic.
    fn review_config(n_probes: usize, verify: bool) -> crate::mission_config::MissionConfig {
        let probe_ids: Vec<String> = ["review-probe-high", "review-probe-mid", "review-probe-low"]
            .iter()
            .take(n_probes.min(3))
            .map(|s| s.to_string())
            .chain((3..n_probes).map(|i| format!("review-probe-{i}")))
            .collect();
        let probe_tasks: Vec<serde_json::Value> = probe_ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": format!("{id}-task"),
                    "role_id": id,
                    "depends_on": ["review-bundle-task"],
                    "steps": [{"id": format!("{id}-step"), "kind": "dispatch.map"}]
                })
            })
            .collect();
        let mut investigate_tasks = vec![serde_json::json!({
            "id": "review-bundle-task",
            "depends_on": [],
            "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]
        })];
        investigate_tasks.extend(probe_tasks);
        investigate_tasks.push(serde_json::json!({
            "id": "review-dedup-task",
            "depends_on": probe_ids.iter().map(|id| format!("{id}-task")).collect::<Vec<_>>(),
            "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]
        }));

        let mut phases = vec![
            serde_json::json!({"id": "investigate", "tasks": investigate_tasks}),
            serde_json::json!({"id": "adjudicate", "tasks": [
                {"id": "review-judge-task", "role_id": "review-judge", "depends_on": ["review-dedup-task"],
                 "steps": [{"id": "review-judge-step", "kind": "review.judge", "config": {"concurrency": 1}}]}
            ]}),
        ];
        let mut report_tasks = Vec::new();
        if verify {
            report_tasks.push(serde_json::json!({
                "id": "review-verify-task", "role_id": "review-verify", "depends_on": ["review-judge-task"],
                "steps": [
                    {"id": "review-verify-render-step", "kind": "review.verify-render"},
                    {"id": "review-verify-step", "kind": "dispatch.map"}
                ]
            }));
        }
        report_tasks.push(serde_json::json!({
            "id": "review-synthesis-task",
            "depends_on": ["review-dedup-task", "review-judge-task"],
            "steps": [{"id": "review-synthesis-step", "kind": "review.synthesis"}]
        }));
        phases.push(serde_json::json!({"id": "report", "tasks": report_tasks}));

        let doc = serde_json::json!({"id": "review", "name": "PR Review", "phases": phases});
        serde_json::from_value(doc).expect("hand-built review config parses")
    }

    /// (#1475 packet 2, headline; #1513 review) Five roles (3 probes, judge,
    /// verify) resolve to five bindings, a heterogeneous run. Distinct probe
    /// roles resolve to distinct profiles/models straight from the map — no
    /// roster scoring, no launch pins, and NO family grouping: the roles
    /// are classified purely from the config's own step kinds.
    #[test]
    fn resolve_review_roles_staffs_every_declared_role_from_the_map() {
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
        let config = review_config(3, true);
        let roles = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|r| {
            map_binding(&bindings, r)
        })
        .unwrap();

        assert_eq!(roles.probes.len(), 3, "one staffing per distinct probe role");
        assert_eq!(roles.probes[0].role_id.as_deref(), Some("review-probe-high"));
        assert_eq!(roles.probes[0].pm.id, "m27", "probe-high → p27 → m27");
        assert_eq!(roles.probes[1].pm.id, "mdev", "probe-mid → pdev → mdev");
        assert_eq!(roles.probes[2].pm.id, "m4b", "probe-low → p4b → m4b");
        assert_eq!(roles.probes[0].k, 1, "one draw per probe role — diversity is role-borne, not k-borne");
        assert_eq!(roles.judge.pm.id, "m35");
        assert_eq!(roles.verify.as_ref().unwrap().pm.id, "m35");
        // The run's display identity is the distinct profile set (heterogeneous here).
        assert_eq!(roles.distinct_profile_names(), "p27+p35+p4b+pdev");
        // Provenance records role→profile, not scored/pinned.
        let prov = roles.probes[0].provenance.as_ref().unwrap();
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
        let config = review_config(3, false);
        let roles = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|r| {
            // Precedence encoded exactly as the production closure does it.
            if let Some(p) = overrides.get(r) {
                RoleBinding::Overridden(p.clone())
            } else {
                map_binding(&map, r)
            }
        })
        .unwrap();
        assert_eq!(roles.judge.pm.id, "mdev", "override → pdev → mdev beats the map's p35");
        let prov = roles.judge.provenance.as_ref().unwrap();
        assert!(prov.detail.contains("launch override"), "provenance names the tier: {}", prov.detail);
        assert!(prov.detail.contains("pdev"), "names the overriding profile: {}", prov.detail);
    }

    /// (#1475 packet 2) An UNMAPPED role falls through to `default_profile` —
    /// the fresh-user floor. An empty map runs every seat on one model.
    #[test]
    fn unmapped_roles_fall_back_to_default_profile() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let config = review_config(3, true);
        let roles =
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
                .unwrap();
        for seat in roles.probes.iter().chain(std::iter::once(&roles.judge)).chain(roles.verify.iter()) {
            assert_eq!(seat.pm.id, "only", "every unmapped seat → default_profile's model");
            assert!(
                seat.provenance.as_ref().unwrap().detail.contains("default_profile fallback"),
                "provenance names the fallback"
            );
        }
        assert_eq!(roles.distinct_profile_names(), "solo", "homogeneous run reads as the one profile");
    }

    /// (#1475 packet 2) A role mapped to a profile absent from the registry is a
    /// loud resolution error (packet 1's message), never a silent fallback.
    #[test]
    fn role_mapped_to_missing_profile_errors_loudly() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let bindings = map_of(&[("review-judge", "ghost")]);
        let config = review_config(3, true);
        // `{:#}` renders the full anyhow chain — the seat context is the outer
        // layer, packet 1's missing-profile message the inner cause.
        let err = format!(
            "{:#}",
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|r| map_binding(&bindings, r))
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
        let config = review_config(3, true);
        let err = resolve_review_roles(&reg, &config, &ov, &|_| RoleBinding::Unmapped).unwrap_err().to_string();
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
        let config = review_config(3, true);
        let err = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
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
        let config = review_config(3, true);
        let roles = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|r| {
            map_binding(&bindings, r)
        })
        .unwrap();
        let verify = roles.verify.as_ref().unwrap();
        assert_eq!(verify.pm.id, "gpt");
        assert!(verify.pm.is_remote(), "a remote-bound verify seat needs no n_ctx");
    }

    // ─── (#1512) config-driven probe count — the payoff ──────────────────────

    /// The headline #1512 assertion: probe roles are discovered straight off
    /// the document's own declared tasks, in order — no Rust constant, no
    /// `depends_on` heuristic, just "every role-bearing task that isn't
    /// judge/verify."
    #[test]
    fn resolve_review_roles_discovers_probe_roles_from_the_document_in_order() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let config = review_config(3, false);
        let roles =
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
                .unwrap();
        let ids: Vec<&str> = roles.probes.iter().map(|s| s.role_id.as_deref().unwrap()).collect();
        assert_eq!(ids, vec!["review-probe-high", "review-probe-mid", "review-probe-low"]);
    }

    /// (#1512 payoff) A ONE-probe config — the Studio 32GB case named in
    /// the issue — resolves exactly one probe staffing. Changing the probe
    /// count is purely a document edit; this function's behavior doesn't
    /// change.
    #[test]
    fn resolve_review_roles_supports_a_one_probe_config() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let config = review_config(1, false);
        let roles =
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
                .unwrap();
        assert_eq!(roles.probes.len(), 1, "exactly one probe role staffed, matching what the document declares");
        assert_eq!(roles.probes[0].role_id.as_deref(), Some("review-probe-high"));
    }

    /// A five-probe config (the issue's other named example) resolves all
    /// five, still with zero Rust changes.
    #[test]
    fn resolve_review_roles_supports_a_five_probe_config() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let config = review_config(5, false);
        let roles =
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
                .unwrap();
        assert_eq!(roles.probes.len(), 5);
    }

    /// A config declaring no verify task (no `"review.verify-render"` step
    /// anywhere) resolves `verify: None` — structurally absent, never a
    /// resolution failure silently swallowed. This is how a config expresses
    /// "no verify stage," not a caught error.
    #[test]
    fn resolve_review_roles_verify_absent_from_the_document_resolves_to_none() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let config = review_config(3, false);
        let roles =
            resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
                .unwrap();
        assert!(roles.verify.is_none());
    }

    /// A config whose ONLY tasks are judge/verify (no probe task at all) is
    /// a loud error — a review with zero probes is not a degraded review,
    /// it's a misconfigured one.
    #[test]
    fn resolve_review_roles_with_no_probe_tasks_errors_loudly() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [
                {"id": "adjudicate", "tasks": [
                    {"id": "review-judge-task", "role_id": "review-judge", "depends_on": [],
                     "steps": [{"id": "review-judge-step", "kind": "review.judge"}]}
                ]}
            ]
        });
        let config: crate::mission_config::MissionConfig = serde_json::from_value(doc).unwrap();
        let err = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no probe roles"), "{err}");
    }

    /// A config with no judge task at all (no `"review.judge"` step
    /// anywhere) is a loud, named error — never a silent pass with no
    /// ruling stage.
    #[test]
    fn resolve_review_roles_with_no_judge_task_errors_loudly() {
        let reg = multi_reg(vec![("solo", vec![model("only", 32000)])], Some("solo"));
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [
                {"id": "investigate", "tasks": [
                    {"id": "review-bundle-task", "depends_on": [], "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]},
                    {"id": "review-probe-only-task", "role_id": "review-probe-only", "depends_on": ["review-bundle-task"],
                     "steps": [{"id": "review-probe-only-step", "kind": "dispatch.map"}]}
                ]}
            ]
        });
        let config: crate::mission_config::MissionConfig = serde_json::from_value(doc).unwrap();
        let err = resolve_review_roles(&reg, &config, &ReviewRoleStaffing::default(), &|_| RoleBinding::Unmapped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no judge task"), "{err}");
    }
}
