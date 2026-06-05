//! (#661) Config accessors — **THE single place setting-precedence lives**:
//! `env(DARKMUX_*) > config.json > built-in default`. Operator sovereignty
//! made structural: a reader never has to wonder where a setting came from.
//!
//! The config FILE is loaded lazily once (the `CONFIG` `OnceLock`, mirroring
//! the flow `SINK` pattern). The ENV layer is read **live per-accessor** (not
//! frozen at load) so a test `set_var` or a power-user export after first
//! load still wins — matching `resolve_machine_id`'s re-read-every-call
//! property the serial tests rely on.
//!
//! Slice 1 (#661) defines the clean-typed accessors + the precedence engine.
//! The dir accessors (path-unification, Slice 3), the boolean knobs with
//! site-specific truthy/falsy parsing (Slice 4), and `redis_url()` with the
//! Keychain split (Slice 5) land alongside their call-site migrations.

use crate::config::DarkmuxConfig;
use std::str::FromStr;
use std::sync::OnceLock;

static CONFIG: OnceLock<DarkmuxConfig> = OnceLock::new();

/// The loaded `config.json` (lazily, once). Malformed/missing → default-empty.
fn config() -> &'static DarkmuxConfig {
    CONFIG.get_or_init(DarkmuxConfig::load_resolved)
}

/// Read a non-empty, trimmed env var. The existing idiom (`paths.rs:76`).
pub(crate) fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Precedence for a string setting: `env > config.json field > built-in
/// default`. Pure + testable (the `cfg`/`default` are passed in, not read
/// from the global) so precedence is unit-tested without the load-once
/// `CONFIG`. An **empty/whitespace config string is treated as unset** (falls
/// through), mirroring `env_str` — so a visible-but-blank field like
/// `"orchestrator": ""` the operator hasn't filled in defers to the env/
/// built-in tier rather than stamping an empty value.
fn pick_string(env_key: &str, cfg: Option<&str>, default: Option<&str>) -> Option<String> {
    env_str(env_key)
        .or_else(|| cfg.filter(|s| !s.trim().is_empty()).map(str::to_string))
        .or_else(|| default.map(str::to_string))
}

/// Precedence for a parseable (numeric) setting. A set-but-unparseable env
/// var falls through (matching today's `.parse().ok()` sites).
fn pick_parsed<T: FromStr + Copy>(env_key: &str, cfg: Option<T>, default: Option<T>) -> Option<T> {
    env_str(env_key)
        .and_then(|s| s.parse::<T>().ok())
        .or(cfg)
        .or(default)
}

// ── Identity / provenance ──
/// `DARKMUX_MACHINE_ID > config.machine_id`. The hostname fallback stays in
/// `resolve_machine_id` (the write-time caller), so this returns `None` when
/// neither layer is set.
pub fn machine_id() -> Option<String> {
    pick_string("DARKMUX_MACHINE_ID", config().machine_id.as_deref(), None)
}
/// `DARKMUX_ORCHESTRATOR > config.orchestrator`. `None` → field omitted.
pub fn orchestrator() -> Option<String> {
    pick_string("DARKMUX_ORCHESTRATOR", config().orchestrator.as_deref(), None)
}

// ── External tooling ──
pub fn lms_bin() -> String {
    pick_string("DARKMUX_LMS_BIN", config().lms_bin.as_deref(), Some("lms")).unwrap()
}
/// The LMStudio **base** URL (`scheme://host:port`), default
/// `http://localhost:1234`. Callers append their endpoint path
/// (`/v1/models` for the runtime probe, `/v1/chat/completions` for sprint
/// narration).
///
/// **Slice-4 migration contract:** today `DARKMUX_LMSTUDIO_URL` is consumed
/// as the *full* chat-completions URL (`sprint_cli::lmstudio_chat_url`,
/// default `…/v1/chat/completions`). Moving to this base-URL accessor is a
/// clean pre-1.0 semantic break — the migrating caller MUST append its path
/// suffix (else it double-appends or 404s), and the runtime probe
/// (`dispatch_internal`, currently hardcoded `localhost:1234`) routes through
/// here too. Document the env var's new "base URL" meaning when it migrates.
pub fn lmstudio_url() -> String {
    pick_string("DARKMUX_LMSTUDIO_URL", config().lmstudio_url.as_deref(), Some("http://localhost:1234")).unwrap()
}

// ── Redis (non-secret bits; password + URL assembly land in Slice 5) ──
pub fn redis_stream() -> String {
    let cfg = config().redis.as_ref().and_then(|r| r.stream.as_deref());
    pick_string("DARKMUX_REDIS_STREAM", cfg, Some("darkmux:flow")).unwrap()
}
/// Redis stream retention for `XADD MAXLEN ~ N`. `0` carries the operator's
/// "unbounded" intent — the `0 → None` translation the XADD path needs stays
/// at the flow call site (Slice 5); this is a plain value provider.
pub fn redis_maxlen() -> usize {
    let cfg = config().redis.as_ref().and_then(|r| r.maxlen);
    pick_parsed("DARKMUX_REDIS_MAXLEN", cfg, Some(10_000)).unwrap()
}

// ── Runtime behavior ──
pub fn inactivity_timeout_seconds() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.inactivity_timeout_seconds);
    pick_parsed("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", cfg, Some(600)).unwrap()
}
pub fn max_turns() -> Option<u32> {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_turns);
    pick_parsed("DARKMUX_RUNTIME_MAX_TURNS", cfg, None)
}
pub fn max_tokens() -> Option<u32> {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_tokens);
    pick_parsed("DARKMUX_RUNTIME_MAX_TOKENS", cfg, None)
}
pub fn default_role() -> Option<String> {
    let cfg = config().runtime.as_ref().and_then(|r| r.default_role.as_deref());
    pick_string("DARKMUX_DEFAULT_ROLE", cfg, None)
}
pub fn daemon_cors_origins() -> Option<String> {
    let cfg = config().runtime.as_ref().and_then(|r| r.daemon_cors_origins.as_deref());
    pick_string("DARKMUX_DAEMON_CORS_ORIGINS", cfg, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_str_skips_empty_and_unset() {
        // unset
        assert!(env_str("DARKMUX_DEFINITELY_UNSET_XYZ").is_none());
    }

    // ── pick_string: env > cfg > default ──
    #[serial_test::serial]
    #[test]
    fn pick_string_precedence() {
        let k = "DARKMUX_TEST_PICK_STRING";
        unsafe { std::env::remove_var(k); }
        // default only
        assert_eq!(pick_string(k, None, Some("d")), Some("d".to_string()));
        // cfg beats default
        assert_eq!(pick_string(k, Some("c"), Some("d")), Some("c".to_string()));
        // env beats cfg
        unsafe { std::env::set_var(k, "e"); }
        assert_eq!(pick_string(k, Some("c"), Some("d")), Some("e".to_string()));
        // empty env is ignored (falls through to cfg)
        unsafe { std::env::set_var(k, "   "); }
        assert_eq!(pick_string(k, Some("c"), Some("d")), Some("c".to_string()));
        unsafe { std::env::remove_var(k); }
        // empty/whitespace cfg is treated as unset (falls through) — the
        // "visible but blank" field (`"orchestrator": ""`) defers to default.
        assert_eq!(pick_string(k, Some("   "), Some("d")), Some("d".to_string()));
        assert_eq!(pick_string(k, Some(""), None), None);
        // nothing set anywhere
        assert_eq!(pick_string(k, None, None), None);
    }

    // ── pick_parsed: env > cfg > default, unparseable env falls through ──
    #[serial_test::serial]
    #[test]
    fn pick_parsed_precedence_and_unparseable() {
        let k = "DARKMUX_TEST_PICK_PARSED";
        unsafe { std::env::remove_var(k); }
        assert_eq!(pick_parsed::<u64>(k, None, Some(600)), Some(600));
        assert_eq!(pick_parsed::<u64>(k, Some(120), Some(600)), Some(120)); // cfg beats default
        unsafe { std::env::set_var(k, "90"); }
        assert_eq!(pick_parsed::<u64>(k, Some(120), Some(600)), Some(90));  // env beats cfg
        unsafe { std::env::set_var(k, "not-a-number"); }
        assert_eq!(pick_parsed::<u64>(k, Some(120), Some(600)), Some(120)); // unparseable env → cfg
        unsafe { std::env::remove_var(k); }
    }

    // ── representative accessor honors the env layer live (the override
    //    property power-users + tests depend on) ──
    #[serial_test::serial]
    #[test]
    fn redis_stream_env_override_wins_live() {
        let prev = std::env::var("DARKMUX_REDIS_STREAM").ok();
        unsafe { std::env::set_var("DARKMUX_REDIS_STREAM", "darkmux:test-override"); }
        assert_eq!(redis_stream(), "darkmux:test-override");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_REDIS_STREAM", v),
                None => std::env::remove_var("DARKMUX_REDIS_STREAM"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn redis_stream_default_when_unset() {
        let prev = std::env::var("DARKMUX_REDIS_STREAM").ok();
        unsafe { std::env::remove_var("DARKMUX_REDIS_STREAM"); }
        // With no env and (in CI) no config.json, the built-in default holds.
        assert_eq!(redis_stream(), "darkmux:flow");
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_REDIS_STREAM", v); } }
    }
}
