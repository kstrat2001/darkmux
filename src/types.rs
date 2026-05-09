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
    pub compaction: Option<serde_json::Map<String, serde_json::Value>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
