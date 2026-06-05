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
    pub audit: Option<AuditConfig>,
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

/// The Redis flow-coordination sink — a **feature block gated by `enabled`**,
/// not by field-presence. `darkmux init` writes the whole block with
/// `enabled: false` and every connection knob populated to its sensible
/// default, so the operator sees the full surface and turns it on by flipping
/// one field (the knobs are already there to tweak). The on/off *gating* wires
/// in #661 Slice 5; this is the visible schema + the written defaults.
///
/// The **password is NEVER here** — it lives in the macOS Keychain (item
/// `darkmux-redis`), assembled at runtime (Slice 5). `DARKMUX_REDIS_URL` (full
/// URL, password inline) still wins as the env override regardless of `enabled`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedisConfig {
    /// The gate: `true` → assemble + connect; `false`/absent → off (unless the
    /// `DARKMUX_REDIS_URL` env override is set). Declared first so it reads at
    /// the top of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub db: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub maxlen: Option<usize>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The hash-chained audit sink (#163) — a **feature block gated by `enabled`**,
/// same pattern as `RedisConfig`. `darkmux init` writes it with `enabled:
/// false` + the default `dir`. Today's env equivalent (`DARKMUX_AUDIT_DIR`
/// presence) still wins as the override; the config gating wires in #661.
/// POSIX-only sink (the env var is recognized but skipped on Windows).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditConfig {
    /// The gate: `true` → the AuditFileSink writes a hash-chained (BLAKE3)
    /// per-day JSONL that `darkmux flow integrity-check` walks to detect chain
    /// breaks; `false`/absent → off. Declared first.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub dir: Option<String>,
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
    /// The full, self-documenting default config that `darkmux init` writes —
    /// every common knob present and visible, so the operator tunes the *file*,
    /// not the code, and can *see* the surface without digging. Scalar defaults
    /// are written explicitly; the integration features (`redis`, `audit`) are
    /// written as complete blocks with `enabled: false`, so their whole surface
    /// is discoverable and one flip from on.
    ///
    /// Deliberately omitted — NOT hidden defaults, but fields where a written
    /// literal would be *wrong*:
    /// - `dirs` — defaults are derived from the root (`<root>/flows`); there is
    ///   no fixed literal to write without freezing the derivation. The
    ///   discovery surface is `darkmux doctor` (resolved path, overridable).
    /// - caps (`max_turns`/`max_tokens`), `default_role`, `daemon_cors_origins`
    ///   — absent is a real behavior (uncapped / none), not a value to default.
    /// - `orchestrator` is written as `""` (visible but unset; empty config
    ///   strings are treated as unset, so the per-session env override drives).
    ///
    /// Single source of truth for the written defaults: `init` writes this and
    /// `config.example.json` is asserted equal to its pretty form (a drift
    /// guard), so the docs reference and the code can't diverge. `machine_id`
    /// is a placeholder here — `init` overrides it with the machine's name.
    pub fn with_defaults() -> Self {
        DarkmuxConfig {
            schema_version: Some(CONFIG_SCHEMA_VERSION.to_string()),
            machine_id: Some("my-machine".to_string()),
            orchestrator: Some(String::new()),
            lms_bin: Some("lms".to_string()),
            lmstudio_url: Some("http://localhost:1234".to_string()),
            dirs: None,
            redis: Some(RedisConfig {
                enabled: Some(false),
                host: Some("127.0.0.1".to_string()),
                port: Some(6379),
                db: None,
                stream: Some("darkmux:flow".to_string()),
                maxlen: Some(10_000),
                extras: Default::default(),
            }),
            audit: Some(AuditConfig {
                enabled: Some(false),
                dir: Some("~/.darkmux/audit".to_string()),
                extras: Default::default(),
            }),
            runtime: Some(RuntimeBehaviorConfig {
                inactivity_timeout_seconds: Some(600),
                max_turns: None,
                max_tokens: None,
                strict_selection: Some(false),
                feedback_injection: Some(true),
                default_role: None,
                check_updates: Some(true),
                daemon_cors_origins: None,
                extras: Default::default(),
            }),
            extras: Default::default(),
        }
    }

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

    /// `with_defaults()` is the full, self-documenting config `init` writes:
    /// feature blocks present + gated off, scalar defaults explicit, derived/
    /// advanced fields absent, and `enabled` serialized first in each block.
    #[test]
    fn with_defaults_is_full_visible_and_round_trips() {
        let cfg = DarkmuxConfig::with_defaults();
        // Integration features: present as `enabled: false` blocks (visible
        // surface, off) — not absent, so the operator can see + flip them.
        let redis = cfg.redis.as_ref().unwrap();
        assert_eq!(redis.enabled, Some(false));
        assert_eq!(redis.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(redis.maxlen, Some(10_000));
        assert_eq!(cfg.audit.as_ref().unwrap().enabled, Some(false));
        // Scalar defaults written explicitly (not hidden in code).
        assert_eq!(cfg.lms_bin.as_deref(), Some("lms"));
        assert_eq!(cfg.runtime.as_ref().unwrap().feedback_injection, Some(true));
        assert_eq!(cfg.orchestrator.as_deref(), Some(""), "visible but unset");
        // Fields where a written literal would be wrong stay absent.
        assert!(cfg.dirs.is_none(), "dirs are derived → surfaced by doctor, not frozen");
        assert!(cfg.runtime.as_ref().unwrap().max_turns.is_none(), "uncapped, not defaulted");
        // `enabled` reads at the TOP of each feature block.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            json.find("\"enabled\"").unwrap() < json.find("\"host\"").unwrap(),
            "enabled must precede the connection knobs"
        );
        // Lossless round-trip.
        let back: DarkmuxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.redis.as_ref().unwrap().enabled, Some(false));
        assert_eq!(back.audit.as_ref().unwrap().dir.as_deref(), Some("~/.darkmux/audit"));
    }

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
