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

/// Read an env var, **trimmed**, returning `None` when unset or
/// empty/whitespace-only. Trimming (not just empty-filtering) matches the
/// prior per-call-site behavior several resolvers had (`PathBuf::from(p.trim())`)
/// and is strictly forgiving everywhere this feeds — paths, ids, and numeric
/// parses all want surrounding whitespace gone. The single env-read idiom.
pub(crate) fn env_str(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// The **override tier** for a directory setting: `env > config tier
/// (tilde-expanded)`, or `None` when neither is set. The caller then supplies
/// its own default — used where one env var overrides two *different* derived
/// defaults (e.g. `DARKMUX_CREW_DIR` overrides both the crew root and the
/// user-state root). `env` is the already-empty-filtered `env_str` output, used
/// raw (the shell expands `~`); the config tier is tilde-expanded (operators
/// hand-write `~/...`) and an empty/whitespace value falls through. Pure +
/// testable — the reusable spine of every dir accessor (#661 Slice 3).
fn pick_dir_override(env: Option<String>, cfg: Option<&str>) -> Option<std::path::PathBuf> {
    if let Some(s) = env {
        return Some(std::path::PathBuf::from(s));
    }
    cfg.filter(|s| !s.trim().is_empty())
        .map(crate::paths::expand_tilde)
}

/// Precedence for a **directory** setting: `env > config tier (tilde-expanded) > built-in default`.
/// `pick_dir_override` plus a lazy default closure (some dirs derive their
/// default from HOME/root). Pure + testable.
fn pick_dir(
    env: Option<String>,
    cfg: Option<&str>,
    default: impl FnOnce() -> std::path::PathBuf,
) -> std::path::PathBuf {
    pick_dir_override(env, cfg).unwrap_or_else(default)
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
/// The LMStudio **base** URL (`scheme://host:port`), resolving
/// `env(DARKMUX_LMSTUDIO_URL) > config.lmstudio_url > http://localhost:1234`.
/// Callers append their endpoint path: `/v1/chat/completions`
/// (`sprint_cli::lmstudio_chat_url`) and `/v1/models` (the `dispatch_internal`
/// model probe).
///
/// (#661 Slice 4) `DARKMUX_LMSTUDIO_URL` is the **base** URL — a clean pre-1.0
/// break from its prior "full chat-completions URL" meaning, so the chat
/// narrator + the probe share one config value, each appending its own path.
pub fn lmstudio_url() -> String {
    // Trim a trailing `/` so a caller's `/v1/...` suffix can't double up — an
    // operator base of `http://host:1234/` is a common slip.
    pick_string("DARKMUX_LMSTUDIO_URL", config().lmstudio_url.as_deref(), Some("http://localhost:1234"))
        .unwrap()
        .trim_end_matches('/')
        .to_string()
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

// The non-secret connection bits for the config-assembled Redis URL (#661
// Slice 5). CONFIG-ONLY — there is no per-field env var; the env path to
// configure Redis is the full `DARKMUX_REDIS_URL` (tier-1 of `flow::redis_url`).
// The password is NEVER here — it comes from the macOS Keychain.

/// The Redis feature gate (`config.redis.enabled`). `false`/absent → no
/// config-assembled Redis (the env `DARKMUX_REDIS_URL` can still enable it).
pub fn redis_enabled() -> bool {
    config().redis.as_ref().and_then(|r| r.enabled).unwrap_or(false)
}
/// Redis host (`config.redis.host`). `None` → nothing to assemble.
pub fn redis_host() -> Option<String> {
    config()
        .redis
        .as_ref()
        .and_then(|r| r.host.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}
/// Redis port (`config.redis.port`), default `6379`.
pub fn redis_port() -> u16 {
    config().redis.as_ref().and_then(|r| r.port).unwrap_or(6379)
}
/// Redis logical DB index (`config.redis.db`). `None` → omit the `/<db>` suffix.
pub fn redis_db() -> Option<u8> {
    config().redis.as_ref().and_then(|r| r.db)
}

// ── Audit (hash-chained sink #163; feature block gated by `enabled`) ──
/// Audit-dir OVERRIDE: `env(DARKMUX_AUDIT_DIR) > config.audit.dir`
/// (tilde-expanded), or `None` when neither is set — the caller applies its
/// `~/.darkmux/audit` default. Mirrors the other dir-override accessors so an
/// operator who sets `audit.dir` in `config.json` is honored, not ignored.
pub fn audit_dir_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_AUDIT_DIR"),
        config().audit.as_ref().and_then(|a| a.dir.as_deref()),
    )
}
/// Whether the AuditFileSink is enabled, per the documented precedence
/// `env(DARKMUX_AUDIT_DIR) > config.audit.enabled`: the historical
/// enable-by-presence of `DARKMUX_AUDIT_DIR`, OR `config.audit.enabled`. There
/// is deliberately no `DARKMUX_AUDIT_ENABLED` env var — the env path to enable
/// audit is setting the dir (preserves the pre-config behavior).
pub fn audit_enabled() -> bool {
    env_str("DARKMUX_AUDIT_DIR").is_some()
        || config().audit.as_ref().and_then(|a| a.enabled).unwrap_or(false)
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
/// Strict model-selection (hard-fail on profile-vs-loaded mismatch).
/// `env(DARKMUX_STRICT_SELECTION)` truthy (`1`/`true`/`yes`/`on`, case-
/// insensitive) > `config.runtime.strict_selection` > `false`. The env layer is
/// a *string* parsed per this var's truthy set (config is already a typed bool).
pub fn strict_selection() -> bool {
    if let Some(s) = env_str("DARKMUX_STRICT_SELECTION") {
        return matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config().runtime.as_ref().and_then(|r| r.strict_selection).unwrap_or(false)
}
/// Whether `darkmux doctor` checks for a newer release. An **opt-out**:
/// `env(DARKMUX_CHECK_UPDATES)` falsy (`0`/`false`/`no`) disables >
/// `config.runtime.check_updates` > `true` (default on). Env match is
/// case-sensitive (preserving the prior behavior); `env_str` trims surrounding
/// whitespace.
pub fn check_updates() -> bool {
    if let Some(s) = env_str("DARKMUX_CHECK_UPDATES") {
        return !matches!(s.as_str(), "0" | "false" | "no");
    }
    config().runtime.as_ref().and_then(|r| r.check_updates).unwrap_or(true)
}

// ── Directories (#661 Slice 3) ──
// Dir accessors layer `env(DARKMUX_*_DIR) > config.dirs.X > built-in default`.
// The env tier preserves today's exact behavior; the config tier (tilde-
// expanded — operators hand-write `~/...`) is the new override. Each accessor
// owns its dir's full precedence so the resolution lives in ONE place.

/// The flows directory (the always-on LocalFileSink target):
/// `env(DARKMUX_FLOWS_DIR) > config.dirs.flows > ~/.darkmux/flows >
/// /tmp/darkmux/flows`. The last is the HOME-less CI/sandbox fallback (parity
/// with the prior `flow::schema::flows_dir`, which this now backs).
pub fn flows_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    pick_dir(
        env_str("DARKMUX_FLOWS_DIR"),
        config().dirs.as_ref().and_then(|d| d.flows.as_deref()),
        || {
            dirs::home_dir()
                .map(|h| h.join(".darkmux").join("flows"))
                .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/flows"))
        },
    )
}

/// (#703) Host cache dir for the extracted static `darkmux-runtime` binary,
/// bind-mounted into operator-named images (`crew dispatch --image <tag>`)
/// so darkmux can inject its agent into ANY Linux image rather than ship a
/// per-language image catalog. `~/.darkmux/runtime` (HOME-less fallback
/// `/tmp/darkmux/runtime`). Internal cache — no env/config override tier.
pub fn runtime_cache_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    dirs::home_dir()
        .map(|h| h.join(".darkmux").join("runtime"))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/runtime"))
}

/// (#703 Slice 3) Host dir for the shared toolchain build/download cache,
/// bind-mounted into every dispatch at `/darkmux-cache` so the inner verify
/// loop doesn't re-download deps each run (cargo registry, npm, pip). The
/// registry/download caches are concurrency-safe; per-dispatch `target/` stays
/// in the workspace (so concurrent dispatches don't contend). `~/.darkmux/cache`
/// (HOME-less fallback `/tmp/darkmux/cache`). Internal cache — no override tier.
pub fn cache_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    dirs::home_dir()
        .map(|h| h.join(".darkmux").join("cache"))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/cache"))
}

/// The crew-state directory **override** (`env(DARKMUX_CREW_DIR) >
/// config.dirs.crew`), or `None` when neither is set. Returns the override only
/// — the env var points at the directory *containing* the crew subdirs, and it
/// overrides two distinct derived defaults (the crew root `<root>/crew` and the
/// user-state root `<root>`), so each caller in `darkmux-crew` applies its own
/// (`crew_root` / `user_state_root`).
pub fn crew_dir_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_CREW_DIR"),
        config().dirs.as_ref().and_then(|d| d.crew.as_deref()),
    )
}

/// The fleet roster file: `env(DARKMUX_FLEET_FILE) > config.dirs.fleet_file >
/// ~/.darkmux/fleet.json` (with a `.darkmux/fleet.json` HOME-less fallback).
/// Backs `fleet::roster::roster_path`.
pub fn fleet_file() -> std::path::PathBuf {
    use std::path::PathBuf;
    pick_dir(
        env_str("DARKMUX_FLEET_FILE"),
        config().dirs.as_ref().and_then(|d| d.fleet_file.as_deref()),
        || {
            dirs::home_dir()
                .map(|h| h.join(".darkmux").join("fleet.json"))
                .unwrap_or_else(|| PathBuf::from(".darkmux/fleet.json"))
        },
    )
}

// The next four are **override-only** (`env > config.dirs.X`, else `None`):
// each caller keeps its own no-HOME default/error handling (some return
// `Option`, some `Result`, some `~/.darkmux/...` vs `~/.openclaw/...`), so the
// accessor yields just the override and the caller applies its default.

/// openclaw config-file override (`env(DARKMUX_OPENCLAW_CONFIG) >
/// config.dirs.openclaw_config`). Callers default to `~/.openclaw/openclaw.json`
/// (the swap patcher → `None` on no HOME; the dispatch path → `./.openclaw/...`).
pub fn openclaw_config_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_OPENCLAW_CONFIG"),
        config().dirs.as_ref().and_then(|d| d.openclaw_config.as_deref()),
    )
}

/// Operator-identity file override (`env(DARKMUX_IDENTITY_PATH) >
/// config.dirs.identity`). Caller defaults to `~/.darkmux/identity.md`.
pub fn identity_path_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_IDENTITY_PATH"),
        config().dirs.as_ref().and_then(|d| d.identity.as_deref()),
    )
}

/// Acknowledgment-files dir override (`env(DARKMUX_ACK_DIR) > config.dirs.ack`).
/// Caller defaults to `~/.darkmux/acks`.
pub fn ack_dir_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_ACK_DIR"),
        config().dirs.as_ref().and_then(|d| d.ack.as_deref()),
    )
}

/// Agent-runtime trajectories dir override (`env(DARKMUX_RUNTIME_AGENTS_DIR) >
/// config.dirs.runtime_agents`). Caller defaults to `~/.openclaw/agents`. NOTE:
/// the prior env read was not empty-filtered; routing through `env_str` makes
/// an empty value fall through (a strict improvement — an empty path is bogus).
pub fn runtime_agents_dir_override() -> Option<std::path::PathBuf> {
    pick_dir_override(
        env_str("DARKMUX_RUNTIME_AGENTS_DIR"),
        config().dirs.as_ref().and_then(|d| d.runtime_agents.as_deref()),
    )
}

/// The operator-override candidates for a **search-path** dir (templates,
/// skills) — `env` first, then the config tier, each highest-priority entries a
/// search caller prepends to its built-in candidate list (cwd, ~/.darkmux/…,
/// /usr/local/…). Unlike the single-value override accessors, BOTH tiers are
/// returned (in precedence order) since a search path layers candidates rather
/// than picking one. Empty when neither is set. Env is raw (shell-expanded);
/// config is tilde-expanded; empty/whitespace values fall through.
fn override_dirs(env_value: Option<String>, cfg: Option<&str>) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(s) = env_value {
        v.push(std::path::PathBuf::from(s));
    }
    if let Some(s) = cfg.filter(|s| !s.trim().is_empty()) {
        v.push(crate::paths::expand_tilde(s));
    }
    v
}

/// Workload-templates override candidates (`env(DARKMUX_TEMPLATES_DIR)` then
/// `config.dirs.templates`). The caller (`lab::workloads::load::builtin_dirs`)
/// joins `workloads/` and prepends these ahead of cwd/home/system candidates.
pub fn templates_override_dirs() -> Vec<std::path::PathBuf> {
    override_dirs(
        env_str("DARKMUX_TEMPLATES_DIR"),
        config().dirs.as_ref().and_then(|d| d.templates.as_deref()),
    )
}

/// Skills-source override candidates (`env(DARKMUX_SKILLS_DIR)` then
/// `config.dirs.skills`). The caller (`skills::locate_on_disk_skills_source`)
/// prepends these ahead of cwd/home/system candidates.
pub fn skills_override_dirs() -> Vec<std::path::PathBuf> {
    override_dirs(
        env_str("DARKMUX_SKILLS_DIR"),
        config().dirs.as_ref().and_then(|d| d.skills.as_deref()),
    )
}

/// The notebook directory: `env(DARKMUX_NOTEBOOK_DIR) > config.dirs.notebook >
/// <root>/notebook`. UNLIKE the other dir accessors, the env value is
/// **tilde-expanded** — operators write `~/Library/Mobile Documents/...` in the
/// shell to point the notebook at an iCloud-synced path — preserving
/// `paths_from_root`'s long-standing behavior, which this layers the config
/// tier over. The `<root>/notebook` fallback routes back through
/// `paths::resolve` (which also honors the env, redundantly + harmlessly, since
/// env already won above when set).
pub fn notebook_dir() -> std::path::PathBuf {
    if let Some(s) = env_str("DARKMUX_NOTEBOOK_DIR") {
        return crate::paths::expand_tilde(&s);
    }
    if let Some(s) = config()
        .dirs
        .as_ref()
        .and_then(|d| d.notebook.as_deref())
        .filter(|s| !s.trim().is_empty())
    {
        return crate::paths::expand_tilde(s);
    }
    crate::paths::resolve(crate::paths::ResolveScope::Auto).notebook
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_str_skips_empty_and_unset() {
        // unset
        assert!(env_str("DARKMUX_DEFINITELY_UNSET_XYZ").is_none());
    }

    #[serial_test::serial]
    #[test]
    fn audit_env_dir_forces_enabled_and_overrides() {
        // (#875) Env-tier behavior of the new audit accessors. The config tier
        // is exercised by `pick_dir_override`'s own tests; here we pin the
        // robust env-forces directions (no `config()` dependence in the asserts).
        let k = "DARKMUX_AUDIT_DIR";
        let prev = std::env::var(k).ok();
        unsafe { std::env::set_var(k, "/tmp/dm-audit-test"); }
        assert!(audit_enabled(), "env DARKMUX_AUDIT_DIR presence enables audit");
        assert_eq!(
            audit_dir_override(),
            Some(std::path::PathBuf::from("/tmp/dm-audit-test")),
            "env value wins the audit-dir override tier"
        );
        // A blank env value is treated as unset by env_str → never the dir.
        unsafe { std::env::set_var(k, "   "); }
        assert_ne!(
            audit_dir_override(),
            Some(std::path::PathBuf::from("   ")),
            "blank env must not become the audit dir"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }

    #[serial_test::serial]
    #[test]
    fn env_str_trims_value() {
        let k = "DARKMUX_TEST_ENV_TRIM";
        unsafe { std::env::set_var(k, "  /padded/path  "); }
        assert_eq!(env_str(k).as_deref(), Some("/padded/path"), "surrounding whitespace trimmed");
        unsafe { std::env::set_var(k, "   "); }
        assert_eq!(env_str(k), None, "whitespace-only → None");
        unsafe { std::env::remove_var(k); }
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

    // ── pick_dir: env > cfg (tilde-expanded) > default — the dir spine ──
    #[test]
    fn pick_dir_precedence_and_tilde() {
        use std::path::PathBuf;
        let home = dirs::home_dir().expect("home dir");
        // default fires when nothing is set
        assert_eq!(pick_dir(None, None, || PathBuf::from("/d")), PathBuf::from("/d"));
        // cfg beats default + is tilde-expanded
        assert_eq!(pick_dir(None, Some("~/cfg"), || PathBuf::from("/d")), home.join("cfg"));
        // empty/whitespace cfg falls through to default
        assert_eq!(pick_dir(None, Some("   "), || PathBuf::from("/d")), PathBuf::from("/d"));
        // env beats cfg and is used RAW (the shell already expanded ~)
        assert_eq!(
            pick_dir(Some("/env".to_string()), Some("~/cfg"), || PathBuf::from("/d")),
            PathBuf::from("/env")
        );
    }

    #[serial_test::serial]
    #[test]
    fn flows_dir_env_override_wins() {
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", "/custom/flows"); }
        assert_eq!(flows_dir(), std::path::PathBuf::from("/custom/flows"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn flows_dir_default_when_unset() {
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::remove_var("DARKMUX_FLOWS_DIR"); }
        // No env, and (in CI) no config.json → ends in a `flows` dir (the
        // ~/.darkmux/flows default, or the /tmp fallback if HOME is absent).
        assert!(flows_dir().ends_with("flows"), "resolves to a flows dir");
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", v); } }
    }

    #[serial_test::serial]
    #[test]
    fn crew_dir_override_env_then_none() {
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", "/custom/crew"); }
        assert_eq!(crew_dir_override(), Some(std::path::PathBuf::from("/custom/crew")));
        // No env, and (in CI) no config → no override; the caller supplies its
        // own default (crew root vs user-state root).
        unsafe { std::env::remove_var("DARKMUX_CREW_DIR"); }
        assert_eq!(crew_dir_override(), None);
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_CREW_DIR", v); } }
    }

    #[serial_test::serial]
    #[test]
    fn fleet_file_env_override_and_default() {
        let prev = std::env::var("DARKMUX_FLEET_FILE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", "/custom/fleet.json"); }
        assert_eq!(fleet_file(), std::path::PathBuf::from("/custom/fleet.json"));
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE"); }
        assert!(fleet_file().ends_with("fleet.json"), "default ends in fleet.json");
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_FLEET_FILE", v); } }
    }

    #[serial_test::serial]
    #[test]
    fn override_only_dir_accessors_env_then_none() {
        type Acc = fn() -> Option<std::path::PathBuf>;
        for (key, accessor) in [
            ("DARKMUX_OPENCLAW_CONFIG", openclaw_config_override as Acc),
            ("DARKMUX_IDENTITY_PATH", identity_path_override),
            ("DARKMUX_ACK_DIR", ack_dir_override),
            ("DARKMUX_RUNTIME_AGENTS_DIR", runtime_agents_dir_override),
        ] {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, "/custom/x"); }
            assert_eq!(accessor(), Some(std::path::PathBuf::from("/custom/x")), "{key} env override");
            // unset → None; each caller then applies its own default (the no-HOME
            // handling differs per dir, which is why these are override-only).
            unsafe { std::env::remove_var(key); }
            assert_eq!(accessor(), None, "{key} unset → None");
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    // ── override_dirs: [env (raw), config (tilde-expanded)] in precedence order ──
    #[test]
    fn override_dirs_orders_env_before_config_and_tilde_expands() {
        use std::path::PathBuf;
        let home = dirs::home_dir().expect("home dir");
        // both → env first (raw), then config (tilde-expanded)
        assert_eq!(
            override_dirs(Some("/env".to_string()), Some("~/cfg")),
            vec![PathBuf::from("/env"), home.join("cfg")]
        );
        // config only
        assert_eq!(override_dirs(None, Some("~/cfg")), vec![home.join("cfg")]);
        // empty/whitespace config falls through
        assert_eq!(override_dirs(None, Some("  ")), Vec::<PathBuf>::new());
        // neither set → no override candidates (caller uses its built-ins)
        assert_eq!(override_dirs(None, None), Vec::<PathBuf>::new());
    }

    #[serial_test::serial]
    #[test]
    fn search_path_override_dirs_env_then_empty() {
        type Acc = fn() -> Vec<std::path::PathBuf>;
        for (key, accessor) in [
            ("DARKMUX_TEMPLATES_DIR", templates_override_dirs as Acc),
            ("DARKMUX_SKILLS_DIR", skills_override_dirs),
        ] {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, "/custom/x"); }
            assert_eq!(accessor(), vec![std::path::PathBuf::from("/custom/x")], "{key} env candidate");
            unsafe { std::env::remove_var(key); }
            assert!(accessor().is_empty(), "{key} unset → no override candidates");
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn notebook_dir_env_is_tilde_expanded_then_default() {
        let prev = std::env::var("DARKMUX_NOTEBOOK_DIR").ok();
        // The notebook env value IS tilde-expanded (the documented iCloud-path
        // ergonomics) — unlike the other dir accessors, whose env is raw.
        unsafe { std::env::set_var("DARKMUX_NOTEBOOK_DIR", "~/nb"); }
        assert_eq!(notebook_dir(), dirs::home_dir().expect("home").join("nb"));
        unsafe { std::env::remove_var("DARKMUX_NOTEBOOK_DIR"); }
        // No env/config → the `<root>/notebook` derived default.
        assert!(notebook_dir().ends_with("notebook"));
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_NOTEBOOK_DIR", v); } }
    }

    #[serial_test::serial]
    #[test]
    fn strict_selection_env_truthy_then_default_false() {
        let prev = std::env::var("DARKMUX_STRICT_SELECTION").ok();
        for truthy in ["1", "true", "YES", "On"] {
            unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", truthy); }
            assert!(strict_selection(), "{truthy} → true (case-insensitive)");
        }
        unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", "nope"); }
        assert!(!strict_selection(), "non-truthy → false");
        unsafe { std::env::remove_var("DARKMUX_STRICT_SELECTION"); }
        assert!(!strict_selection(), "unset → false default");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_STRICT_SELECTION", v),
                None => std::env::remove_var("DARKMUX_STRICT_SELECTION"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_updates_env_optout_then_default_on() {
        let prev = std::env::var("DARKMUX_CHECK_UPDATES").ok();
        for off in ["0", "false", "no"] {
            unsafe { std::env::set_var("DARKMUX_CHECK_UPDATES", off); }
            assert!(!check_updates(), "{off} → disabled");
        }
        unsafe { std::env::set_var("DARKMUX_CHECK_UPDATES", "1"); }
        assert!(check_updates(), "non-falsy value → on");
        unsafe { std::env::remove_var("DARKMUX_CHECK_UPDATES"); }
        assert!(check_updates(), "unset → on (default)");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CHECK_UPDATES", v),
                None => std::env::remove_var("DARKMUX_CHECK_UPDATES"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn lmstudio_url_is_base_and_trims_trailing_slash() {
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        // A trailing-slash base is trimmed so callers' `/v1/...` can't double up.
        unsafe { std::env::set_var("DARKMUX_LMSTUDIO_URL", "http://host:1234/"); }
        assert_eq!(lmstudio_url(), "http://host:1234");
        // Default (no env/config) is the bare base, no trailing slash.
        unsafe { std::env::remove_var("DARKMUX_LMSTUDIO_URL"); }
        assert_eq!(lmstudio_url(), "http://localhost:1234");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
    }
}
