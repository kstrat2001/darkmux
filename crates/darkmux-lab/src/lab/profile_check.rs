//! (#365) Profile-vs-loaded envelope check.
//!
//! `darkmux lab run` stamps each run's `manifest.json` with the
//! *requested* profile name (either `--profile` or the registry's
//! `default_profile`). But the dispatch goes through LMStudio's
//! OpenAI-compatible API by model id, so LMStudio answers with whatever
//! is actually loaded — regardless of which profile the lab thinks it's
//! using. If the operator did `darkmux swap balanced` and then
//! `darkmux lab run medium-coding` (no `--profile`), the manifest says
//! `profile=deep` while the real runtime envelope is `balanced`'s.
//!
//! That silent provenance drift poisons reproducibility: a notebook
//! entry citing "measured against profile deep" is wrong, and a future
//! operator re-running at `deep` gets a different context envelope.
//!
//! Per the operator-sovereignty doctrine, the fix is the *least*
//! intrusive of the issue's three options: **warn, don't block**. We
//! compare the requested profile's declared model envelope against
//! `lms ps` and emit operator-facing warnings on divergence; the
//! dispatch proceeds either way (the operator may have swapped
//! deliberately — A/B, defensive escalation, candidate eval).

use darkmux_profiles::envelope::{ctx_diverges, loaded_matches};
use darkmux_types::{LoadedModel, Profile};

/// Compare the requested profile's declared model envelope against what
/// LMStudio actually has loaded, returning operator-facing warning lines
/// (empty when everything lines up — or when `loaded` is empty and we
/// can't tell). Pure: the caller owns the best-effort `lms ps` query and
/// the printing.
///
/// Two findings, both pointing at the same risk — the manifest's
/// `profile=` tag may not reflect the real runtime envelope:
///   1. The profile's **default worker** model isn't in the loaded set at
///      all, so the dispatch will use whatever LMStudio has loaded.
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
    // (#590) Only the default worker (default_model, or first model) is
    // load-bearing for the measurement envelope.
    let default_id = profile.default_model_id();
    for pm in &profile.models {
        match loaded.iter().find(|lm| loaded_matches(lm, pm)) {
            None => {
                // A missing non-default model is common and not worth the noise.
                if Some(pm.id.as_str()) == default_id {
                    out.push(format!(
                        "requested profile `{profile_name}` declares default-worker model `{}` (ctx {}) \
                         but it is not among the currently loaded models — the dispatch will use \
                         whatever LMStudio has loaded, so this run's `profile={profile_name}` tag \
                         may not reflect the real runtime envelope. Run `darkmux swap {profile_name}` \
                         or pass `--profile <loaded-profile>` to align them.",
                        pm.id, pm.n_ctx,
                    ));
                }
            }
            Some(lm) if ctx_diverges(pm.n_ctx as u64, lm.context) => {
                out.push(format!(
                    "requested profile `{profile_name}` declares model `{}` at {} ctx but the \
                     loaded instance is at {} ctx — proceeding, but this run's `profile={profile_name}` \
                     tag may not reflect the real runtime envelope (loaded != requested).",
                    pm.id, pm.n_ctx, lm.context,
                ));
            }
            Some(_) => {}
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
            id: id.to_string(),
            n_ctx,
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
        }
    }

    fn profile(models: Vec<ProfileModel>) -> Profile {
        Profile {
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
        // (#590) Only the default worker (first model) is load-bearing for the
        // envelope. The first model is loaded; a missing SECOND (non-default)
        // worker is common and quiet. (Replaces the old primary+compactor case —
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
}
