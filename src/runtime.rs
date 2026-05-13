use crate::swap::namespaced_identifier;
use crate::types::{Profile, ProfileModel};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve the openclaw config path for a given operation.
///
/// Lookup order:
///   1. `explicit_path` — typically `profile.runtime.config_path` when set;
///      respects the operator's pin to a specific config file
///   2. `DARKMUX_OPENCLAW_CONFIG` env var — escape hatch for non-standard
///      installs (also makes the path injectable from tests)
///   3. Default `~/.openclaw/openclaw.json` when `DARKMUX_RUNTIME_CMD`
///      targets openclaw (its default)
///   4. `None` for unrecognized runtimes — caller decides whether to
///      skip quietly or surface the gap
///
/// Exposed for `doctor --fix`, which has no profile context but still
/// needs to find the openclaw config to apply ctx-window fixes.
pub fn resolve_openclaw_config_path(explicit_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit_path {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = env::var("DARKMUX_OPENCLAW_CONFIG") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let runtime_cmd =
        env::var("DARKMUX_RUNTIME_CMD").unwrap_or_else(|_| "openclaw".to_string());
    if runtime_cmd == "openclaw" {
        if let Some(home) = dirs::home_dir() {
            return Some(home.join(".openclaw/openclaw.json"));
        }
    }
    None
}

/// Apply a profile's `runtime:` block to the configured runtime config file
/// (e.g. ~/.openclaw/openclaw.json). Targeted JSON edits — never overwrites
/// unrelated fields. Idempotent: a no-op if values already match.
///
/// Two classes of edits:
///   - **Genuinely runtime-customization** (gated on `profile.runtime`):
///     - `agents.defaults.contextTokens` (from `runtime.contextTokens`)
///     - `agents.defaults.compaction` (merged with `runtime.compaction`)
///   - **Inherent to namespaced loading** (always runs whenever there's an
///     openclaw config to update — these aren't customizations, they're
///     consequences of `darkmux swap` issuing namespaced `lms load`s):
///     - `agents.defaults.model.primary` / `agents.defaults.compaction.model`
///       rewritten from `lmstudio/<bare-id>` to `lmstudio/darkmux:<bare-id>`
///     - `models.providers.*.models[].contextWindow` synced to profile n_ctx
///       for any matching bare-or-namespaced id
///     - Namespaced registry entries added when missing (see #61)
///
/// Path resolution falls back to `~/.openclaw/openclaw.json` when the
/// profile has no `runtime.config_path` and `DARKMUX_RUNTIME_CMD` targets
/// openclaw (its default). On other runtimes, returns `Ok(false)` quietly.
///
/// Returns true if the file was modified.
pub fn apply_runtime(profile: &Profile) -> Result<bool> {
    let explicit_path = profile
        .runtime
        .as_ref()
        .and_then(|rt| rt.config_path.as_deref());
    let Some(path) = resolve_openclaw_config_path(explicit_path) else {
        return Ok(false);
    };
    if !path.exists() {
        // Operator pinned an explicit config_path that doesn't resolve —
        // that's an error worth surfacing. Fallback-to-default with the
        // file absent is a quiet skip (operator may not have run openclaw
        // yet on this machine).
        if explicit_path.is_some() {
            bail!(
                "darkmux: runtime configPath does not exist: {}",
                path.display()
            );
        }
        return Ok(false);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut config: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON at {}", path.display()))?;
    let mut modified = false;

    // ─── Runtime-customization fields (require profile.runtime block) ──
    if let Some(rt) = profile.runtime.as_ref() {
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
    }

    // ─── Namespaced-load sync (always runs when there's a config to update) ──
    // These aren't operator customizations — they're consequences of issuing
    // namespaced `lms load`s in swap.rs. Without them, openclaw's view of the
    // model registry drifts from `lms ps` every swap (issue #72).
    if rewrite_namespaced_model_refs(&mut config, &profile.models) {
        modified = true;
    }
    if sync_context_windows(&mut config, &profile.models) {
        modified = true;
    }
    if ensure_namespaced_entries(&mut config, &profile.models) {
        modified = true;
    }

    if modified {
        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(&path, pretty + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(modified)
}

/// Sync `models.providers.*.models[].contextWindow` against profile n_ctx
/// for any entry whose id matches the bare or namespaced form of a profile
/// model. Internal helper for `apply_runtime`. See issue #72 for why this
/// must run regardless of whether the profile has a `runtime` block.
fn sync_context_windows(config: &mut Value, models: &[ProfileModel]) -> bool {
    let Some(providers) = config
        .get_mut("models")
        .and_then(|m| m.get_mut("providers"))
        .and_then(|p| p.as_object_mut())
    else {
        return false;
    };
    let mut modified = false;
    for (_pname, prov) in providers.iter_mut() {
        let Some(entries) = prov.get_mut("models").and_then(|m| m.as_array_mut()) else {
            continue;
        };
        for entry in entries.iter_mut() {
            let Some(entry_obj) = entry.as_object_mut() else { continue };
            let Some(id) = entry_obj.get("id").and_then(|x| x.as_str()) else { continue };
            let id = id.to_string();
            let want = models.iter().find_map(|pm| {
                if pm.id == id || namespaced_identifier(pm) == id {
                    Some(pm.n_ctx as u64)
                } else {
                    None
                }
            });
            if let Some(want) = want {
                let cur = entry_obj.get("contextWindow").and_then(|x| x.as_u64());
                if cur != Some(want) {
                    entry_obj.insert(
                        "contextWindow".to_string(),
                        Value::Number(want.into()),
                    );
                    modified = true;
                }
            }
        }
    }
    modified
}

/// Ensure every namespaced profile model has a registry entry in the
/// lmstudio provider, cloning the bare-id entry as the template when
/// available and synthesizing minimal metadata otherwise. See issue #61
/// for the metadata-edge motivation (compaction trigger calculation
/// needs registry entries that match the dispatched identifier).
fn ensure_namespaced_entries(config: &mut Value, models: &[ProfileModel]) -> bool {
    let Some(lmstudio_models) = config
        .get_mut("models")
        .and_then(|m| m.get_mut("providers"))
        .and_then(|p| p.get_mut("lmstudio"))
        .and_then(|l| l.get_mut("models"))
        .and_then(|m| m.as_array_mut())
    else {
        return false;
    };
    let mut modified = false;
    for pm in models.iter() {
        if pm.identifier.is_some() {
            continue; // operator opted out of the namespace for this load
        }
        let ns_ident = namespaced_identifier(pm);
        if ns_ident == pm.id {
            continue; // no rewrite happened; no namespaced entry needed
        }
        let already_has_ns = lmstudio_models
            .iter()
            .any(|e| e.get("id").and_then(|v| v.as_str()) == Some(ns_ident.as_str()));
        if already_has_ns {
            continue; // sync_context_windows keeps its contextWindow in sync
        }
        let template: Option<Value> = lmstudio_models
            .iter()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(pm.id.as_str()))
            .cloned();
        let mut entry = template.unwrap_or_else(|| {
            serde_json::json!({
                "id": ns_ident,
                "contextWindow": pm.n_ctx as u64,
                "input": ["text"],
                "maxTokens": 8192,
                "cost": {"cacheRead": 0, "cacheWrite": 0, "input": 0, "output": 0},
                "name": format!("{ns_ident} (darkmux-managed)"),
                "reasoning": false
            })
        });
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("id".to_string(), Value::String(ns_ident.clone()));
            obj.insert(
                "contextWindow".to_string(),
                Value::Number((pm.n_ctx as u64).into()),
            );
        }
        lmstudio_models.push(entry);
        modified = true;
    }
    modified
}

/// One openclaw.json `contextWindow` entry that was rewritten by
/// `fix_ctx_window_to_loaded` to match the loaded ctx. Surfaced so
/// callers (typically `doctor --fix`) can render *what* changed, not
/// just *how many*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtxWindowChange {
    /// `id` field of the openclaw.json model entry that was rewritten.
    /// May be a bare id (`openai/gpt-oss-20b`) or namespaced
    /// (`darkmux:openai/gpt-oss-20b`).
    pub model_id: String,
    /// Old `contextWindow` value in the openclaw entry before the fix.
    pub from: u64,
    /// New value — matches what `lms ps` reports loaded for this model.
    pub to: u64,
}

/// Realign `models.providers.*.models[].contextWindow` to what `lms ps`
/// actually has loaded right now. Used by `doctor --fix` to close the
/// ctx-window-mismatch loop without needing an active profile.
///
/// Returns the list of rewritten entries — empty when openclaw.json
/// already matches loaded state (the idempotent case).
pub fn fix_ctx_window_to_loaded(
    config_path: &Path,
    loaded: &[crate::types::LoadedModel],
) -> Result<Vec<CtxWindowChange>> {
    if !config_path.exists() {
        bail!(
            "darkmux: openclaw config does not exist: {}",
            config_path.display()
        );
    }
    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut config: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON at {}", config_path.display()))?;

    // Build a map of loaded identifier (and its bare form, if namespaced)
    // → loaded ctx. Both shapes get the same target so we patch matching
    // entries regardless of which side carries the `darkmux:` prefix.
    let mut want: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for m in loaded {
        let ctx = m.context;
        want.insert(m.identifier.clone(), ctx);
        if let Some(bare) = m.identifier.strip_prefix("darkmux:") {
            want.insert(bare.to_string(), ctx);
        }
    }

    let mut changes: Vec<CtxWindowChange> = Vec::new();
    if let Some(providers) = config
        .get_mut("models")
        .and_then(|m| m.get_mut("providers"))
        .and_then(|p| p.as_object_mut())
    {
        for (_pname, prov) in providers.iter_mut() {
            let Some(entries) = prov.get_mut("models").and_then(|m| m.as_array_mut())
            else {
                continue;
            };
            for entry in entries.iter_mut() {
                let Some(entry_obj) = entry.as_object_mut() else { continue };
                let Some(id) = entry_obj.get("id").and_then(|x| x.as_str()) else {
                    continue;
                };
                let id = id.to_string();
                let Some(target) = want.get(&id).copied() else { continue };
                let cur = entry_obj
                    .get("contextWindow")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                if cur != target {
                    entry_obj.insert(
                        "contextWindow".to_string(),
                        Value::Number(target.into()),
                    );
                    changes.push(CtxWindowChange {
                        model_id: id,
                        from: cur,
                        to: target,
                    });
                }
            }
        }
    }

    if !changes.is_empty() {
        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(config_path, pretty + "\n")
            .with_context(|| format!("writing {}", config_path.display()))?;
    }
    Ok(changes)
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
    fn adds_namespaced_registry_entry_from_bare_template() {
        // Bare-id entry exists; section 4 should clone it as a namespaced
        // entry with the profile's contextWindow. Closes the metadata edge
        // (2026-05-13 eureka).
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {
                                    "id": "qwen3.6-35b-a3b",
                                    "contextWindow": 131072,
                                    "cost": {"cacheRead": 0, "cacheWrite": 0, "input": 0, "output": 0},
                                    "input": ["text"],
                                    "maxTokens": 8192,
                                    "name": "Qwen 3.6 35B A3B (LMStudio)",
                                    "reasoning": true
                                }
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
            vec![ProfileModel {
                id: "qwen3.6-35b-a3b".into(),
                n_ctx: 101000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 2, "namespaced entry should have been added");

        let ns_entry = arr
            .iter()
            .find(|e| e["id"] == "darkmux:qwen3.6-35b-a3b")
            .expect("namespaced entry not found");
        assert_eq!(ns_entry["contextWindow"], 101000);
        // Template fields preserved from the bare-id entry.
        assert_eq!(ns_entry["maxTokens"], 8192);
        assert_eq!(ns_entry["reasoning"], true);
        assert_eq!(ns_entry["input"][0], "text");
        assert_eq!(ns_entry["cost"]["cacheRead"], 0);
    }

    #[test]
    fn synthesizes_minimal_registry_entry_when_no_template() {
        // No bare-id entry exists; section 4 should synthesize a minimal
        // entry rather than leaving the namespaced load without metadata.
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": []
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
                id: "unknown-model-id".into(),
                n_ctx: 64000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 1);
        let entry = &arr[0];
        assert_eq!(entry["id"], "darkmux:unknown-model-id");
        assert_eq!(entry["contextWindow"], 64000);
        assert!(entry["maxTokens"].is_number());
        assert!(entry["input"].is_array());
    }

    #[test]
    fn does_not_duplicate_namespaced_entry_on_second_swap() {
        // Section 4 must be idempotent — running apply_runtime twice should
        // not add a second namespaced entry.
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "foo", "contextWindow": 10000}
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
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 50000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );

        // First swap: section 4 adds the namespaced entry.
        assert!(apply_runtime(&profile).unwrap());
        let mid: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let mid_arr = mid["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(mid_arr.len(), 2);

        // Second swap: idempotent — no further modification.
        assert!(!apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 2, "second swap should not duplicate the namespaced entry");
    }

    #[test]
    fn section_3_resyncs_namespaced_entry_context_window() {
        // After section 4 adds a namespaced entry, a later swap with a
        // different n_ctx should re-sync its contextWindow via section 3's
        // extended id matching.
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "foo", "contextWindow": 10000},
                                {"id": "darkmux:foo", "contextWindow": 50000}
                            ]
                        }
                    }
                }
            }"#,
        );
        // Profile now wants n_ctx=80000 (different from the stored 50000).
        let profile = profile_with_runtime(
            p.to_str().unwrap(),
            None,
            None,
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 80000,
                role: ModelRole::Primary,
                identifier: None,
            }],
        );
        assert!(apply_runtime(&profile).unwrap());
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        let ns_entry = arr
            .iter()
            .find(|e| e["id"] == "darkmux:foo")
            .expect("namespaced entry should still exist");
        assert_eq!(ns_entry["contextWindow"], 80000);
    }

    #[test]
    fn does_not_add_namespaced_entry_when_identifier_explicit() {
        // Profile sets an explicit identifier → operator opted out of the
        // namespace → section 4 should NOT add a namespaced entry.
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "foo", "contextWindow": 10000}
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
            vec![ProfileModel {
                id: "foo".into(),
                n_ctx: 50000,
                role: ModelRole::Primary,
                identifier: Some("my-alias".into()),
            }],
        );
        let _ = apply_runtime(&profile).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(arr.len(), 1, "no namespaced entry when identifier is explicit");
        assert_eq!(arr[0]["id"], "foo");
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

    // ─── Issue #72: namespaced-load sync must run even without runtime block ──
    //
    // Tests below use #[serial_test::serial] because they mutate process env
    // vars (DARKMUX_OPENCLAW_CONFIG / DARKMUX_RUNTIME_CMD) — running them in
    // parallel races against any other test that touches those vars.

    use crate::types::LoadedModel;
    use serial_test::serial;

    fn profile_without_runtime(models: Vec<ProfileModel>) -> Profile {
        Profile {
            description: None,
            models,
            runtime: None,
            use_when: None,
        }
    }

    /// The #72 regression case: profile has no `runtime` block, but
    /// `darkmux swap` still loads namespaced models. The openclaw config
    /// must still get its contextWindow + namespaced entries synced via
    /// the env-var override (operators get this via `~/.openclaw/openclaw.json`
    /// fallback; this test substitutes a tempfile via `DARKMUX_OPENCLAW_CONFIG`).
    #[test]
    #[serial]
    fn no_runtime_block_still_syncs_namespaced_load() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "openai/gpt-oss-20b", "contextWindow": 32768}
                            ]
                        }
                    }
                }
            }"#,
        );
        let profile = profile_without_runtime(vec![ProfileModel {
            id: "openai/gpt-oss-20b".into(),
            n_ctx: 100000,
            role: ModelRole::Primary,
            identifier: None,
        }]);

        unsafe { env::set_var("DARKMUX_OPENCLAW_CONFIG", p.to_str().unwrap()) };
        let modified = apply_runtime(&profile).unwrap();
        unsafe { env::remove_var("DARKMUX_OPENCLAW_CONFIG") };

        assert!(modified, "namespaced sync should fire without a runtime block");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        // Bare-id entry got its contextWindow synced to profile n_ctx.
        let bare = arr.iter().find(|m| m["id"] == "openai/gpt-oss-20b").unwrap();
        assert_eq!(bare["contextWindow"], 100000);
        // Namespaced entry was added.
        let ns = arr
            .iter()
            .find(|m| m["id"] == "darkmux:openai/gpt-oss-20b")
            .expect("namespaced entry should have been added");
        assert_eq!(ns["contextWindow"], 100000);
    }

    /// Path resolution priority: explicit > env > openclaw default. Lock the
    /// order so future changes don't accidentally reorder it.
    #[test]
    #[serial]
    fn resolve_path_priority_explicit_over_env_over_default() {
        unsafe { env::set_var("DARKMUX_OPENCLAW_CONFIG", "/from/env.json") };
        // 1. Explicit wins over env.
        assert_eq!(
            resolve_openclaw_config_path(Some("/from/explicit.json")),
            Some(PathBuf::from("/from/explicit.json"))
        );
        // 2. Env wins when no explicit.
        assert_eq!(
            resolve_openclaw_config_path(None),
            Some(PathBuf::from("/from/env.json"))
        );
        // 3. Default kicks in when no explicit and no env (and runtime is openclaw).
        unsafe { env::remove_var("DARKMUX_OPENCLAW_CONFIG") };
        unsafe { env::remove_var("DARKMUX_RUNTIME_CMD") };
        let resolved = resolve_openclaw_config_path(None);
        // Can't assert exact path (depends on $HOME), but should end with the canonical suffix.
        assert!(
            resolved
                .as_ref()
                .map(|p| p.ends_with(".openclaw/openclaw.json"))
                .unwrap_or(false),
            "default should be $HOME/.openclaw/openclaw.json, got {resolved:?}"
        );
    }

    #[test]
    #[serial]
    fn resolve_path_returns_none_for_unrecognized_runtime() {
        unsafe { env::remove_var("DARKMUX_OPENCLAW_CONFIG") };
        unsafe { env::set_var("DARKMUX_RUNTIME_CMD", "aider") };
        let resolved = resolve_openclaw_config_path(None);
        unsafe { env::remove_var("DARKMUX_RUNTIME_CMD") };
        assert!(resolved.is_none(), "unrecognized runtime should yield None, got {resolved:?}");
    }

    // ─── fix_ctx_window_to_loaded (doctor --fix handler) ──

    fn loaded_model(identifier: &str, ctx: u64) -> LoadedModel {
        LoadedModel {
            identifier: identifier.into(),
            model: identifier.into(),
            status: "idle".into(),
            size: "12.00 GB".into(),
            context: ctx,
        }
    }

    #[test]
    fn fix_ctx_window_aligns_bare_and_namespaced_entries() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "openai/gpt-oss-20b", "contextWindow": 32768},
                                {"id": "darkmux:openai/gpt-oss-20b", "contextWindow": 32768},
                                {"id": "unrelated/model", "contextWindow": 50000}
                            ]
                        }
                    }
                }
            }"#,
        );
        // Loaded: namespaced form at 100000 ctx. Both bare and namespaced
        // entries should be aligned; the unrelated entry stays put.
        let loaded = vec![loaded_model("darkmux:openai/gpt-oss-20b", 100000)];
        let changes = fix_ctx_window_to_loaded(&p, &loaded).unwrap();

        // Assert exactly which two entries got rewritten — both shapes of
        // the loaded id, with the right old→new transition. Order isn't
        // guaranteed by the iteration, so collect by id before checking.
        assert_eq!(changes.len(), 2, "both bare and namespaced should be patched");
        let by_change_id: std::collections::HashMap<&str, &CtxWindowChange> =
            changes.iter().map(|c| (c.model_id.as_str(), c)).collect();
        let bare = by_change_id
            .get("openai/gpt-oss-20b")
            .expect("bare-id change missing");
        assert_eq!(bare.from, 32768);
        assert_eq!(bare.to, 100000);
        let ns = by_change_id
            .get("darkmux:openai/gpt-oss-20b")
            .expect("namespaced change missing");
        assert_eq!(ns.from, 32768);
        assert_eq!(ns.to, 100000);
        assert!(
            !by_change_id.contains_key("unrelated/model"),
            "unrelated entry should not appear in changes"
        );

        // And the on-disk state agrees.
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = after["models"]["providers"]["lmstudio"]["models"]
            .as_array()
            .unwrap();
        let by_id = |id: &str| arr.iter().find(|m| m["id"] == id).unwrap()["contextWindow"]
            .as_u64()
            .unwrap();
        assert_eq!(by_id("openai/gpt-oss-20b"), 100000);
        assert_eq!(by_id("darkmux:openai/gpt-oss-20b"), 100000);
        assert_eq!(by_id("unrelated/model"), 50000); // untouched
    }

    #[test]
    fn fix_ctx_window_is_idempotent_when_already_aligned() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            &tmp,
            r#"{
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                {"id": "openai/gpt-oss-20b", "contextWindow": 100000}
                            ]
                        }
                    }
                }
            }"#,
        );
        let loaded = vec![loaded_model("openai/gpt-oss-20b", 100000)];
        let changes = fix_ctx_window_to_loaded(&p, &loaded).unwrap();
        assert!(changes.is_empty(), "no changes when config already matches loaded ctx");
    }

    #[test]
    fn fix_ctx_window_errors_when_config_missing() {
        let tmp = TempDir::new().unwrap();
        let absent = tmp.path().join("does-not-exist.json");
        let loaded = vec![loaded_model("foo", 1000)];
        let err = fix_ctx_window_to_loaded(&absent, &loaded).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }
}
