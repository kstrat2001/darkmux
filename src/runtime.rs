use crate::swap::namespaced_identifier;
use crate::types::{Profile, ProfileModel};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Apply a profile's `runtime:` block to the configured runtime config file
/// (e.g. ~/.openclaw/openclaw.json). Targeted JSON edits — never overwrites
/// unrelated fields. Idempotent: a no-op if values already match.
///
/// Updates:
///   - agents.defaults.contextTokens (from runtime.contextTokens)
///   - agents.defaults.compaction (merged with runtime.compaction)
///   - models.providers.*.models[].contextWindow for any model id mentioned
///     in profile.models (so OpenClaw's view of the model's ctx matches what
///     LMStudio actually loaded)
///   - agents.defaults.model.primary rewritten from `lmstudio/<bare-id>` to
///     `lmstudio/darkmux:<bare-id>` when the bare id matches a profile model
///     (so dispatchers hit the namespaced load by exact identifier, avoiding
///     the silent-correctness gap when the user has the same model loaded
///     for their own purposes — see issue #52)
///   - agents.defaults.compaction.model rewritten the same way
///
/// Returns true if the file was modified.
pub fn apply_runtime(profile: &Profile) -> Result<bool> {
    let Some(rt) = profile.runtime.as_ref() else {
        return Ok(false);
    };
    let Some(path_str) = rt.config_path.as_ref() else {
        return Ok(false);
    };
    let path = Path::new(path_str);
    if !path.exists() {
        bail!("darkmux: runtime configPath does not exist: {}", path.display());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut config: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON at {}", path.display()))?;
    let mut modified = false;

    // 1. agents.defaults.contextTokens
    if let Some(want) = rt.context_tokens {
        let agents = config.as_object_mut().unwrap()
            .entry("agents".to_string()).or_insert(Value::Object(Default::default()))
            .as_object_mut().unwrap();
        let defaults = agents
            .entry("defaults".to_string()).or_insert(Value::Object(Default::default()))
            .as_object_mut().unwrap();
        let cur = defaults.get("contextTokens").and_then(|v| v.as_u64());
        if cur != Some(want) {
            defaults.insert("contextTokens".to_string(), Value::Number(want.into()));
            modified = true;
        }
    }

    // 2. agents.defaults.compaction (merge)
    if let Some(comp) = rt.compaction.as_ref() {
        let agents = config.as_object_mut().unwrap()
            .entry("agents".to_string()).or_insert(Value::Object(Default::default()))
            .as_object_mut().unwrap();
        let defaults = agents
            .entry("defaults".to_string()).or_insert(Value::Object(Default::default()))
            .as_object_mut().unwrap();
        let existing = defaults
            .get("compaction")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let mut merged = existing.as_object().cloned().unwrap_or_default();
        for (k, v) in comp {
            merged.insert(k.clone(), v.clone());
        }
        let new_compaction = Value::Object(merged);
        if defaults.get("compaction") != Some(&new_compaction) {
            defaults.insert("compaction".to_string(), new_compaction);
            modified = true;
        }
    }

    // 2b. Namespace darkmux-managed model references — rewrite bare model ids
    // in `agents.defaults.model.primary` and `agents.defaults.compaction.model`
    // to their `darkmux:` namespaced form when they match a profile model.
    //
    // Dispatchers (e.g. openclaw's lmstudio plugin) then resolve these by
    // *exact identifier match* against darkmux's load, avoiding the silent
    // mis-routing when the user has the same model loaded for another purpose.
    // See issue #52 for the empirical case that motivates this.
    if rewrite_namespaced_model_refs(&mut config, &profile.models) {
        modified = true;
    }

    // 3. models.providers.*.models[].contextWindow — match any model in profile
    if let Some(providers) = config
        .get_mut("models")
        .and_then(|m| m.get_mut("providers"))
        .and_then(|p| p.as_object_mut())
    {
        for (_pname, prov) in providers.iter_mut() {
            let Some(models) = prov.get_mut("models").and_then(|m| m.as_array_mut()) else {
                continue;
            };
            for entry in models.iter_mut() {
                let Some(entry_obj) = entry.as_object_mut() else {
                    continue;
                };
                let Some(id) = entry_obj.get("id").and_then(|x| x.as_str()) else {
                    continue;
                };
                if let Some(want) = profile
                    .models
                    .iter()
                    .find(|pm| pm.id == id)
                    .map(|pm| pm.n_ctx as u64)
                {
                    let cur = entry_obj.get("contextWindow").and_then(|x| x.as_u64());
                    if cur != Some(want) {
                        entry_obj.insert("contextWindow".to_string(), Value::Number(want.into()));
                        modified = true;
                    }
                }
            }
        }
    }

    if modified {
        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(path, pretty + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(modified)
}

/// Walk darkmux-managed model-reference sites and rewrite bare ids to their
/// `darkmux:` namespaced form when they match a profile model. Only touches
/// fields darkmux owns (`agents.defaults.model.*`, `agents.defaults.compaction.model`)
/// — per-agent overrides in `agents.list[]` and channel routing in
/// `channels.modelByChannel` are operator-managed and left untouched.
///
/// `lmstudio/<bare-id>` becomes `lmstudio/darkmux:<bare-id>`. Other provider
/// prefixes (e.g. `openrouter/`, `xai/`) are left as-is; the namespace only
/// applies to LMStudio loads.
///
/// Idempotent: already-namespaced refs are recognized and not rewritten
/// again. Returns true if any field was modified.
fn rewrite_namespaced_model_refs(config: &mut Value, models: &[ProfileModel]) -> bool {
    let mut modified = false;
    // Map of `lmstudio/<bare-id>` → `lmstudio/darkmux:<bare-id>` for every
    // model the profile wants loaded under the namespace. Skip models whose
    // profile entry sets an explicit identifier (operator opted out of the
    // namespace for that load).
    let mut rewrites: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in models {
        if m.identifier.is_some() {
            continue;
        }
        let ns_ident = namespaced_identifier(m);
        if ns_ident == m.id {
            continue; // no rewrite needed
        }
        let bare_ref = format!("lmstudio/{}", m.id);
        let ns_ref = format!("lmstudio/{ns_ident}");
        rewrites.insert(bare_ref, ns_ref);
    }
    if rewrites.is_empty() {
        return false;
    }

    // Rewrite agents.defaults.model.primary
    if let Some(model_obj) = config
        .get_mut("agents")
        .and_then(|a| a.get_mut("defaults"))
        .and_then(|d| d.get_mut("model"))
    {
        if let Some(primary) = model_obj.get_mut("primary").and_then(|v| v.as_str()).map(String::from) {
            if let Some(new_ref) = rewrites.get(&primary) {
                model_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("primary".to_string(), Value::String(new_ref.clone()));
                modified = true;
            }
        }
    }

    // Rewrite agents.defaults.compaction.model
    if let Some(comp_obj) = config
        .get_mut("agents")
        .and_then(|a| a.get_mut("defaults"))
        .and_then(|d| d.get_mut("compaction"))
        .and_then(|c| c.as_object_mut())
    {
        if let Some(cur) = comp_obj.get("model").and_then(|v| v.as_str()).map(String::from) {
            if let Some(new_ref) = rewrites.get(&cur) {
                comp_obj.insert("model".to_string(), Value::String(new_ref.clone()));
                modified = true;
            }
        }
    }

    modified
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelRole, Profile, ProfileModel, ProfileRuntime};
    use serde_json::Value as JsonValue;
    use tempfile::TempDir;
    type Compaction = serde_json::Map<String, JsonValue>;

    fn write_config(dir: &TempDir, contents: &str) -> std::path::PathBuf {
        let p = dir.path().join("config.json");
        fs::write(&p, contents).unwrap();
        p
    }

    fn profile_with_runtime(
        config_path: &str,
        context_tokens: Option<u64>,
        compaction: Option<Compaction>,
        models: Vec<ProfileModel>,
    ) -> Profile {
        Profile {
            description: None,
            models,
            runtime: Some(ProfileRuntime {
                config_path: Some(config_path.to_string()),
                context_tokens,
                compaction,
            }),
            use_when: None,
        }
    }

    #[test]
    fn no_runtime_block_returns_false() {
        let profile = Profile {
            description: None,
            models: vec![],
            runtime: None,
            use_when: None,
        };
        assert!(!apply_runtime(&profile).unwrap());
    }

    #[test]
    fn no_config_path_returns_false() {
        let profile = Profile {
            description: None,
            models: vec![],
            runtime: Some(ProfileRuntime::default()),
            use_when: None,
        };
        assert!(!apply_runtime(&profile).unwrap());
    }

    #[test]
    fn missing_config_file_errors() {
        let profile = profile_with_runtime("/nonexistent/path.json", Some(1000), None, vec![]);
        let err = apply_runtime(&profile).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn updates_context_tokens() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(&tmp, r#"{"agents": {"defaults": {"contextTokens": 90000}}}"#);
        let profile = profile_with_runtime(p.to_str().unwrap(), Some(250000), None, vec![]);
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(after["agents"]["defaults"]["contextTokens"], 250000);
    }

    #[test]
    fn idempotent_when_already_matches() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(&tmp, r#"{"agents": {"defaults": {"contextTokens": 250000}}}"#);
        let profile = profile_with_runtime(p.to_str().unwrap(), Some(250000), None, vec![]);
        assert!(!apply_runtime(&profile).unwrap());
    }

    #[test]
    fn merges_compaction_block() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{"agents":{"defaults":{"compaction":{"mode":"default","model":"old-model"}}}}"#,
        );
        let mut comp = Compaction::new();
        comp.insert(
            "model".into(),
            JsonValue::String("new-model".into()),
        );
        comp.insert(
            "maxHistoryShare".into(),
            JsonValue::from(0.35),
        );
        let profile = profile_with_runtime(p.to_str().unwrap(), None, Some(comp), vec![]);
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(after["agents"]["defaults"]["compaction"]["model"], "new-model");
        // unchanged keys preserved
        assert_eq!(after["agents"]["defaults"]["compaction"]["mode"], "default");
        assert!(after["agents"]["defaults"]["compaction"]["maxHistoryShare"].is_number());
    }

    #[test]
    fn updates_per_model_context_window() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "primary-id", "contextWindow": 100000},
                                {"id": "compactor-id", "contextWindow": 100000},
                                {"id": "untouched-id", "contextWindow": 50000}
                            ]
                        }
                    }
                }
            }"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![
                ProfileModel {
                    id: "primary-id".into(),
                    n_ctx: 262144,
                    role: ModelRole::Primary,
                    identifier: None,
                },
                ProfileModel {
                    id: "compactor-id".into(),
                    n_ctx: 120000,
                    role: ModelRole::Compactor,
                    identifier: None,
                },
            ],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        let by_id = |id: &str| {
            arr.iter()
                .find(|m| m["id"] == id)
                .unwrap()["contextWindow"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(by_id("primary-id"), 262144);
        assert_eq!(by_id("compactor-id"), 120000);
        assert_eq!(by_id("untouched-id"), 50000); // untouched
    }

    #[test]
    fn missing_models_block_is_ok() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(&tmp, r#"{"agents": {"defaults": {"contextTokens": 90000}}}"#);
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            Some(250000),
            None,
            vec![ProfileModel {
                id: "any".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        // should not error even though models.providers doesn't exist
        assert!(apply_runtime(&profile).unwrap());
    }

    #[test]
    fn creates_agents_defaults_when_missing() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(&tmp, r#"{}"#);
        let profile = profile_with_runtime(p.to_str().unwrap(), Some(50000), None, vec![]);
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(after["agents"]["defaults"]["contextTokens"], 50000);
    }

    #[test]
    fn rewrites_defaults_model_primary_to_namespaced() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "agents": {
                    "defaults": {
                        "model": {
                            "primary": "lmstudio/qwen3.6-35b-a3b",
                            "fallbacks": []
                        }
                    }
                }
            }"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "qwen3.6-35b-a3b".into(),
                n_ctx: 100000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            after["agents"]["defaults"]["model"]["primary"],
            "lmstudio/darkmux:qwen3.6-35b-a3b"
        );
    }

    #[test]
    fn rewrites_compaction_model_to_namespaced() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "agents": {
                    "defaults": {
                        "compaction": {
                            "mode": "default",
                            "model": "lmstudio/qwen3-4b-instruct-2507",
                            "maxHistoryShare": 0.35
                        }
                    }
                }
            }"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "qwen3-4b-instruct-2507".into(),
                n_ctx: 120000,
                role: ModelRole::Compactor,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            after["agents"]["defaults"]["compaction"]["model"],
            "lmstudio/darkmux:qwen3-4b-instruct-2507"
        );
        // Unrelated fields preserved
        assert_eq!(after["agents"]["defaults"]["compaction"]["mode"], "default");
    }

    #[test]
    fn skips_namespaced_rewrite_when_profile_sets_explicit_identifier() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{"agents":{"defaults":{"model":{"primary":"lmstudio/foo","fallbacks":[]}}}}"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: Some("my-explicit-alias".into()),
            }],
        );
        // Profile sets explicit identifier → operator opted out of namespace
        // for this load → primary reference should NOT be rewritten.
        let _ = apply_runtime(&profile).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            after["agents"]["defaults"]["model"]["primary"],
            "lmstudio/foo"
        );
    }

    #[test]
    fn rewrite_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        // Already-namespaced primary — second swap should NOT modify it.
        let p = write_config(
            &tmp,
            r#"{"agents":{"defaults":{"model":{"primary":"lmstudio/darkmux:foo","fallbacks":[]}}}}"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        // No modification expected — primary is already namespaced.
        assert!(!apply_runtime(&profile).unwrap());
    }

    #[test]
    fn leaves_per_agent_model_overrides_alone() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "agents": {
                    "defaults": {"model": {"primary": "lmstudio/foo", "fallbacks": []}},
                    "list": [
                        {"id": "user-agent", "model": "lmstudio/foo"}
                    ]
                }
            }"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        // Defaults rewritten…
        assert_eq!(
            after["agents"]["defaults"]["model"]["primary"],
            "lmstudio/darkmux:foo"
        );
        // …but the operator-set per-agent override is preserved verbatim.
        // (Operator-sovereignty: darkmux doesn't touch user-managed agents.)
        assert_eq!(
            after["agents"]["list"][0]["model"],
            "lmstudio/foo"
        );
    }

    #[test]
    fn leaves_non_lmstudio_provider_refs_alone() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{"agents":{"defaults":{"model":{"primary":"openrouter/google/gemini-2.5","fallbacks":[]}}}}"#,
        );
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "google/gemini-2.5".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        let _ = apply_runtime(&profile).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        // Non-lmstudio provider prefix → namespace doesn't apply.
        assert_eq!(
            after["agents"]["defaults"]["model"]["primary"],
            "openrouter/google/gemini-2.5"
        );
    }
}
