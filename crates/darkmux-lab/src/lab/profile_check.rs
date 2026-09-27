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
//! What this check still reports is whether that load happens INSIDE the
//! run: when the declared model is not resident, or is resident at a
//! different window, the run's wall-clock includes a model load and is not
//! comparable to a warm run. That matters to reproducibility (a notebook
//! comparing two runs should know one of them paid for a load).
//!
//! Per the operator-sovereignty doctrine: **warn, don't block**. We compare
//! the requested profile's declared model envelope against `lms ps` before
//! the dispatch and emit operator-facing notes; the dispatch proceeds
//! either way.

use darkmux_profiles::envelope::{ctx_diverges, loaded_matches};
use darkmux_types::{LoadedModel, Profile};

/// Compare the requested profile's declared model envelope against what
/// LMStudio actually has loaded, returning operator-facing warning lines
/// (empty when everything lines up — or when `loaded` is empty and we
/// can't tell). Pure: the caller owns the best-effort `lms ps` query and
/// the printing.
///
/// Two findings, both meaning "this run starts with a model load":
///   1. The profile's **default** model isn't in the loaded set at all.
///   2. A declared model **is** loaded but at a materially different
///      context window than the profile declares.
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
    for pm in &profile.models {
        // (#1282, #2902) Only a model darkmux manages has a loaded LM Studio
        // envelope to validate.
        if !pm.is_managed() {
            continue;
        }
        match loaded.iter().find(|lm| loaded_matches(lm, pm)) {
            None => {
                // A missing non-default model is common and not worth the noise.
                if Some(pm.id.as_str()) == default_id {
                    let declared = pm
                        .n_ctx
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "unset".to_string());
                    out.push(format!(
                        "requested profile `{profile_name}` declares default model `{}` (ctx {declared}) \
                         but it is not among the currently loaded models — the dispatch loads it \
                         first, so this run's wall-clock includes a model load and is not comparable \
                         to a warm run.",
                        pm.id,
                    ));
                }
            }
            Some(lm) => {
                // (#1282) No declared window (an Option now) ⇒ nothing to
                // compare — divergence is only meaningful against a declared
                // value.
                if let Some(declared) = pm.n_ctx {
                    if ctx_diverges(declared as u64, lm.context) {
                        out.push(format!(
                            "requested profile `{profile_name}` declares model `{}` at {} ctx but the \
                             loaded instance is at {} ctx — the dispatch loads darkmux's own instance at \
                             the declared window, so this run's wall-clock includes a model load.",
                            pm.id, declared, lm.context,
                        ));
                    }
                }
            }
        }
    }
    out
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
