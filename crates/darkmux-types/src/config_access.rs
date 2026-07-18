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

// The loaded `config.json` (lazily, once) — production path only. Gated out of
// test / test-support builds, where `config()` is empty by construction (below).
#[cfg(not(any(test, feature = "test-support")))]
static CONFIG: OnceLock<DarkmuxConfig> = OnceLock::new();

// (#811) Test-isolation: the default-EMPTY config returned under test /
// test-support builds. Its own `OnceLock` so the `&'static` lifetime works
// without touching the production `CONFIG`.
#[cfg(any(test, feature = "test-support"))]
static EMPTY_CONFIG: OnceLock<DarkmuxConfig> = OnceLock::new();

/// The config tier of `env > config.json > default`.
///
/// **Production** (`cfg(not(test, test-support))`): the operator's real
/// `~/.darkmux/config.json`, loaded lazily once. Malformed/missing → default.
///
/// **Test / test-support** (`cfg(any(test, feature = "test-support"))`):
/// EMPTY by construction — `config()` never reads the operator's real
/// `~/.darkmux/config.json`. This is clean-by-construction test isolation
/// (#811): the config tier is a process-wide `OnceLock`, so a test could never
/// reliably control its *value* anyway, and a populated real config silently
/// flaked default-assertion tests (e.g. `redis.enabled: true` re-enabled the
/// Redis sink → test records XADD'd to the real `darkmux:flow` stream; a set
/// `dirs.notebook` beat the built-in default). Precedence is still fully tested
/// — `pick_*()` take explicit cfg args, and accessor tests assert the env tier
/// or the built-in default. A crate's whole test build opts in by enabling the
/// `darkmux-types/test-support` feature (a dev-dependency); no per-test call.
fn config() -> &'static DarkmuxConfig {
    #[cfg(any(test, feature = "test-support"))]
    return EMPTY_CONFIG.get_or_init(DarkmuxConfig::default);
    #[cfg(not(any(test, feature = "test-support")))]
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

// ── Fleet position (#933) ──
/// The machine's declared fleet position as the RAW operator token, resolving
/// `env(DARKMUX_FLEET_MODE) > config.fleet.mode > "standalone"`. An
/// unrecognized value passes through unchanged so `darkmux doctor` can validate
/// it against what the operator actually wrote (#934); typed callers use
/// `fleet_mode()`.
pub fn fleet_mode_raw() -> String {
    let cfg = config().fleet.as_ref().and_then(|f| f.mode.as_deref());
    pick_string("DARKMUX_FLEET_MODE", cfg, Some("standalone")).unwrap()
}

/// The machine's declared fleet position, typed. An unrecognized token resolves
/// to `Standalone` (the safe default — a single machine that coordinates
/// nothing); `darkmux doctor` surfaces the raw typo separately (#934).
pub fn fleet_mode() -> crate::config::FleetMode {
    crate::config::FleetMode::parse(&fleet_mode_raw()).unwrap_or_default()
}

// ── External tooling ──
pub fn lms_bin() -> String {
    pick_string("DARKMUX_LMS_BIN", config().lms_bin.as_deref(), Some("lms")).unwrap()
}
/// The LMStudio **base** URL (`scheme://host:port`), resolving
/// `env(DARKMUX_LMSTUDIO_URL) > config.lmstudio_url > http://localhost:1234`.
/// Callers append their endpoint path: `/v1/chat/completions`
/// (`phase_cli::lmstudio_chat_url`) and `/v1/models` (the `dispatch_internal`
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
/// (#1276) Bounded model-load/unload phase for gestalt host-port calls —
/// consumed by `darkmux_profiles::gestalt_host::resolved_load_deadline`,
/// which wraps it into the mandatory `Deadline` every `ModelHost` mutation
/// takes. Mirrors `inactivity_timeout_seconds`' wiring exactly.
pub fn model_load_timeout_seconds() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.model_load_timeout_seconds);
    pick_parsed("DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS", cfg, Some(600)).unwrap()
}
pub fn max_turns() -> Option<u32> {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_turns);
    pick_parsed("DARKMUX_RUNTIME_MAX_TURNS", cfg, None)
}
pub fn max_tokens() -> Option<u32> {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_tokens);
    pick_parsed("DARKMUX_RUNTIME_MAX_TOKENS", cfg, None)
}
/// (#1221) Per-call completion-token cap override. `None` = the runtime's
/// built-in default (`MAX_TOKENS_PER_CALL` = 10000).
pub fn max_tokens_per_call() -> Option<u32> {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_tokens_per_call);
    pick_parsed("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", cfg, None)
}

// ── Remote (hosted-endpoint) dispatch (#1260/#1177) ──
/// The per-EXECUTION remote token allowance — an execution is one pipeline
/// stage (the review pipeline's probe pass, each judge pass, the verify pass; a bare
/// dispatch is one execution). Only REMOTE (endpoint-staffed) calls
/// draw from it. Resolves `env(DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION) >
/// config.remote.max_tokens_per_execution > 500000` (operator decision on
/// #1260 — tokens only, never currency).
pub fn remote_max_tokens_per_execution() -> u64 {
    let cfg = config().remote.as_ref().and_then(|r| r.max_tokens_per_execution);
    pick_parsed("DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION", cfg, Some(500_000)).unwrap()
}
/// (#1230 Packet 1) Max CONCURRENT remote dispatches
/// `darkmux_crew::concurrent_dispatch::run_bounded` runs at once. Resolves
/// `env(DARKMUX_REMOTE_CONCURRENT_CAP) > config.remote.concurrent_cap > 4`
/// — mirrors `remote_max_tokens_per_execution`'s wiring exactly. A
/// placeholder default (see `RemoteConfig::concurrent_cap`'s doc), not yet
/// empirically tuned against real hosted-endpoint rate limits.
pub fn remote_concurrent_cap() -> u32 {
    let cfg = config().remote.as_ref().and_then(|r| r.concurrent_cap);
    pick_parsed("DARKMUX_REMOTE_CONCURRENT_CAP", cfg, Some(4)).unwrap()
}
// ── Review pipeline (#1349) ──
/// (#1349) The PR-review pipeline's judge step internal bounded-concurrency
/// for-each cap. Resolves `env(DARKMUX_REVIEW_JUDGE_CONCURRENCY) >
/// config.review.judge_concurrency > 1` (fully sequential) — mirrors
/// `remote_concurrent_cap`'s wiring exactly. Was a bare
/// `std::env::var("DARKMUX_FUNNEL_JUDGE_CONCURRENCY")` read before this;
/// moved onto the standard precedence chain as part of the funnel->review
/// rename (renaming the var anyway was the natural point to fix it).
pub fn review_judge_concurrency() -> u32 {
    let cfg = config().review.as_ref().and_then(|r| r.judge_concurrency);
    pick_parsed("DARKMUX_REVIEW_JUDGE_CONCURRENCY", cfg, Some(1))
        .unwrap()
        .max(1)
}

// ── Role -> profile map (#1475 packet 1) ──
/// (#1475 packet 1) Normalize a raw role->profile map: trim BOTH the role key
/// AND the profile value, and drop any binding whose profile is blank (a
/// `"judge": "  "` slip never binds a blank name). Both public accessors below
/// funnel through this ONE normalizer so that doctor's view (`role_profiles`)
/// and resolution's lookup (`role_profile`) can never disagree on a key: a
/// hand-edited padded key like `" judge"` reads as `judge` for BOTH, so it can't
/// be doctor-visible-but-resolution-invisible (the honesty split #1475 forbids).
/// Pure (takes the raw map explicitly) so it's testable without the process-wide
/// `config()`.
fn normalize_role_profiles(
    raw: Option<&std::collections::BTreeMap<String, String>>,
) -> std::collections::BTreeMap<String, String> {
    match raw {
        None => std::collections::BTreeMap::new(),
        Some(m) => m
            .iter()
            .filter_map(|(role, profile)| {
                let p = profile.trim();
                (!p.is_empty()).then(|| (role.trim().to_string(), p.to_string()))
            })
            .collect(),
    }
}

/// (#1475 packet 1) Look up a role's bound profile in a raw map, normalized.
/// The role id is trimmed to match the normalized (trimmed) keys — the pure core
/// of [`role_profile`], sharing the exact same key handling as [`role_profiles`]
/// so the two never diverge. `None` = the role is UNMAPPED.
fn lookup_role_profile(
    raw: Option<&std::collections::BTreeMap<String, String>>,
    role_id: &str,
) -> Option<String> {
    normalize_role_profiles(raw).get(role_id.trim()).cloned()
}

/// (#1475 packet 1) The full machine-local role->profile map from
/// `config.json` (`{ "<role-id>": "<profile-name>" }`), or an empty map when
/// unset. Config is the PRIMARY (and only) mechanism — there is deliberately no
/// per-role env var (a `DARKMUX_ROLE_PROFILE_<role>` over a map is awkward and
/// unneeded; the map is edited via `darkmux config set role_profiles.<role>
/// <profile>`), mirroring the CONFIG-ONLY Redis connection bits above. Keys and
/// values are trimmed and blank bindings dropped via [`normalize_role_profiles`]
/// — the SAME normalizer [`role_profile`] resolves through, so doctor (which
/// reads this) and resolution (which reads `role_profile`) validate the identical
/// key set.
pub fn role_profiles() -> std::collections::BTreeMap<String, String> {
    normalize_role_profiles(config().role_profiles.as_ref())
}

/// (#1475 packet 1) The profile name bound to `role_id` in the role->profile
/// map, or `None` when the role is UNMAPPED (the caller then falls back to
/// `default_profile` — the fresh-user floor). Resolves through the SAME
/// [`normalize_role_profiles`] that [`role_profiles`] (doctor's view) uses, so a
/// padded key resolves exactly as doctor reports it — no silent divergence. The
/// registry-existence of the named profile is NOT checked here (config-leniency
/// contract 7 — semantic validation lives at resolution:
/// `darkmux_profiles::resolve_role_profile` — and in `darkmux doctor`).
pub fn role_profile(role_id: &str) -> Option<String> {
    lookup_role_profile(config().role_profiles.as_ref(), role_id)
}

pub fn default_role() -> Option<String> {
    let cfg = config().runtime.as_ref().and_then(|r| r.default_role.as_deref());
    pick_string("DARKMUX_DEFAULT_ROLE", cfg, None)
}
pub fn daemon_cors_origins() -> Option<String> {
    let cfg = config().runtime.as_ref().and_then(|r| r.daemon_cors_origins.as_deref());
    pick_string("DARKMUX_DAEMON_CORS_ORIGINS", cfg, None)
}
/// (#881) Whether the serve daemon may read the `darkmux-serve-token` macOS
/// Keychain item for bearer auth. Config-only gate (`config.runtime.
/// daemon_auth_enabled`, default `false`) — the env token path
/// `DARKMUX_SERVE_TOKEN` needs no gate (its presence is the opt-in). Consumed by
/// `darkmux_flow::serve_token`'s tier-2; auth being *active* is decided by
/// whether a token actually resolves (`serve_token_present`), never by this flag
/// alone (a gate-on-but-no-token state must NOT 401 every request).
pub fn serve_auth_config_enabled() -> bool {
    config().runtime.as_ref().and_then(|r| r.daemon_auth_enabled).unwrap_or(false)
}
/// (#1011) Fraction (0–1) of the dispatch model's context window budgeted for
/// the coder brief's injected-context blocks (cautions + lessons + corrections).
/// Precedence: `env(DARKMUX_INJECTED_CONTEXT_FRACTION)`, then
/// `config.runtime.injected_context_fraction`, then the `0.15` default. Clamped
/// to `[0.0, 1.0]` so a fat-fingered config/env can't budget a negative or
/// over-100%-of-window block.
pub fn injected_context_fraction() -> f64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.injected_context_fraction);
    let v = pick_parsed("DARKMUX_INJECTED_CONTEXT_FRACTION", cfg, Some(0.15)).unwrap();
    // A non-finite value (`NaN`/`inf` — `clamp` would pass `NaN` through) falls
    // back to the default rather than silently degrading the budget to its floor.
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.15
    }
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
/// (#1311) Diagnostic verbosity. `env(DARKMUX_LOG)` (lower-cased) >
/// `config.runtime.log_level` > `"info"`. `"info"` = the informative
/// dispatch-liveness phase markers; `"debug"` additionally turns on per-call
/// detail (hosted call host/model/tokens/wall_ms). NEVER a secret at any level.
pub fn log_level() -> String {
    if let Some(s) = env_str("DARKMUX_LOG") {
        return s.to_ascii_lowercase();
    }
    config()
        .runtime
        .as_ref()
        .and_then(|r| r.log_level.clone())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "info".to_string())
}

/// (#1311) Whether per-call debug logging is on (`log_level() == "debug"`).
pub fn debug_logging() -> bool {
    log_level() == "debug"
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

// ── Mission board (#1230 Packet 5) ──
/// How many days an Active mission may sit with zero `Complete` phases
/// before `darkmux mission status`'s drift detector flags it as stale.
/// Resolves `env(DARKMUX_MISSION_STALE_ACTIVE_DAYS) >
/// config.mission.stale_active_days > 14` — mirrors
/// `remote_concurrent_cap`'s wiring exactly.
pub fn mission_stale_active_days() -> u64 {
    let cfg = config().mission.as_ref().and_then(|m| m.stale_active_days);
    pick_parsed("DARKMUX_MISSION_STALE_ACTIVE_DAYS", cfg, Some(14)).unwrap()
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
    pick_dir(
        env_str("DARKMUX_FLOWS_DIR"),
        config().dirs.as_ref().and_then(|d| d.flows.as_deref()),
        flows_dir_default,
    )
}

/// The built-in flows-dir default (third precedence tier), split out so it can
/// be isolated in test builds.
///
/// (#994) In test / `test-support` builds the default must NOT be the
/// operator's real `~/.darkmux/flows`. Derived consumers now READ the flow
/// stream during a rebuild — the crew index's `cautions` derive scans
/// `flows_dir()` — so any test that doesn't explicitly set `DARKMUX_FLOWS_DIR`
/// would ingest live operator flow data (machine-dependent, and a ~50 MB scan
/// on CI). Isolating the default to a throwaway path makes an un-set flows dir
/// empty by construction; tests that need real content set `DARKMUX_FLOWS_DIR`
/// (the env tier, which wins). Production is unaffected — `cfg`-gated, and the
/// path still ends in `flows`. Same #811-style "empty operator state by
/// construction in test builds" move the empty `config()` tier already makes.
#[cfg(not(any(test, feature = "test-support")))]
fn flows_dir_default() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".darkmux").join("flows"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/darkmux/flows"))
}

#[cfg(any(test, feature = "test-support"))]
fn flows_dir_default() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/darkmux-test-isolated/flows")
}

/// (#703) Host cache dir for the extracted static `darkmux-runtime` binary,
/// bind-mounted into operator-named images (`dispatch --image <tag>`)
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

// The next two are **override-only** (`env > config.dirs.X`, else `None`):
// each caller keeps its own no-HOME default/error handling, so the accessor
// yields just the override and the caller applies its default.

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

    // ── fleet_mode (#933): env > config > standalone default ──
    #[serial_test::serial]
    #[test]
    fn fleet_mode_env_overrides_and_defaults_standalone() {
        use crate::config::FleetMode;
        let k = "DARKMUX_FLEET_MODE";
        unsafe { std::env::remove_var(k); }
        // No env + EMPTY test config → standalone default.
        assert_eq!(fleet_mode_raw(), "standalone");
        assert_eq!(fleet_mode(), FleetMode::Standalone);
        // Env override wins, case-insensitive; the raw token is preserved.
        unsafe { std::env::set_var(k, "HUB"); }
        assert_eq!(fleet_mode_raw(), "HUB");
        assert_eq!(fleet_mode(), FleetMode::Hub);
        // An unrecognized token passes through raw but resolves typed→standalone
        // (doctor flags the raw typo separately, #934).
        unsafe { std::env::set_var(k, "hubb"); }
        assert_eq!(fleet_mode_raw(), "hubb");
        assert_eq!(fleet_mode(), FleetMode::Standalone);
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

    // ── model_load_timeout_seconds (#1276): env > config > 600 default,
    //    mirroring inactivity_timeout_seconds' resolution exactly ──
    #[serial_test::serial]
    #[test]
    fn model_load_timeout_env_overrides_then_default() {
        let k = "DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        // No env + the empty test config (#811) → the built-in 600 default.
        assert_eq!(model_load_timeout_seconds(), 600);
        unsafe { std::env::set_var(k, "45") };
        assert_eq!(model_load_timeout_seconds(), 45, "env wins live");
        // An unparseable env value falls through (here, to the default).
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(model_load_timeout_seconds(), 600);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn redis_stream_default_when_unset() {
        let prev = std::env::var("DARKMUX_REDIS_STREAM").ok();
        unsafe { std::env::remove_var("DARKMUX_REDIS_STREAM"); }
        // With no env and the empty test config (#811), the built-in default holds.
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
        // No env, and the empty test config (#811) → ends in a `flows` dir (the
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
            ("DARKMUX_IDENTITY_PATH", identity_path_override as Acc),
            ("DARKMUX_ACK_DIR", ack_dir_override),
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
        // No env, empty config → the `<root>/notebook` derived default.
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
    fn injected_context_fraction_env_then_default_and_clamped() {
        let prev = std::env::var("DARKMUX_INJECTED_CONTEXT_FRACTION").ok();
        unsafe { std::env::remove_var("DARKMUX_INJECTED_CONTEXT_FRACTION"); }
        assert!((injected_context_fraction() - 0.15).abs() < 1e-9, "unset → default 0.15");
        unsafe { std::env::set_var("DARKMUX_INJECTED_CONTEXT_FRACTION", "0.30"); }
        assert!((injected_context_fraction() - 0.30).abs() < 1e-9, "env wins");
        // Out-of-range values are clamped to [0,1].
        unsafe { std::env::set_var("DARKMUX_INJECTED_CONTEXT_FRACTION", "5.0"); }
        assert!((injected_context_fraction() - 1.0).abs() < 1e-9, ">1 clamps to 1.0");
        unsafe { std::env::set_var("DARKMUX_INJECTED_CONTEXT_FRACTION", "-2.0"); }
        assert!(injected_context_fraction() == 0.0, "<0 clamps to 0.0");
        // A non-finite value falls back to the default (clamp would pass NaN).
        unsafe { std::env::set_var("DARKMUX_INJECTED_CONTEXT_FRACTION", "NaN"); }
        assert!((injected_context_fraction() - 0.15).abs() < 1e-9, "NaN → default, not floor");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_INJECTED_CONTEXT_FRACTION", v),
                None => std::env::remove_var("DARKMUX_INJECTED_CONTEXT_FRACTION"),
            }
        }
    }

    // ── remote_max_tokens_per_execution (#1260): env > config > 500000 ──
    #[serial_test::serial]
    #[test]
    fn remote_max_tokens_per_execution_env_then_default() {
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        // No env + the empty test config (#811) → the built-in 500K default.
        assert_eq!(remote_max_tokens_per_execution(), 500_000);
        unsafe { std::env::set_var(k, "25000"); }
        assert_eq!(remote_max_tokens_per_execution(), 25_000, "env tier wins live");
        // An unparseable env value falls through to the default, never panics.
        unsafe { std::env::set_var(k, "half-a-million"); }
        assert_eq!(remote_max_tokens_per_execution(), 500_000);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── remote_concurrent_cap (#1230 Packet 1): env > config > 4 ──
    #[serial_test::serial]
    #[test]
    fn remote_concurrent_cap_env_then_default() {
        let k = "DARKMUX_REMOTE_CONCURRENT_CAP";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        // No env + the empty test config (#811) → the built-in placeholder 4.
        assert_eq!(remote_concurrent_cap(), 4);
        unsafe { std::env::set_var(k, "8"); }
        assert_eq!(remote_concurrent_cap(), 8, "env tier wins live");
        // An unparseable env value falls through to the default, never panics.
        unsafe { std::env::set_var(k, "lots"); }
        assert_eq!(remote_concurrent_cap(), 4);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── review_judge_concurrency (#1349): env > config > 1, min-clamped ──
    #[serial_test::serial]
    #[test]
    fn review_judge_concurrency_env_then_default_then_min_clamp() {
        let k = "DARKMUX_REVIEW_JUDGE_CONCURRENCY";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        // No env + the empty test config (#811) → the built-in default 1
        // (fully sequential — same conservative default the pre-#1349 bare
        // env read used).
        assert_eq!(review_judge_concurrency(), 1);
        unsafe { std::env::set_var(k, "4"); }
        assert_eq!(review_judge_concurrency(), 4, "env tier wins live");
        // Zero (and any unparseable value) never disables the judge step —
        // clamped up to the sequential floor, same as the retired
        // `.filter(|n| *n >= 1)` guard.
        unsafe { std::env::set_var(k, "0"); }
        assert_eq!(review_judge_concurrency(), 1);
        unsafe { std::env::set_var(k, "not-a-number"); }
        assert_eq!(review_judge_concurrency(), 1);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── role_profiles / role_profile (#1475 packet 1) ──
    // Under the empty test config (#811) the map is empty and every role is
    // unmapped — the fresh-user floor. Populated-map resolution is exercised by
    // `darkmux_profiles::resolve_role_profile_with` (the pure core that takes the
    // mapped name explicitly, so it doesn't depend on the process-wide config()).
    #[test]
    fn role_profile_unset_is_empty_and_none() {
        assert!(role_profiles().is_empty(), "empty test config → no role bindings");
        assert_eq!(role_profile("judge"), None, "unmapped role → None (caller uses default_profile)");
    }

    #[test]
    fn padded_role_key_resolves_same_as_doctor_reports() {
        // (#1475 packet 1) A hand-edited padded key (`" judge"`) must resolve the
        // SAME binding doctor validates. Doctor reads `role_profiles()` (→
        // `normalize_role_profiles`), resolution reads `role_profile()` (→
        // `lookup_role_profile` → the same normalizer). This asserts they can't
        // diverge: the padded key is doctor-visible AND resolution finds it — no
        // "doctor says fine / resolution silently falls back to default" split.
        let mut raw = std::collections::BTreeMap::new();
        raw.insert(" judge".to_string(), "qwen35b".to_string());

        // Doctor's view: the padded key reads under its trimmed name.
        let doctor_view = normalize_role_profiles(Some(&raw));
        assert_eq!(
            doctor_view.get("judge"),
            Some(&"qwen35b".to_string()),
            "doctor validates the padded key under its trimmed name"
        );

        // Resolution's view: the SAME binding is found — no silent divergence.
        assert_eq!(
            lookup_role_profile(Some(&raw), "judge"),
            Some("qwen35b".to_string()),
            "resolution finds the binding doctor reported (not None → default fallback)"
        );

        // And they agree key-for-key across the whole doctor-visible map.
        for (role, profile) in &doctor_view {
            assert_eq!(
                lookup_role_profile(Some(&raw), role).as_ref(),
                Some(profile),
                "every doctor-visible binding resolves identically for role `{role}`"
            );
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
