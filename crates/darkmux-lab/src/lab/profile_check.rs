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

use darkmux_types::{LoadedModel, ModelRole, Profile, ProfileModel};

/// Does this loaded model correspond to the profile's declared model?
///
/// Matches on the bare model key first — the canonical dispatch
/// resolution per the darkmux namespace convention (`identifier =
/// darkmux:foo`, `modelKey = foo`; dispatchers call with the bare `foo`
/// and resolve via `modelKey`). Also accepts the namespaced identifier
/// and any explicit `identifier` the operator set in the profile.
fn matches(lm: &LoadedModel, pm: &ProfileModel) -> bool {
    let namespaced = format!("darkmux:{}", pm.id);
    lm.model == pm.id
        || lm.identifier == pm.id
        || lm.identifier == namespaced
        || pm
            .identifier
            .as_deref()
            .is_some_and(|id| id == lm.identifier || id == lm.model)
}

/// Declared vs loaded context windows differ enough to flag?
///
/// A 5% tolerance absorbs benign rounding (a profile declaring `262000`
/// against a `262144` power-of-two load) while still catching a real
/// envelope swap (the Beat-39 case: `262000` declared vs `101000`
/// loaded). A loaded context of `0` means `lms ps` didn't report one —
/// we can't tell, so we don't flag.
fn ctx_diverges(declared: u64, loaded: u64) -> bool {
    if loaded == 0 {
        return false;
    }
    declared.abs_diff(loaded) * 20 > declared
}

/// Compare the requested profile's declared model envelope against what
/// LMStudio actually has loaded, returning operator-facing warning lines
/// (empty when everything lines up — or when `loaded` is empty and we
/// can't tell). Pure: the caller owns the best-effort `lms ps` query and
/// the printing.
///
/// Two findings, both pointing at the same risk — the manifest's
/// `profile=` tag may not reflect the real runtime envelope:
///   1. The profile's **Primary** model isn't in the loaded set at all,
///      so the dispatch will use whatever LMStudio has loaded.
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
    for pm in &profile.models {
        match loaded.iter().find(|lm| matches(lm, pm)) {
            None => {
                // Only the Primary model is load-bearing for the
                // measurement envelope; a missing compactor/auxiliary is
                // common and not worth the noise.
                if matches!(pm.role, ModelRole::Primary) {
                    out.push(format!(
                        "requested profile `{profile_name}` declares primary model `{}` (ctx {}) \
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

    fn pm(id: &str, n_ctx: u32, role: ModelRole) -> ProfileModel {
        ProfileModel {
            id: id.to_string(),
            n_ctx,
            role,
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
            models,
            runtime: None,
            use_when: None,
        }
    }

    #[test]
    fn matches_on_bare_model_key() {
        // namespaced load: identifier=darkmux:foo, modelKey=foo
        let loaded = lm("darkmux:qwen-35b", "qwen-35b", 262000);
        assert!(matches(
            &loaded,
            &pm("qwen-35b", 262000, ModelRole::Primary)
        ));
    }

    #[test]
    fn matches_on_namespaced_identifier() {
        let loaded = lm("darkmux:qwen-35b", "different-key", 262000);
        assert!(matches(
            &loaded,
            &pm("qwen-35b", 262000, ModelRole::Primary)
        ));
    }

    #[test]
    fn no_warning_when_envelope_aligns() {
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn warns_on_context_envelope_mismatch() {
        // The Beat-39 case: profile=deep declares 262K, balanced is loaded @101K.
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 101000)];
        let w = envelope_warnings(&p, "deep", &loaded);
        assert_eq!(w.len(), 1, "expected one mismatch warning, got: {w:?}");
        assert!(w[0].contains("262000") && w[0].contains("101000"));
        assert!(w[0].contains("deep"));
    }

    #[test]
    fn warns_when_primary_not_loaded() {
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        // A wholly different model is loaded.
        let loaded = vec![lm("darkmux:other-model", "other-model", 32000)];
        let w = envelope_warnings(&p, "deep", &loaded);
        assert_eq!(w.len(), 1, "expected not-loaded warning, got: {w:?}");
        assert!(w[0].contains("not among the currently loaded"));
    }

    #[test]
    fn missing_compactor_does_not_warn() {
        // Only the primary is loaded; a missing compactor is common and quiet.
        let p = profile(vec![
            pm("qwen-35b", 262000, ModelRole::Primary),
            pm("qwen-4b", 68000, ModelRole::Compactor),
        ]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262000)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn tolerates_rounding_within_5_percent() {
        // 262000 declared vs 262144 loaded (power-of-two) — benign.
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 262144)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn unknown_loaded_context_does_not_warn() {
        // lms ps didn't report a context (0) — can't tell, stay quiet.
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        let loaded = vec![lm("darkmux:qwen-35b", "qwen-35b", 0)];
        assert!(envelope_warnings(&p, "deep", &loaded).is_empty());
    }

    #[test]
    fn empty_loaded_set_yields_no_warnings() {
        let p = profile(vec![pm("qwen-35b", 262000, ModelRole::Primary)]);
        assert!(envelope_warnings(&p, "deep", &[]).is_empty());
    }
}
