//! (#661) Versioned config file — `~/.darkmux/config.json`.
//!
//! The canonical, `darkmux init`-written configuration surface. Every setting
//! resolves with precedence **`env > config.json > built-in default`** — see
//! [`crate::config_access`], the single place that precedence lives.
//!
//! This module owns only the *shape* + *load* of the file; the accessors
//! (which layer env over these fields over the built-in defaults) live in
//! `config_access`. A missing or malformed file is non-fatal — it loads as
//! the empty default and every accessor falls through to its env/built-in
//! tiers, so a bad config never bricks the CLI.
//!
//! **Carve-outs (NOT in this file by design):**
//! - the Redis **password** lives in the macOS Keychain, never plaintext —
//!   `RedisConfig` holds only non-secret connection bits.
//! - the config-file location is found via the `DARKMUX_HOME` bootstrap
//!   pointer + `paths::resolve` (`<root>/config.json`), not from inside the
//!   config itself.
//!
//! Schema shape mirrors `RuntimeCompactionConfig` (typed `Option`s +
//! `#[serde(flatten)] extras` for forward-compat overflow); see its
//! round-trip invariant tests for the pattern this file's tests copy.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Semver of the `config.json` shape. Additive field/section adds are a
/// **minor** bump (older binaries safely ignore unknown keys via `extras`);
/// renaming/retyping a field is **major**. Mirrors the `FLOW_SCHEMA_VERSION`
/// discipline (`crates/darkmux-flow/src/schema.rs`).
pub const CONFIG_SCHEMA_VERSION: &str = "1.0";

/// The `~/.darkmux/config.json` document. All fields optional + skipped when
/// `None`, so a fresh/empty config serializes to `{}` and any field absent
/// from the file falls through to its env/built-in default at the accessor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DarkmuxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,

    // ── Provenance / identity ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,

    // ── External tooling ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lms_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmstudio_url: Option<String>,

    // ── Sections ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirs: Option<DirsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis: Option<RedisConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeBehaviorConfig>,

    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Directory/path overrides. Each layers `env(DARKMUX_*) > config.dirs.X >
/// the `DarkmuxPaths` built-in` at the accessor (path unification lands in
/// #661 Slice 3). Values support `~` expansion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub flows: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub audit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub notebook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub crew: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub templates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub runtime_agents: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub fleet_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub openclaw_config: Option<String>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Non-secret Redis connection bits. The **password is NEVER here** — it
/// lives in the macOS Keychain (item `darkmux-redis`), assembled at runtime
/// (#661 Slice 5). `DARKMUX_REDIS_URL` (full URL, password inline) still
/// wins as the env override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedisConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub db: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub maxlen: Option<usize>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Per-dispatch runtime behavior knobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeBehaviorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub inactivity_timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub strict_selection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub feedback_injection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub default_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub check_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub daemon_cors_origins: Option<String>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

impl DarkmuxConfig {
    /// Load the config.json at the resolved location (`<root>/config.json`
    /// via `paths::resolve`). Missing or malformed → default-empty (loud
    /// validation belongs to `darkmux doctor`, not the hot load path; a bad
    /// config must never brick the CLI — accessors fall through to env/
    /// built-in defaults).
    pub fn load_resolved() -> Self {
        let path = crate::paths::resolve(crate::paths::ResolveScope::Auto).config;
        Self::load_from(&path)
    }

    /// Load from an explicit path (used by tests + `load_resolved`). Silent
    /// default on missing/unreadable/unparseable file.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default serializes to `{}` and round-trips empty — the forward-compat
    /// guarantee (mirrors `runtime_compaction_config_default_round_trips_empty`).
    #[test]
    fn default_round_trips_empty() {
        let cfg = DarkmuxConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
        let back: DarkmuxConfig = serde_json::from_str(&json).unwrap();
        assert!(back.machine_id.is_none());
        assert!(back.redis.is_none());
        assert!(back.dirs.is_none());
        assert!(back.runtime.is_none());
        assert!(back.extras.is_empty());
    }

    #[test]
    fn full_shape_round_trips() {
        let json = r#"{
            "schema_version": "1.0",
            "machine_id": "studio",
            "orchestrator": "claude-opus-4-8",
            "lms_bin": "/usr/local/bin/lms",
            "lmstudio_url": "http://localhost:1234",
            "dirs": { "flows": "~/dm/flows", "audit": "~/dm/audit" },
            "redis": { "host": "100.74.208.36", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
            "runtime": { "inactivity_timeout_seconds": 600, "max_turns": 40, "strict_selection": true }
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("studio"));
        assert_eq!(cfg.redis.as_ref().unwrap().host.as_deref(), Some("100.74.208.36"));
        assert_eq!(cfg.redis.as_ref().unwrap().port, Some(6379));
        assert_eq!(cfg.dirs.as_ref().unwrap().flows.as_deref(), Some("~/dm/flows"));
        assert_eq!(cfg.runtime.as_ref().unwrap().max_turns, Some(40));
        assert_eq!(cfg.runtime.as_ref().unwrap().strict_selection, Some(true));
        // Re-serialize → parse → still equal on the load-bearing fields.
        let round = serde_json::to_string(&cfg).unwrap();
        let back: DarkmuxConfig = serde_json::from_str(&round).unwrap();
        assert_eq!(back.machine_id, cfg.machine_id);
        assert_eq!(back.redis.as_ref().unwrap().port, Some(6379));
    }

    /// Unknown top-level keys land in `extras` and re-serialize flat (a newer
    /// config read by an older binary) — and the Redis section has NO
    /// password field, so a stray `password` key would land in `extras`, not
    /// a typed slot (the carve-out holds structurally).
    #[test]
    fn unknown_keys_land_in_extras_and_reserialize_flat() {
        let json = r#"{ "machine_id": "x", "future_knob": 7, "nested_future": {"a": 1} }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("x"));
        assert_eq!(cfg.extras.get("future_knob").and_then(|v| v.as_u64()), Some(7));
        let out: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        let obj = out.as_object().unwrap();
        assert!(!obj.contains_key("extras"), "extras must flatten, not nest");
        assert!(obj.contains_key("future_knob"), "unknown key re-serializes flat");
    }

    #[test]
    fn redis_password_is_not_a_typed_field() {
        // The carve-out, structurally: a config with a redis.password lands it
        // in the sub-struct's extras (forward-compat overflow), NOT a typed
        // slot darkmux reads — secrets never resolve from plaintext config.
        let json = r#"{ "redis": { "host": "h", "password": "leaked" } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        let redis = cfg.redis.unwrap();
        assert_eq!(redis.host.as_deref(), Some("h"));
        assert!(redis.extras.contains_key("password"), "password is overflow, not typed");
    }

    #[test]
    fn load_from_missing_file_is_default() {
        let cfg = DarkmuxConfig::load_from(Path::new("/nonexistent/darkmux/config.json"));
        assert!(cfg.machine_id.is_none());
        assert!(cfg.extras.is_empty());
    }

    #[test]
    fn load_from_malformed_file_is_default_not_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "{ not valid json").unwrap();
        let cfg = DarkmuxConfig::load_from(tmp.path());
        assert!(cfg.machine_id.is_none(), "malformed config falls back to default, never panics");
    }
}
