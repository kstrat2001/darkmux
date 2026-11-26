//! (#365) Profile-vs-loaded envelope check.
//!
//! `darkmux lab run` stamps each run's `manifest.json` with the
//! *requested* profile name (either `--profile` or the registry's
//! `default_profile`). The dispatch itself loads darkmux's own instance of
//! the selected model at the profile's declared `n_ctx` (#1135) and
//! addresses that `darkmux:` identifier on the wire (#2240), so the model
//! and window the run measures are the profile's, whatever else is
//! resident.
//!
//! What this check reports is what the dispatch's residency preflight
//! (`darkmux_crew::dispatch_internal::decide_preflight`, the one owner of the
//! rule) will do: load a new instance (the run's wall-clock includes a model load and is not
//! comparable to a warm run), or reuse a larger darkmux-owned resident (no
//! load, but the run measures that larger window). That matters to reproducibility (a notebook
//! comparing two runs should know one of them paid for a load).
//!
//! Per the operator-sovereignty doctrine: **warn, don't block**. We compare
//! the requested profile's declared model envelope against `lms ps` before
//! the dispatch and emit operator-facing notes; the dispatch proceeds
//! either way.

use darkmux_crew::dispatch_internal::{decide_preflight, PreflightDecision};
use darkmux_profiles::engine::server::EngineServer;
use darkmux_profiles::envelope::ctx_diverges;
use darkmux_types::{EndpointKind, LoadedModel, ManagedBackend, Profile, ProfileModel};

/// (#3678) What one lab run's dispatch makes resident, resolved before the
/// run by the resolution the dispatch itself runs ([`dispatch_plan`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct DispatchPlan {
    /// The model the dispatch runs on, and the role that selects it. `None`
    /// when no role or no selection resolved (a provider that names no single
    /// role, or a profile that selects nothing, which fails the dispatch
    /// itself): the profile's default model stands in, as before #3678.
    pub work: Option<(ProfileModel, String)>,
    /// The `internal.utility` binding, when the dispatch loads it on LM Studio.
    pub utility: Option<UtilityPlan>,
}

/// (#3678) The `internal.utility` binding: its id as written and its
/// declared window.
#[derive(Debug, Clone)]
pub(crate) struct UtilityPlan {
    pub id: String,
    pub n_ctx: Option<u32>,
}

/// (#3678) The plan for a lab dispatch of `role` on `profile_name`, from
/// crew's own resolvers: `resolve_in` with `--profile` given (so
/// `role_profiles` is never consulted and `mapped` is `None`) and the lab's
/// utility opt-in (`allow_utility_model: true`, as every lab provider sets
/// it), and `utility_target_in` for the binding.
pub(crate) fn dispatch_plan(
    registry: &darkmux_types::ProfileRegistry,
    role: Option<&darkmux_crew::types::Role>,
    profile_name: &str,
) -> DispatchPlan {
    let work = role.and_then(|r| {
        let t = darkmux_crew::target::resolve_in(registry, r, Some(profile_name), None, true).ok()?.target()?;
        Some((t.model, r.id.clone()))
    });
    // A binding whose endpoint does not resolve fails the dispatch, so
    // nothing loads for it; one on an unmanaged endpoint is no LM Studio
    // load. A managed one is loaded on LM Studio whatever its endpoint (the
    // crew's `prepare_compactor` takes the managed arm for a Flint or
    // Tensorfold binding too), so it is planned like any other.
    let utility = darkmux_crew::target::utility_target_in(registry)
        .ok()
        .flatten()
        .filter(|u| u.kind.is_managed())
        .map(|u| UtilityPlan { id: u.id, n_ctx: u.n_ctx });
    DispatchPlan { work, utility }
}

/// (#3678) One model the dispatch makes resident, at the window it ends
/// resident at, and the part it plays.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub model: ProfileModel,
    pub part: Part,
}

/// The part a planned model plays in the dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    /// The work model, the profile's default.
    Default,
    /// The work model, a non-default one the role (named) selects.
    Selected(String),
    /// The utility model, in an instance of its own.
    Utility,
    /// The work model is the utility model: one instance serves both.
    Shared,
}

impl Part {
    /// The note's subject, ahead of its window.
    fn who(&self, id: &str) -> String {
        match self {
            Part::Default => format!("declares default model `{id}`"),
            Part::Selected(role) => format!("declares model `{id}` for role `{role}`"),
            Part::Utility => format!("runs utility model `{id}` (`internal.utility`)"),
            Part::Shared => format!("runs model `{id}` as both work and utility model"),
        }
    }
}

/// (#3678) The models `plan`'s dispatch makes resident from `profile`: the
/// work model, then the utility model at the window the dispatch loads it
/// at, which is the binding's, else the work model's
/// (`resolve_compactor_load_window`). With neither, compaction is refused
/// and the utility model is not loaded.
///
/// When the two are one LM Studio instance, the dispatch's work preflight
/// leaves it resident at no less than the work model's window, and its
/// compactor preflight then reuses it when that is at least the binding's,
/// else reloads it there (`decide_preflight`, sufficient means `>=`). So it
/// ends resident at no less than the larger window, and a load happens
/// exactly when the instance before the run is below that larger window:
/// one judgment at the larger window is the two preflights' answer.
pub(crate) fn planned_models(profile: &Profile, plan: &DispatchPlan) -> Vec<Planned> {
    let work = work_model(profile, plan);
    let Some(u) = &plan.utility else { return work.into_iter().collect() };
    let window = u.n_ctx.or(work.as_ref().and_then(|w| w.model.n_ctx));
    match work {
        Some(w) if shares_instance(&w.model, &u.id) => {
            let n_ctx = w.model.n_ctx.max(window);
            vec![Planned { model: ProfileModel { n_ctx, ..w.model }, part: Part::Shared }]
        }
        work => work.into_iter().chain(window.map(|n| utility_model(&u.id, n))).collect(),
    }
}

/// The work model: the plan's selection, else the profile's default.
fn work_model(profile: &Profile, plan: &DispatchPlan) -> Option<Planned> {
    let default_id = profile.default_model_id();
    let Some((model, role)) = &plan.work else {
        let model = profile.models.iter().find(|m| Some(m.id.as_str()) == default_id)?;
        return Some(Planned { model: model.clone(), part: Part::Default });
    };
    let part = if Some(model.id.as_str()) == default_id { Part::Default } else { Part::Selected(role.clone()) };
    Some(Planned { model: model.clone(), part })
}

/// The utility model as the dispatch loads it: its id at `n_ctx`, no
/// identifier opt-out (crew's `utility_residency_pm`).
fn utility_model(id: &str, n_ctx: u32) -> Planned {
    let model = ProfileModel { id: id.to_string(), n_ctx: Some(n_ctx), ..Default::default() };
    Planned { model, part: Part::Utility }
}

/// Whether the work model's LM Studio instance is the utility model's: the
/// same bare key (`names_utility_model`) addressed by the same identifier
/// (the compactor's is always `darkmux:<key>`, `compactor_wire_model_id`).
fn shares_instance(work: &ProfileModel, utility_id: &str) -> bool {
    let bare = darkmux_gestalt::bare_model_key;
    let identifier = |key, explicit| darkmux_gestalt::namespaced_identifier(bare(key), explicit);
    on_lm_studio(work)
        && darkmux_crew::select::names_utility_model(&work.id, utility_id)
        && identifier(&work.id, work.identifier.as_deref()) == identifier(utility_id, None)
}

/// Whether darkmux loads `pm` on LM Studio (managed, and not on an engine
/// darkmux starts servers for).
pub(crate) fn on_lm_studio(pm: &ProfileModel) -> bool {
    pm.is_managed() && !pm.endpoint_kind().is_ok_and(|k| k.starts_servers())
}

/// Compare the models the run's dispatch makes resident ([`planned_models`])
/// against what is resident now, returning operator-facing warning lines
/// (empty when everything lines up — or when `loaded` is empty and we
/// can't tell). Pure: the caller owns the best-effort `lms ps` query and
/// the printing.
///
/// One line per preflight outcome worth knowing: a load inside the run (work
/// model absent, resident undersized, or only a foreign copy resident), or a
/// reuse of a materially larger darkmux-owned resident.
pub(crate) fn envelope_warnings(
    planned: &[Planned],
    profile_name: &str,
    loaded: &[LoadedModel],
    servers: &[EngineServer],
    recs_own: Option<&str>,
) -> Vec<String> {
    planned
        .iter()
        // (#1282, #2902) Only a model darkmux manages has a loaded LM Studio
        // envelope to validate.
        .filter(|p| p.model.is_managed())
        .filter_map(|p| {
            // (#3450) A model on an endpoint darkmux starts servers for is
            // resident as darkmux's server there, never as an LM Studio load.
            // Only the work model can be one ([`dispatch_plan`]).
            if !on_lm_studio(&p.model) {
                return server_note(&p.model, &p.part.who(&p.model.id), servers, recs_own);
            }
            lms_note(&p.model, &p.part, loaded)
        })
        .map(|n| format!("requested profile `{profile_name}` {n}"))
        .collect()
}

/// The note for an LM Studio model, from what the dispatch's residency
/// preflight will do with `loaded`.
fn lms_note(pm: &ProfileModel, part: &Part, loaded: &[LoadedModel]) -> Option<String> {
    // Empty means either nothing is loaded or `lms ps` couldn't be
    // queried — in both cases we can't validate, so stay quiet rather
    // than crying "primary not loaded" against a set we don't trust.
    if loaded.is_empty() {
        return None;
    }
    // (F1, 5.0 dogfood) The dispatch preflight's own decision function
    // owns the reuse/reload/load rule; this check reports it (#2985).
    let declared = pm.n_ctx.map(u64::from);
    let who = part.who(&pm.id);
    match decide_preflight(&pm.id, pm.identifier.as_deref(), pm.n_ctx.unwrap_or(0), loaded) {
        // Every planned model is loaded before the run's timed section ends,
        // the utility model included (`prepare_compactor`), so an absent one
        // is a load inside the measurement.
        PreflightDecision::LoadFresh { foreign_identifier: None } => Some(format!(
            "{who} (ctx {}) but it is not among the currently loaded models: the dispatch loads a \
             new instance, so this run's wall-clock includes a model load and is not comparable \
             to a warm run.",
            declared.map_or_else(|| "unset".to_string(), |v| v.to_string()),
        )),
        PreflightDecision::Reuse { resident_ctx, .. } => declared.filter(|d| ctx_diverges(*d, resident_ctx)).map(|d| {
            format!(
                "{who} at {d} ctx; the dispatch reuses the resident instance at {resident_ctx} \
                 (larger than the declared {d}), so the run measures the larger window and pays \
                 no model load.",
            )
        }),
        // Without a declared window there is nothing to compare; an
        // unreported resident ctx (0) is insufficient, so it reloads.
        PreflightDecision::Reload { stale_ctx, .. } => declared.map(|d| {
            format!(
                "{who} at {d} ctx but the resident instance is at {stale_ctx}: the dispatch \
                 unloads it and loads a new instance at the declared window, so this run's \
                 wall-clock includes a model load.",
            )
        }),
        PreflightDecision::LoadFresh { foreign_identifier: Some(foreign_identifier) } => Some(format!(
            "model `{}` is resident only as `{foreign_identifier}`, which darkmux never reuses: \
             the dispatch loads a new instance beside it, so this run's wall-clock includes \
             a model load.",
            pm.id,
        )),
    }
}

/// (#3450) The note for a work model on an engine server: none when
/// darkmux's server on its endpoint runs it at its declared window. `who`
/// is the note's subject ([`Part::who`]).
/// (#3574) With rec's own server up (at `recs_own`, from `rec status`), a
/// Flint model's dispatch in a lab run is refused, which is the note.
fn server_note(pm: &ProfileModel, who: &str, servers: &[EngineServer], recs_own: Option<&str>) -> Option<String> {
    let endpoint = pm.endpoint.as_ref().and_then(|e| e.named_id()).unwrap_or_default();
    if let Some(address) = recs_own_up(pm, servers, recs_own) {
        return Some(format!(
            "{who} on Flint endpoint `{endpoint}`, where rec's own server is up (at \
             {address}): {}, and darkmux starts no server beside rec's, so the dispatch is refused \
             (`engine_in_use`). Run `rec stop` first.",
            darkmux_profiles::engine::flint::LAB_NEVER_REUSES,
        ));
    }
    let declared = pm.n_ctx.map_or_else(|| "unset".to_string(), |v| v.to_string());
    match servers.iter().find(|s| s.endpoint == endpoint && s.model == pm.id) {
        Some(s) if pm.n_ctx == Some(s.n_ctx) => None,
        Some(s) => Some(format!(
            "{who} at {declared} ctx; darkmux's server on endpoint `{endpoint}` runs it at \
             ctx {}: the dispatch restarts it at the declared window, so this run's wall-clock includes a model \
             start and is not comparable to a warm run.",
            s.n_ctx,
        )),
        None => Some(format!(
            "{who} (ctx {declared}) on endpoint `{endpoint}`, where no server of darkmux's \
             runs it: the dispatch starts its server, so this run's wall-clock includes a model start and is not \
             comparable to a warm run.",
        )),
    }
}

/// The address of rec's own server when it is up beside a Flint model:
/// `None` for any other model, or when `rec status` names darkmux's own
/// server's port.
fn recs_own_up<'a>(pm: &ProfileModel, servers: &[EngineServer], recs_own: Option<&'a str>) -> Option<&'a str> {
    let flint = pm.endpoint_kind().is_ok_and(|k| k == EndpointKind::Managed(ManagedBackend::Flint));
    let address = recs_own.filter(|_| flint)?;
    let port = darkmux_profiles::engine::flint::endpoint_port(address);
    (!servers.iter().any(|s| Some(s.port) == port)).then_some(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::ProfileModel;

    /// The warnings for `plan`'s dispatch on `profile`.
    fn planned_envelope_warnings(
        profile: &Profile,
        profile_name: &str,
        plan: &DispatchPlan,
        loaded: &[LoadedModel],
        servers: &[EngineServer],
        recs_own: Option<&str>,
    ) -> Vec<String> {
        super::envelope_warnings(&planned_models(profile, plan), profile_name, loaded, servers, recs_own)
    }

    /// The warnings with no role resolved and no utility binding: the
    /// profile's default model alone.
    fn envelope_warnings(
        profile: &Profile,
        profile_name: &str,
        loaded: &[LoadedModel],
        servers: &[EngineServer],
        recs_own: Option<&str>,
    ) -> Vec<String> {
        planned_envelope_warnings(profile, profile_name, &DispatchPlan::default(), loaded, servers, recs_own)
    }

    fn pm(id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel { max_output_tokens: None, engine_args: Vec::new(),
            endpoint: None,
            extras: Default::default(),
            id: id.to_string(),
            n_ctx: Some(n_ctx),
            capabilities: Default::default(),
            identifier: None,
        }
    }

    fn lm(identifier: &str, model: &str, context: u64) -> LoadedModel {
        LoadedModel {
            identifier: identifier.to_string(),
            model: model.to_string(),
            status: "idle".to_string(),
            size: "1.00 GB".to_string(),
            context,
            queued: None,
        }
    }

    fn profile(models: Vec<ProfileModel>) -> Profile {
        Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models,
            runtime: None,
            use_when: None,
        }
    }

    // matches_on_* unit tests moved to darkmux-profiles::envelope (the
    // matcher's new home, #544). The envelope_warnings tests below
    // exercise the shared matcher transitively.

    #[test]
    fn no_warning_when_envelope_aligns() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded, &[], None).is_empty());
    }

    #[test]
    fn warns_on_context_envelope_mismatch() {
        // The Beat-39 case: profile=deep declares 262K, balanced is loaded @101K.
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 101000)];
        let w = envelope_warnings(&p, "deep", &loaded, &[], None);
        assert_eq!(w.len(), 1, "expected one mismatch warning, got: {w:?}");
        assert!(w[0].contains("262000") && w[0].contains("101000"));
        assert!(w[0].contains("deep"));
    }

    /// (F1, 5.0 dogfood) A darkmux-owned resident LARGER than the declared
    /// window is reused by the residency planner, not reloaded: the warning
    /// must say so instead of claiming a model load.
    #[test]
    fn larger_resident_is_reported_as_reuse_not_a_load() {
        let p = profile(vec![pm("qwen-35b", 32768)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262144)];
        let w = envelope_warnings(&p, "small", &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("reuses the resident instance at 262144"), "{}", w[0]);
        assert!(w[0].contains("larger than the declared 32768"), "{}", w[0]);
        assert!(!w[0].contains("includes a model load"), "{}", w[0]);
    }

    /// An undersized darkmux-owned resident is reloaded (the planner's
    /// Reconcile arm), and the message says a new instance loads.
    #[test]
    fn undersized_resident_is_reported_as_a_load() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 101000)];
        let w = envelope_warnings(&p, "deep", &loaded, &[], None);
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    /// A user-loaded copy is never reused (#1274), so darkmux loads its own
    /// beside it, and that is a load inside the run.
    #[test]
    fn foreign_resident_is_never_reused() {
        let p = profile(vec![pm("qwen-35b", 32768)]);
        let loaded = vec![lm("qwen-35b", "qwen-35b", 262144)];
        let w = envelope_warnings(&p, "small", &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    #[test]
    fn warns_when_primary_not_loaded() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        // A wholly different model is loaded.
        let loaded = vec![lm("darkmux:other-model", "other-model", 32000)];
        let w = envelope_warnings(&p, "deep", &loaded, &[], None);
        assert_eq!(w.len(), 1, "expected not-loaded warning, got: {w:?}");
        assert!(w[0].contains("not among the currently loaded"));
    }

    /// (#3678) A plan whose work model is `work` (selected for role `coder`)
    /// and whose utility binding is `utility` at its declared window.
    fn plan(work: &ProfileModel, utility: Option<(&str, Option<u32>)>) -> DispatchPlan {
        DispatchPlan {
            work: Some((work.clone(), "coder".to_string())),
            utility: utility.map(|(id, n_ctx)| UtilityPlan { id: id.to_string(), n_ctx }),
        }
    }

    /// (#3678) The 5.12.0 dogfood case: the utility model is listed in the
    /// profile at 120000 but bound at 68000 and resident at 68000, and the
    /// default model is resident at its window. Nothing loads, so no note.
    /// A binding above the resident's window does reload it.
    #[test]
    fn dogfood_utility_resident_at_its_bindings_window_is_quiet() {
        let p = profile(vec![pm("qwen-35b", 262000), pm("qwen-4b", 120000)]);
        let loaded = [lm("darkmux:qwen-35b", "qwen-35b", 262000), lm("darkmux:qwen-4b", "qwen-4b", 68000)];
        let w = planned_envelope_warnings(&p, "deep", &plan(&p.models[0], Some(("qwen-4b", Some(68000)))), &loaded, &[], None);
        assert!(w.is_empty(), "{w:?}");
        let tighter = plan(&p.models[0], Some(("qwen-4b", Some(120000))));
        let w = planned_envelope_warnings(&p, "deep", &tighter, &loaded, &[], None);
        assert_eq!(w.len(), 1, "a binding above the resident's window still reloads it: {w:?}");
        assert!(w[0].contains("utility model `qwen-4b`"), "{}", w[0]);
        assert!(w[0].contains("at 120000 ctx but the resident instance is at 68000"), "{}", w[0]);
    }

    /// (#3678 R1) When the utility model is also the work model (the lab's
    /// `allow_utility_model`), the dispatch makes it resident at the work
    /// model's window AND at the binding's, so it ends at least the larger:
    /// a resident at the binding's smaller window is reloaded.
    #[test]
    fn a_utility_model_that_is_the_work_model_is_judged_at_the_larger_window() {
        let p = profile(vec![pm("qwen-4b", 120000)]);
        let small = [lm("darkmux:qwen-4b", "qwen-4b", 68000)];
        let w = planned_envelope_warnings(&p, "util", &plan(&p.models[0], Some(("qwen-4b", Some(68000)))), &small, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("as both work and utility model"), "{}", w[0]);
        assert!(w[0].contains("at 120000 ctx but the resident instance is at 68000"), "{}", w[0]);
        // Resident at the larger window: both preflights reuse it.
        let large = [lm("darkmux:qwen-4b", "qwen-4b", 120000)];
        let w = planned_envelope_warnings(&p, "util", &plan(&p.models[0], Some(("qwen-4b", Some(68000)))), &large, &[], None);
        assert!(w.is_empty(), "{w:?}");
    }

    /// (#3678 R2) `internal.utility` may be written `darkmux:<key>`; the
    /// dispatch compares bare keys, and so does the check.
    #[test]
    fn a_namespaced_utility_binding_names_its_bare_model() {
        let p = profile(vec![pm("qwen-35b", 262000), pm("qwen-4b", 120000)]);
        let loaded = [lm("darkmux:qwen-35b", "qwen-35b", 262000), lm("darkmux:qwen-4b", "qwen-4b", 68000)];
        let bound = plan(&p.models[0], Some(("darkmux:qwen-4b", Some(68000))));
        let w = planned_envelope_warnings(&p, "deep", &bound, &loaded, &[], None);
        assert!(w.is_empty(), "the binding's window, not the profile's listing: {w:?}");
        // The same spelling naming the work model: one instance, larger window.
        let p = profile(vec![pm("qwen-4b", 68000)]);
        let loaded = [lm("darkmux:qwen-4b", "qwen-4b", 68000)];
        let shared = plan(&p.models[0], Some(("darkmux:qwen-4b", Some(120000))));
        let w = planned_envelope_warnings(&p, "util", &shared, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("as both work and utility model"), "{}", w[0]);
        assert!(w[0].contains("at 120000 ctx but the resident instance is at 68000"), "{}", w[0]);
    }

    /// (#3678) A work model with its own `identifier` opt-out is an instance
    /// apart from the utility model's `darkmux:<key>`, even on the same key:
    /// each is judged on its own, so the utility instance's reload is noted
    /// although the work instance is large enough for both windows.
    #[test]
    fn an_identifier_opt_out_work_model_is_not_the_utility_instance() {
        let mut work = pm("qwen-4b", 120000);
        work.identifier = Some("my-qwen".into());
        let p = profile(vec![work]);
        let mut loaded = vec![lm("my-qwen", "qwen-4b", 120000), lm("darkmux:qwen-4b", "qwen-4b", 32000)];
        let bound = plan(&p.models[0], Some(("qwen-4b", Some(68000))));
        let w = planned_envelope_warnings(&p, "util", &bound, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("utility model `qwen-4b`") && w[0].contains("resident instance is at 32000"), "{}", w[0]);
        loaded[1].context = 68000;
        assert!(planned_envelope_warnings(&p, "util", &bound, &loaded, &[], None).is_empty());
    }

    /// (#3678) Only a binding the dispatch loads on LM Studio is planned: one
    /// on an unmanaged endpoint loads nothing.
    #[test]
    fn a_utility_binding_on_an_unmanaged_endpoint_is_not_planned() {
        let registry = |endpoint: serde_json::Value| -> darkmux_types::ProfileRegistry {
            let mut r: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
                "default_profile": "p",
                "profiles": {"p": {"models": [{"id": "qwen-35b", "n_ctx": 262000}]}},
                "endpoints": {"hosted": {"url": "https://example.com/v1"}},
                "internal": {"utility": {"id": "qwen-4b", "n_ctx": 68000, "endpoint": endpoint}}
            }))
            .unwrap();
            r.materialize_endpoints();
            r
        };
        let hosted = dispatch_plan(&registry(serde_json::json!("hosted")), None, "p");
        assert!(hosted.utility.is_none(), "{hosted:?}");
        let local = dispatch_plan(&registry(serde_json::Value::Null), None, "p");
        assert_eq!(local.utility.map(|u| (u.id, u.n_ctx)), Some(("qwen-4b".to_string(), Some(68000))));
    }

    /// (#3678 review T2) A binding on a managed endpoint darkmux starts a
    /// server for (Flint here) is still loaded on LM Studio by the crew's
    /// compactor setup, so it is planned.
    #[test]
    fn a_utility_binding_on_a_server_endpoint_is_still_planned() {
        let mut r: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "default_profile": "p",
            "profiles": {"p": {"models": [{"id": "qwen-35b", "n_ctx": 262000}]}},
            "endpoints": {"fl": {"managed": "flint", "port": 8092}},
            "internal": {"utility": {"id": "qwen-4b", "n_ctx": 68000, "endpoint": "fl"}}
        }))
        .unwrap();
        r.materialize_endpoints();
        let plan = dispatch_plan(&r, None, "p");
        assert_eq!(plan.utility.map(|u| (u.id, u.n_ctx)), Some(("qwen-4b".to_string(), Some(68000))));
    }

    /// (#3678 R3) A binding with no declared window is loaded at the
    /// SELECTED model's window, not the default model's.
    #[test]
    fn a_windowless_binding_takes_the_selected_models_window() {
        let p = profile(vec![pm("qwen-35b", 262000), pm("coder-30b", 120000), pm("qwen-4b", 68000)]);
        let mut loaded = vec![
            lm("darkmux:qwen-35b", "qwen-35b", 262000),
            lm("darkmux:coder-30b", "coder-30b", 120000),
            lm("darkmux:qwen-4b", "qwen-4b", 120000),
        ];
        let windowless = plan(&p.models[1], Some(("qwen-4b", None)));
        let w = planned_envelope_warnings(&p, "deep", &windowless, &loaded, &[], None);
        assert!(w.is_empty(), "{w:?}");
        loaded[2].context = 68000;
        let w = planned_envelope_warnings(&p, "deep", &windowless, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`qwen-4b`") && w[0].contains("at 120000 ctx but the resident instance is at 68000"), "{}", w[0]);
    }

    /// (#3678) The utility model is loaded whether or not the profile lists
    /// it, so an undersized resident is a reload and a missing one is a load,
    /// both inside the run's measurement (#3678 review T1).
    #[test]
    fn an_unlisted_utility_model_is_judged_too() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let mut loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000), lm("darkmux:qwen-4b", "qwen-4b", 32000)];
        let bound = plan(&p.models[0], Some(("qwen-4b", Some(68000))));
        let w = planned_envelope_warnings(&p, "deep", &bound, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("utility model `qwen-4b`") && w[0].contains("loads a new instance"), "{}", w[0]);
        loaded.pop();
        let w = planned_envelope_warnings(&p, "deep", &bound, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("utility model `qwen-4b`") && w[0].contains("not among the currently loaded"), "{}", w[0]);
    }

    /// (#3678) Capability scoring can select a non-default model. The check
    /// asks the dispatch's own resolution (`resolve_in`, through
    /// [`dispatch_plan`]), so the selected model's load is noted and the
    /// unselected default's undersized resident, which the dispatch never
    /// touches, is not.
    #[test]
    #[serial_test::serial]
    fn a_capability_selected_model_is_the_one_judged() {
        let _state = darkmux_types::test_isolation::IsolatedState::new();
        let all = |v: f32| serde_json::json!({"code": v, "agentic_tool_use": v, "reasoning": v, "instruction_following": v});
        let mut registry: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "default_profile": "p",
            "profiles": {"p": {"models": [
                {"id": "qwen-35b", "n_ctx": 262000, "capabilities": all(0.1)},
                {"id": "coder-30b", "n_ctx": 120000, "capabilities": all(1.0)}
            ]}}
        }))
        .unwrap();
        registry.materialize_endpoints();
        let role = darkmux_crew::dispatch::lab_role(&darkmux_crew::dispatch::LabInputs::Builtin, "coder").unwrap();
        let plan = dispatch_plan(&registry, Some(&role), "p");
        assert_eq!(plan.work.as_ref().map(|(m, _)| m.id.as_str()), Some("coder-30b"), "{plan:?}");
        let p = &registry.profiles["p"];
        let loaded = [lm("darkmux:qwen-35b", "qwen-35b", 101000)];
        let w = planned_envelope_warnings(p, "p", &plan, &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`coder-30b` for role `coder`") && w[0].contains("not among the currently loaded"), "{}", w[0]);
    }

    /// (#3678) No stray " : " in a note.
    #[test]
    fn notes_join_their_clauses_with_a_plain_colon() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        for loaded in [vec![lm("darkmux:qwen-35b", "qwen-35b", 101000)], vec![lm("qwen-35b", "qwen-35b", 262000)]] {
            let w = envelope_warnings(&p, "deep", &loaded, &[], None);
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(!w[0].contains(" : "), "{}", w[0]);
        }
    }

    #[test]
    fn missing_non_default_model_does_not_warn() {
        // (#3678) A missing second model the run's dispatch does not select
        // is not in the plan, so it gets no note. (The compactor moved to the
        // registry's internal.utility binding and can no longer live in
        // `models[]`.)
        let p = profile(vec![pm("qwen-35b", 262000), pm("qwen-4b", 68000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded, &[], None).is_empty());
    }

    #[test]
    fn tolerates_rounding_within_5_percent() {
        // 262000 declared vs 262144 loaded (power-of-two) — benign.
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262144)];
        assert!(envelope_warnings(&p, "deep", &loaded, &[], None).is_empty());
    }

    /// (#2985) A resident that reports no context (0) is insufficient to the
    /// dispatch preflight, which reloads it, so the warning says a load happens.
    #[test]
    fn unreported_loaded_context_is_reported_as_a_reload() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 0)];
        let w = envelope_warnings(&p, "deep", &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    /// (#2985) The warning is the preflight's decision, not a second rule: an
    /// operator `identifier` opt-out is reused only under that exact string.
    #[test]
    fn identifier_opt_out_with_default_namespaced_resident_is_a_load() {
        let mut m = pm("qwen-35b", 32768);
        m.identifier = Some("my-qwen".into());
        let p = profile(vec![m]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262144)];
        let w = envelope_warnings(&p, "small", &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    /// (#2985) A different `darkmux:*` instance of the same weights cannot
    /// answer the wire `model` (#2240), so the preflight loads its own.
    #[test]
    fn differently_named_darkmux_instance_is_a_load() {
        let p = profile(vec![pm("qwen-35b", 32768)]);
        let loaded = vec![lm("darkmux:qwen-35b-alt", "qwen-35b", 262144)];
        let w = envelope_warnings(&p, "small", &loaded, &[], None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    /// (#2985) A profile model id spelled with the namespace prefix is
    /// normalized to its bare key by the preflight, so the resident is found.
    #[test]
    fn namespaced_profile_id_finds_its_resident() {
        let p = profile(vec![pm("darkmux:qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded, &[], None).is_empty());
    }

    #[test]
    fn empty_loaded_set_yields_no_warnings() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        assert!(envelope_warnings(&p, "deep", &[], &[], None).is_empty());
    }

    /// (#1282) An endpoint-bearing model (no `n_ctx`, served remotely) has no
    /// local envelope to validate — no warning even though it will never be
    /// among the loaded models.
    #[test]
    fn remote_endpoint_model_is_skipped() {
        let mut remote = pm("gpt-4o", 0);
        remote.n_ctx = None;
        remote.endpoint = Some(darkmux_types::ModelEndpoint {
            url: Some("https://example.azure.com/openai".into()),
            ..Default::default()
        });
        let p = profile(vec![remote]);
        let loaded = vec![lm("darkmux:other-model", "other-model", 32000)];
        assert!(envelope_warnings(&p, "azure", &loaded, &[], None).is_empty());
    }

    fn on_tensorfold(endpoint: &str, id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel {
            endpoint: Some(darkmux_types::ModelEndpoint {
                managed: Some(darkmux_types::ManagedBackend::Tensorfold.into()),
                port: Some(8091),
                source: darkmux_types::EndpointSource::Named(endpoint.to_string()),
                ..Default::default()
            }),
            ..pm(id, n_ctx)
        }
    }

    fn served(endpoint: &str, model: &str, n_ctx: u32) -> EngineServer {
        EngineServer {
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: model.into(),
            served_name: format!("darkmux:{model}"),
            n_ctx,
            port: 8091,
            pid: 4242,
            started_at_ms: 0,
            answering: true,
        }
    }

    /// (#3450) A Tensorfold model's residency is darkmux's server on its
    /// endpoint, read from its record, never from LM Studio: a run whose
    /// default model's server is not running it (or runs it at another
    /// context) starts it inside the run and says so; one already served
    /// at its context says nothing, even when LM Studio lists nothing.
    #[test]
    fn a_tensorfold_default_model_is_checked_against_its_server() {
        let p = profile(vec![on_tensorfold("tf", "TensorFold/q38", 262_144)]);
        let warm = envelope_warnings(&p, "tf", &[], &[served("tf", "TensorFold/q38", 262_144)], None);
        assert!(warm.is_empty(), "{warm:?}");
        let cold = envelope_warnings(&p, "tf", &[], &[], None);
        assert_eq!(cold.len(), 1, "{cold:?}");
        assert!(cold[0].contains("starts its server") && cold[0].contains("endpoint `tf`"), "{}", cold[0]);
        let resized = envelope_warnings(&p, "tf", &[], &[served("tf", "TensorFold/q38", 8192)], None);
        assert!(resized[0].contains("at ctx 8192") && resized[0].contains("restarts it"), "{resized:?}");
        // An LM Studio load of the same id is not this model's residency.
        let lms = vec![lm("darkmux:TensorFold/q38", "TensorFold/q38", 262_144)];
        assert_eq!(envelope_warnings(&p, "tf", &lms, &[], None).len(), 1);
    }

    /// A Flint model on endpoint `fl`.
    fn on_flint(id: &str, n_ctx: u32) -> ProfileModel {
        let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "profiles": {"p": {"models": [{"id": id, "n_ctx": n_ctx, "endpoint": "fl"}]}},
            "endpoints": {"fl": {"managed": "flint", "port": 8092}}
        }))
        .unwrap();
        reg.materialize_endpoints();
        reg.profiles["p"].models[0].clone()
    }

    /// (#3574) With rec's own server up, a lab run on a Flint default model
    /// is refused (a lab run never uses a server darkmux did not start, and
    /// darkmux starts none beside rec's): the note says so and names
    /// `rec stop`, never a model start. A `rec status` naming darkmux's own
    /// server's port is darkmux's server, and the usual note stands.
    #[test]
    fn a_flint_default_model_with_recs_own_server_up_is_refused_not_started() {
        let p = profile(vec![on_flint("flint-int4", 8192)]);
        let up = envelope_warnings(&p, "fl", &[], &[], Some("http://127.0.0.1:11500/v1"));
        assert_eq!(up.len(), 1, "{up:?}");
        assert!(up[0].contains("rec's own server is up") && up[0].contains("rec stop"), "{}", up[0]);
        assert!(up[0].contains("refused") && !up[0].contains("starts its server"), "{}", up[0]);
        let mut ours = served("fl", "flint-int4", 8192);
        ours.port = 11500;
        let darkmuxs = envelope_warnings(&p, "fl", &[], &[ours], Some("http://127.0.0.1:11500/v1"));
        assert!(darkmuxs.is_empty(), "{darkmuxs:?}");
        // A Tensorfold model is not Flint: rec's server has no bearing.
        let tf = profile(vec![on_tensorfold("tf", "TensorFold/q38", 262_144)]);
        let cold = envelope_warnings(&tf, "tf", &[], &[], Some("http://127.0.0.1:11500/v1"));
        assert!(cold[0].contains("starts its server"), "{cold:?}");
    }
}
