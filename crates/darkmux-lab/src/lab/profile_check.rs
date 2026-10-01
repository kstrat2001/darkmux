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
//! What this check reports is what the residency planner
//! (`darkmux_gestalt::decide_residency`, the one owner of the rule) will do:
//! load a new instance (the run's wall-clock includes a model load and is not
//! comparable to a warm run), or reuse a larger darkmux-owned resident (no
//! load, but the run measures that larger window). That matters to reproducibility (a notebook
//! comparing two runs should know one of them paid for a load).
//!
//! Per the operator-sovereignty doctrine: **warn, don't block**. We compare
//! the requested profile's declared model envelope against `lms ps` before
//! the dispatch and emit operator-facing notes; the dispatch proceeds
//! either way.

use darkmux_gestalt::{decide_residency, Placement, ResidencyDecision, ResidentFact};
use darkmux_profiles::envelope::ctx_diverges;
use darkmux_profiles::ownership::namespaced_identifier;
use darkmux_types::{LoadedModel, Profile};

/// Compare the requested profile's declared model envelope against what
/// LMStudio actually has loaded, returning operator-facing warning lines
/// (empty when everything lines up — or when `loaded` is empty and we
/// can't tell). Pure: the caller owns the best-effort `lms ps` query and
/// the printing.
///
/// One line per planner outcome worth knowing: a load inside the run (default
/// model absent, resident undersized, or only a foreign copy resident), or a
/// reuse of a materially larger darkmux-owned resident.
pub(crate) fn envelope_warnings(
    profile: &Profile,
    profile_name: &str,
    loaded: &[LoadedModel],
) -> Vec<String> {
    let mut out = Vec::new();
    // Empty means either nothing is loaded or `lms ps` couldn't be
    // queried — in both cases we can't validate, so stay quiet rather
    // than crying "primary not loaded" against a set we don't trust.
    if loaded.is_empty() {
        return out;
    }
    // (#590) Only the default model (default_model, or first model) is
    // load-bearing for the measurement envelope.
    let default_id = profile.default_model_id();
    let residents = resident_facts(loaded);
    for pm in &profile.models {
        // (#1282, #2902) Only a model darkmux manages has a loaded LM Studio
        // envelope to validate.
        if !pm.is_managed() {
            continue;
        }
        // (F1, 5.0 dogfood) The residency planner owns the reuse/reload/load
        // rule; this check asks it rather than keeping a second copy.
        let placement = Placement {
            model_key: pm.id.clone(),
            identifier: namespaced_identifier(pm),
            min_ctx: pm.n_ctx.unwrap_or(0),
            seat: "lab-envelope-check".to_string(),
        };
        let declared = pm.n_ctx.map(u64::from);
        let note = match decide_residency(&residents, &placement) {
            // A missing non-default model is common and not worth the noise.
            ResidencyDecision::LoadFresh if Some(pm.id.as_str()) == default_id => Some(format!(
                "declares default model `{}` (ctx {}) but it is not among the currently loaded \
                 models — the dispatch loads a new instance, so this run's wall-clock includes a \
                 model load and is not comparable to a warm run.",
                pm.id,
                declared.map_or_else(|| "unset".to_string(), |v| v.to_string()),
            )),
            ResidencyDecision::LoadFresh => None,
            ResidencyDecision::Reuse { resident_ctx, .. } => declared
                .filter(|d| ctx_diverges(*d, resident_ctx))
                .map(|d| {
                    format!(
                        "declares model `{}` at {d} ctx; the dispatch reuses the resident instance \
                         at {resident_ctx} (larger than the declared {d}), so the run measures the \
                         larger window and pays no model load.",
                        pm.id,
                    )
                }),
            // Without a declared window there is nothing to compare, and an
            // unreported resident ctx (0) is unknown, not undersized.
            ResidencyDecision::Reconcile { stale_ctx, .. } => declared
                .filter(|_| stale_ctx != 0)
                .map(|d| {
                    format!(
                        "declares model `{}` at {d} ctx but the resident instance is at {stale_ctx} \
                         — the dispatch unloads it and loads a new instance at the declared window, \
                         so this run's wall-clock includes a model load.",
                        pm.id,
                    )
                }),
            ResidencyDecision::ForeignDuplicate { foreign_identifier } => Some(format!(
                "model `{}` is resident only as `{foreign_identifier}`, which darkmux never reuses \
                 — the dispatch loads a new instance beside it, so this run's wall-clock includes \
                 a model load.",
                pm.id,
            )),
        };
        out.extend(note.map(|n| format!("requested profile `{profile_name}` {n}")));
    }
    out
}

/// The loaded set as the planner's residency facts (size and parallelism are
/// irrelevant to the reuse rule).
fn resident_facts(loaded: &[LoadedModel]) -> Vec<ResidentFact> {
    loaded
        .iter()
        .map(|lm| ResidentFact {
            identifier: lm.identifier.clone(),
            model_key: lm.model.clone(),
            ctx: lm.context,
            est_bytes: None,
            parallel: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::ProfileModel;

    fn pm(id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel {
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
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn warns_on_context_envelope_mismatch() {
        // The Beat-39 case: profile=deep declares 262K, balanced is loaded @101K.
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 101000)];
        let w = envelope_warnings(&p, "deep", &loaded);
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
        let w = envelope_warnings(&p, "small", &loaded);
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
        let w = envelope_warnings(&p, "deep", &loaded);
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    /// A user-loaded copy is never reused (#1274), so darkmux loads its own
    /// beside it, and that is a load inside the run.
    #[test]
    fn foreign_resident_is_never_reused() {
        let p = profile(vec![pm("qwen-35b", 32768)]);
        let loaded = vec![lm("qwen-35b", "qwen-35b", 262144)];
        let w = envelope_warnings(&p, "small", &loaded);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("loads a new instance"), "{}", w[0]);
    }

    #[test]
    fn warns_when_primary_not_loaded() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        // A wholly different model is loaded.
        let loaded = vec![lm("darkmux:other-model", "other-model", 32000)];
        let w = envelope_warnings(&p, "deep", &loaded);
        assert_eq!(w.len(), 1, "expected not-loaded warning, got: {w:?}");
        assert!(w[0].contains("not among the currently loaded"));
    }

    #[test]
    fn missing_non_default_model_does_not_warn() {
        // (#590) Only the default model (first model) is load-bearing for the
        // envelope. The first model is loaded; a missing SECOND (non-default)
        // model is common and quiet. (Replaces the old primary+compactor case —
        // the compactor moved to the registry's internal.utility binding and can
        // no longer live in `models[]`.)
        let p = profile(vec![pm("qwen-35b", 262000), pm("qwen-4b", 68000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn tolerates_rounding_within_5_percent() {
        // 262000 declared vs 262144 loaded (power-of-two) — benign.
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262144)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn unknown_loaded_context_does_not_warn() {
        // lms ps didn't report a context (0) — can't tell, stay quiet.
        let p = profile(vec![pm("qwen-35b", 262000)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 0)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn empty_loaded_set_yields_no_warnings() {
        let p = profile(vec![pm("qwen-35b", 262000)]);
        assert!(envelope_warnings(&p, "deep", &[]).is_empty());
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
        assert!(envelope_warnings(&p, "azure", &loaded).is_empty());
    }
}
