use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    Primary,
    Compactor,
    Auxiliary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileModel {
    pub id: String,
    pub n_ctx: u32,
    pub role: ModelRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileRuntime {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "configPath")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "contextTokens")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<RuntimeCompactionConfig>,
}

/// Tier-1 access-pattern eviction config. Future Step 3 of #352 epic
/// reads `eviction_after_unreferenced_turns` to decide when a tool
/// result can be dropped from context without invoking the compactor
/// model. v0.1 ships the field shape only; the consumer is added in
/// Step 3 (#352 sub-issue list).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tier1Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eviction_after_unreferenced_turns: Option<u32>,
}

/// Tier-2 structured-slot fact extraction config. Future Step 4 of #352
/// reads `schema_version` + `slot_caps` to size the compacted
/// working-memory artifact. Per-slot caps are soft limits in
/// characters; the compactor truncates if a slot exceeds its cap.
///
/// `slot_caps` defaults to the v0.1 commitment table (see
/// `RuntimeCompactionConfig::default_slot_caps`); operator-supplied
/// entries override matching defaults at consume-time. Unknown slot
/// names are accepted (forward-compat for per-role schema extensions
/// in v0.2+).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tier2Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_caps: BTreeMap<String, u32>,
}

/// Reserve doctrine: bail-and-reframe thresholds. When neither tier-1
/// nor tier-2 keeps the dispatch viable, the agent emits partial state
/// plus a reframe marker rather than compressing further. Step 6 of
/// #352 wires up the consumer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReserveConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_token_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_compactions: Option<u32>,
}

/// Profile-level compaction config — typed surface for the v0.1
/// commitments (see #354) plus an `extras` overflow that carries
/// openclaw-shape passthrough fields (`model`, `mode`,
/// `customInstructions`, `maxHistoryShare`, `recentTurnsPreserve`)
/// and any forward-compat fields darkmux doesn't yet recognize.
///
/// **Wire-format invariant**: serializes to the same JSON shape as
/// the pre-typed `serde_json::Map<String, serde_json::Value>` it
/// replaced — typed fields and `extras` keys appear at the same nest
/// level. Existing `profiles.json` files parse without migration.
///
/// All v0.1 fields are `Option` so missing-fields paths preserve
/// current behavior: when `strategy` is absent the runtime uses
/// today's middle-replace shape; when `tier1`/`tier2`/`reserve` are
/// absent their future consumers see "no config" and skip the new
/// behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCompactionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1: Option<Tier1Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2: Option<Tier2Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve: Option<ReserveConfig>,
    /// Openclaw-shape passthrough + forward-compat overflow. Read by
    /// existing consumers (`sprint_cli` reads `maxHistoryShare`;
    /// openclaw runtime config-patching reads `model` / `mode` /
    /// `customInstructions`). Future Step 7 of #352 will stamp the
    /// resolved config onto each dispatch's flow record; until then
    /// the field is operator-owned passthrough.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl RuntimeCompactionConfig {
    /// v0.1 default per-slot character caps per #354's commitment
    /// table. Operator-supplied entries in `tier2.slot_caps` override
    /// matching defaults at consume-time (this function returns the
    /// fallback set; merge with operator config in the consumer).
    ///
    /// No consumer in this PR — Step 4 of #352 (tier-2 structured-slot
    /// extraction) wires up the consumer that reads slot caps to size
    /// the compactor's output. The function ships in Step 2 so the
    /// v0.1 commitments table lives next to the schema it describes,
    /// rather than getting authored separately when Step 4 lands.
    #[allow(dead_code)]
    pub fn default_slot_caps() -> BTreeMap<String, u32> {
        let entries: &[(&str, u32)] = &[
            ("objective", 1024),
            ("current_truth.active_files", 4096),
            ("current_truth.test_outcomes", 2048),
            ("current_truth.external_state", 2048),
            ("completed_decisions", 4096),
            ("errors_to_preserve", 2048),
            ("next_concrete_actions", 1024),
            ("verify_criteria", 1024),
            ("sprint_id", 256),
        ];
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileHookCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryHooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_swap: Vec<ProfileHookCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_swap: Vec<ProfileHookCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub models: Vec<ProfileModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ProfileRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_when: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileRegistry {
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<RegistryHooks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
}

// `Serialize` so `darkmux serve`'s `/model/status` endpoint (#87) can
// return loaded-model state as JSON for the flow viewer's toolbar pill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadedModel {
    pub identifier: String,
    pub model: String,
    pub status: String,
    pub size: String,
    pub context: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_role_serializes_lowercase() {
        let role = ModelRole::Primary;
        assert_eq!(serde_json::to_string(&role).unwrap(), "\"primary\"");
        let parsed: ModelRole = serde_json::from_str("\"compactor\"").unwrap();
        assert_eq!(parsed, ModelRole::Compactor);
    }

    #[test]
    fn profile_model_round_trips() {
        let m = ProfileModel {
            id: "x".to_string(),
            n_ctx: 32_000,
            role: ModelRole::Primary,
            identifier: Some("alias".to_string()),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProfileModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.n_ctx, 32_000);
        assert_eq!(back.role, ModelRole::Primary);
        assert_eq!(back.identifier.as_deref(), Some("alias"));
    }

    #[test]
    fn profile_model_omits_none_identifier() {
        let m = ProfileModel {
            id: "x".to_string(),
            n_ctx: 1024,
            role: ModelRole::Auxiliary,
            identifier: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("identifier"));
    }

    #[test]
    fn registry_parses_minimal_profile() {
        let json = r#"{
            "profiles": {
                "fast": {
                    "description": "tiny",
                    "models": [
                        {"id": "model-a", "n_ctx": 32000, "role": "primary"}
                    ]
                }
            }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.profiles.len(), 1);
        let p = reg.profiles.get("fast").unwrap();
        assert_eq!(p.models[0].n_ctx, 32_000);
        assert!(matches!(p.models[0].role, ModelRole::Primary));
    }

    // ─── RuntimeCompactionConfig (v0.1 schema extension, #357) ──────────

    /// All-default RuntimeCompactionConfig serializes to `{}` and
    /// round-trips back to the all-`None` shape — the pre-#357
    /// behavior preservation guarantee for profiles that don't opt
    /// into the new tier fields.
    #[test]
    fn runtime_compaction_config_default_round_trips_empty() {
        let cfg = RuntimeCompactionConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
        let back: RuntimeCompactionConfig = serde_json::from_str(&json).unwrap();
        assert!(back.strategy.is_none());
        assert!(back.threshold_tokens.is_none());
        assert!(back.tier1.is_none());
        assert!(back.tier2.is_none());
        assert!(back.reserve.is_none());
        assert!(back.extras.is_empty());
    }

    /// Full v0.1 shape round-trips cleanly — strategy, threshold,
    /// tier1/tier2/reserve nests, and operator-extensible slot_caps.
    #[test]
    fn runtime_compaction_config_v0_1_shape_round_trips() {
        let json = r#"{
            "strategy": "structured-slot",
            "threshold_tokens": 60000,
            "tier1": {"eviction_after_unreferenced_turns": 5},
            "tier2": {
                "schema_version": "0.1",
                "slot_caps": {"objective": 1024, "current_truth.active_files": 4096}
            },
            "reserve": {"bail_after_token_count": 95000, "bail_after_compactions": 3}
        }"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.strategy.as_deref(), Some("structured-slot"));
        assert_eq!(cfg.threshold_tokens, Some(60000));
        let t1 = cfg.tier1.as_ref().expect("tier1 set");
        assert_eq!(t1.eviction_after_unreferenced_turns, Some(5));
        let t2 = cfg.tier2.as_ref().expect("tier2 set");
        assert_eq!(t2.schema_version.as_deref(), Some("0.1"));
        assert_eq!(t2.slot_caps.get("objective"), Some(&1024));
        assert_eq!(t2.slot_caps.get("current_truth.active_files"), Some(&4096));
        let r = cfg.reserve.as_ref().expect("reserve set");
        assert_eq!(r.bail_after_token_count, Some(95000));
        assert_eq!(r.bail_after_compactions, Some(3));
        // Re-serialize and parse back to confirm full round-trip.
        let reserialized = serde_json::to_string(&cfg).unwrap();
        let back: RuntimeCompactionConfig = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(back.strategy, cfg.strategy);
        assert_eq!(back.tier2.as_ref().unwrap().slot_caps, t2.slot_caps);
    }

    /// Backward-compat invariant: openclaw-shape passthrough fields
    /// (`model`, `mode`, `customInstructions`, `maxHistoryShare`,
    /// `recentTurnsPreserve`) deserialize into `.extras` and
    /// re-serialize at the same nesting level as typed fields. A
    /// pre-#357 `profiles.json` parses unchanged.
    #[test]
    fn runtime_compaction_config_openclaw_passthrough_lands_in_extras() {
        let json = r#"{
            "mode": "default",
            "model": "lmstudio/qwen3-4b-instruct-2507",
            "customInstructions": "preserve test outcomes",
            "maxHistoryShare": 0.35,
            "recentTurnsPreserve": 5
        }"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        // No typed v0.1 field set; everything landed in extras.
        assert!(cfg.strategy.is_none());
        assert!(cfg.threshold_tokens.is_none());
        assert_eq!(
            cfg.extras.get("mode").and_then(|v| v.as_str()),
            Some("default")
        );
        assert!(cfg.extras.get("model").is_some());
        assert!(cfg.extras.get("customInstructions").is_some());
        assert_eq!(
            cfg.extras.get("maxHistoryShare").and_then(|v| v.as_f64()),
            Some(0.35)
        );
        assert_eq!(
            cfg.extras
                .get("recentTurnsPreserve")
                .and_then(|v| v.as_u64()),
            Some(5)
        );
        // Re-serializes as a flat object (no separate `extras` key).
        let out: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        let obj = out.as_object().expect("object shape");
        assert!(obj.contains_key("mode"));
        assert!(!obj.contains_key("extras"));
    }

    /// Mixed v0.1 typed fields + openclaw extras serialize to a
    /// single flat JSON object — the wire-format invariant that
    /// keeps `profiles.json` files diff-stable when an operator
    /// adds tier-2 fields next to existing openclaw passthrough.
    #[test]
    fn runtime_compaction_config_mixed_serializes_flat() {
        let json = r#"{
            "strategy": "structured-slot",
            "model": "lmstudio/qwen3-4b-instruct-2507",
            "threshold_tokens": 60000,
            "maxHistoryShare": 0.35
        }"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        let out: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        let obj = out.as_object().expect("object shape");
        assert!(obj.contains_key("strategy"));
        assert!(obj.contains_key("threshold_tokens"));
        assert!(obj.contains_key("model"));
        assert!(obj.contains_key("maxHistoryShare"));
        assert!(!obj.contains_key("extras"));
    }

    /// v0.1 default slot caps match the commitment table from #354.
    /// Operator-supplied slot names in `tier2.slot_caps` override
    /// these at consume-time (the helper returns the fallback set;
    /// merging is the consumer's responsibility).
    #[test]
    fn default_slot_caps_v0_1_table() {
        let caps = RuntimeCompactionConfig::default_slot_caps();
        assert_eq!(caps.len(), 9);
        assert_eq!(caps.get("objective"), Some(&1024));
        assert_eq!(caps.get("current_truth.active_files"), Some(&4096));
        assert_eq!(caps.get("current_truth.test_outcomes"), Some(&2048));
        assert_eq!(caps.get("current_truth.external_state"), Some(&2048));
        assert_eq!(caps.get("completed_decisions"), Some(&4096));
        assert_eq!(caps.get("errors_to_preserve"), Some(&2048));
        assert_eq!(caps.get("next_concrete_actions"), Some(&1024));
        assert_eq!(caps.get("verify_criteria"), Some(&1024));
        assert_eq!(caps.get("sprint_id"), Some(&256));
    }

    /// Operator-extensibility: arbitrary slot names (including ones
    /// reserved for v0.2+ per-role schema extensions) deserialize
    /// cleanly. No validation rejects unknown keys at the schema
    /// layer; that's the consumer's responsibility once Step 4 ships.
    #[test]
    fn tier2_slot_caps_accept_unknown_keys() {
        let json = r#"{
            "tier2": {"slot_caps": {"custom_coder_slot": 8192, "objective": 2048}}
        }"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        let t2 = cfg.tier2.as_ref().unwrap();
        assert_eq!(t2.slot_caps.get("custom_coder_slot"), Some(&8192));
        // Operator overrode the default for `objective`.
        assert_eq!(t2.slot_caps.get("objective"), Some(&2048));
    }

    /// Pre-#357 backward-compat: a JSON string written against the
    /// untyped-Map schema parses through the new typed schema and
    /// re-serializes byte-equivalent (as JSON `Value`). The hard-line
    /// invariant — every operator's `~/.darkmux/profiles.json` from
    /// before this PR re-emits unchanged after a darkmux upgrade.
    #[test]
    fn pre_357_json_round_trips_byte_equivalent() {
        let pre_357 = r#"{
            "mode": "default",
            "model": "lmstudio/qwen3-4b-instruct-2507",
            "customInstructions": "preserve test outcomes",
            "maxHistoryShare": 0.35,
            "recentTurnsPreserve": 5
        }"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(pre_357).unwrap();
        let original: serde_json::Value = serde_json::from_str(pre_357).unwrap();
        let reserialized = serde_json::to_value(&cfg).unwrap();
        assert_eq!(original, reserialized);
    }

    /// Full ProfileRegistry round-trip with the new typed compaction
    /// shape inside `runtime.compaction` — the integration check that
    /// the typed schema composes cleanly into the parent profile.
    #[test]
    fn registry_parses_profile_with_typed_compaction() {
        let json = r#"{
            "profiles": {
                "balanced": {
                    "models": [{"id": "primary", "n_ctx": 60000, "role": "primary"}],
                    "runtime": {
                        "compaction": {
                            "strategy": "structured-slot",
                            "threshold_tokens": 60000,
                            "tier1": {"eviction_after_unreferenced_turns": 5},
                            "model": "lmstudio/qwen3-4b-instruct-2507"
                        }
                    }
                }
            }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        let p = reg.profiles.get("balanced").unwrap();
        let rt = p.runtime.as_ref().unwrap();
        let comp = rt.compaction.as_ref().unwrap();
        assert_eq!(comp.strategy.as_deref(), Some("structured-slot"));
        assert_eq!(comp.threshold_tokens, Some(60000));
        assert_eq!(
            comp.tier1
                .as_ref()
                .unwrap()
                .eviction_after_unreferenced_turns,
            Some(5)
        );
        // Openclaw-shape `model` field still parses (via extras).
        assert!(comp.extras.contains_key("model"));
    }

    #[test]
    fn loaded_model_equality() {
        let a = LoadedModel {
            identifier: "x".into(),
            model: "x".into(),
            status: "idle".into(),
            size: "1G".into(),
            context: 1000,
        };
        let mut b = a.clone();
        assert_eq!(a, b);
        b.context = 2000;
        assert_ne!(a, b);
    }
}
