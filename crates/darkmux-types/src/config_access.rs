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
/// `"radio": { "router_profile": "" }` the operator hasn't filled in defers
/// to the env/built-in tier rather than stamping an empty value.
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

/// (#2165) Which tier of `env > config.json > built-in default` a resolved
/// setting's value actually came from — the provenance half of "the operator
/// never has to wonder where a decision came from" (#44). Every accessor
/// below has a name-matched `<accessor>_with_source()` sibling that returns
/// this alongside the value, for the record/envelope/doctor surfaces that
/// need to SHOW the tier, not just resolve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    Config,
    Env,
}

impl Source {
    /// The wire string this tier renders as everywhere a record/envelope/
    /// doctor row names it: `"built-in"` | `"config"` | `"env"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::BuiltIn => "built-in",
            Source::Config => "config",
            Source::Env => "env",
        }
    }
}

/// `pick_parsed`'s sibling that also reports WHICH tier won. Pure + testable
/// like `pick_parsed` — the only difference is the second return value.
fn pick_parsed_with_source<T: FromStr + Copy>(
    env_key: &str,
    cfg: Option<T>,
    default: Option<T>,
) -> (Option<T>, Source) {
    if let Some(v) = env_str(env_key).and_then(|s| s.parse::<T>().ok()) {
        return (Some(v), Source::Env);
    }
    if let Some(v) = cfg {
        return (Some(v), Source::Config);
    }
    (default, Source::BuiltIn)
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

// ── GitHub CLI verb allowlist (#1685) ──
/// Whether the `gh`-verb allowlist gate is active at all. `env(DARKMUX_CMD_ENABLED)`
/// truthy (`1`/`true`/`yes`/`on`, case-insensitive) > `config.cmd.enabled` >
/// `false` — fail closed: darkmux never runs an operator-authored panel verb
/// that shells out to the operator's own `gh` unless this is explicitly
/// turned on. See `CmdConfig`'s own doc for the feature this gates.
pub fn cmd_enabled() -> bool {
    if let Some(s) = env_str("DARKMUX_CMD_ENABLED") {
        return matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config().cmd.as_ref().and_then(|g| g.enabled).unwrap_or(false)
}
/// The allowlisted verb names. `env(DARKMUX_CMD_ALLOWED)` (comma-separated,
/// trimmed, empty entries dropped) > `config.cmd.allowed` > empty — nothing
/// is allowed by default even once `cmd_enabled()` is true; the allowlist
/// itself is opt-in per verb, not implied by the gate alone.
pub fn cmd_allowed_verbs() -> Vec<String> {
    if let Some(s) = env_str("DARKMUX_CMD_ALLOWED") {
        return s.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
    }
    config().cmd.as_ref().and_then(|g| g.allowed.clone()).unwrap_or_default()
}
/// Whether `verb` may run right now — the gate is enabled AND `verb` is
/// named in the allowlist. Fails closed on either count.
pub fn cmd_allowed(verb: &str) -> bool {
    cmd_enabled() && cmd_allowed_verbs().iter().any(|v| v == verb)
}

// ── (#2093) Hooks — flow-record hook sink (match → HTTP POST) ──
/// The hooks feature gate: `env(DARKMUX_HOOKS_ENABLED)` truthy
/// (`1`/`true`/`yes`/`on`, case-insensitive) > `config.hooks.enabled` >
/// `false` — fail closed, mirroring `cmd_enabled`. There is deliberately NO
/// env override for individual rules; a rule is a structured object, not a
/// scalar an env var can carry — the env tier only gates the feature as a
/// whole.
pub fn hooks_enabled() -> bool {
    if let Some(s) = env_str("DARKMUX_HOOKS_ENABLED") {
        return matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config().hooks.as_ref().and_then(|h| h.enabled).unwrap_or(false)
}
/// (#2093 merge-gate finding 8) Derived from the SAME root resolution
/// every other darkmux directory resolves through —
/// `paths::resolve(Auto)`, which honors `DARKMUX_HOME` and a
/// project-local `./.darkmux` before `~/.darkmux` — mirroring
/// `lab_dir_default`'s own doc. Before this fix, `hooks_outbox_dir`'s
/// fallback went straight to `dirs::home_dir()`, so a `DARKMUX_HOME`-
/// scoped install still wrote hook outbox files to the operator's REAL
/// `~/.darkmux/hooks` regardless — the same bug class #1585 fixed for
/// `lab_dir`, one directory over.
#[cfg(not(any(test, feature = "test-support")))]
fn hooks_outbox_dir_default() -> std::path::PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::Auto).root.join("hooks")
}

/// Test builds must never default onto the operator's real
/// `~/.darkmux/hooks` — same isolation discipline as `lab_dir_default`'s
/// own test-build variant (#994). A test that DID isolate itself (a
/// `DARKMUX_HOME` tempdir, or a project-local `./.darkmux`) is honored
/// verbatim, because a test that isolated itself means it.
#[cfg(any(test, feature = "test-support"))]
fn hooks_outbox_dir_default() -> std::path::PathBuf {
    let resolved = crate::paths::resolve(crate::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return std::path::PathBuf::from("/tmp/darkmux-test-isolated/hooks");
    }
    resolved.root.join("hooks")
}

/// Where per-rule outbox/cursor files live: `config.hooks.outbox_dir`
/// (tilde-expanded) or the built-in default `<darkmux root>/hooks`.
/// CONFIG-ONLY — no per-field env var (see `hooks_enabled`'s doc for why).
pub fn hooks_outbox_dir() -> std::path::PathBuf {
    pick_dir(None, config().hooks.as_ref().and_then(|h| h.outbox_dir.as_deref()), hooks_outbox_dir_default)
}
/// The configured hook rules, verbatim (raw `HookRule`s — match validation
/// and URL-loopback validation happen at `HookSink` construction, not here;
/// this accessor is a pure config-tier read). CONFIG-ONLY, same reasoning
/// as `hooks_outbox_dir`.
pub fn hooks_rules() -> Vec<crate::config::HookRule> {
    config().hooks.as_ref().and_then(|h| h.rules.clone()).unwrap_or_default()
}
/// (#2093 merge-gate finding 5) The hard cap, in MiB, on undelivered bytes
/// a single rule's outbox may hold before appends for that rule stop:
/// `env(DARKMUX_HOOKS_MAX_OUTBOX_MB)` > `config.hooks.max_outbox_mb` >
/// built-in default `256`.
pub fn hooks_max_outbox_mb() -> u64 {
    let cfg = config().hooks.as_ref().and_then(|h| h.max_outbox_mb);
    pick_parsed("DARKMUX_HOOKS_MAX_OUTBOX_MB", cfg, Some(256)).unwrap()
}
/// (#2183) Where jq hook adapters live — always `<hooks_outbox_dir>/adapters`,
/// so an operator who relocates `hooks.outbox_dir` gets their adapters
/// relocated with it. No separate config field / env var: this is a fixed
/// convention (the issue's own spec — "resolved inside
/// `~/.darkmux/hooks/adapters/`"), not an independent knob.
pub fn hooks_adapters_dir() -> std::path::PathBuf {
    hooks_outbox_dir().join("adapters")
}
/// (#2183) The wall-clock cap, in milliseconds, on one `transform`
/// evaluation (compile + run): `env(DARKMUX_HOOKS_JQ_TIMEOUT_MS)` >
/// `config.hooks.jq_timeout_ms` > built-in default `5000` (5s).
pub fn hooks_jq_timeout_ms() -> u64 {
    let cfg = config().hooks.as_ref().and_then(|h| h.jq_timeout_ms);
    pick_parsed("DARKMUX_HOOKS_JQ_TIMEOUT_MS", cfg, Some(5_000)).unwrap()
}
/// (#2183) The hard cap, in bytes, on a `transform`'s produced body:
/// `env(DARKMUX_HOOKS_JQ_MAX_OUTPUT_BYTES)` > `config.hooks.jq_max_output_bytes`
/// > built-in default `1048576` (1 MiB).
pub fn hooks_jq_max_output_bytes() -> u64 {
    let cfg = config().hooks.as_ref().and_then(|h| h.jq_max_output_bytes);
    pick_parsed("DARKMUX_HOOKS_JQ_MAX_OUTPUT_BYTES", cfg, Some(1_048_576)).unwrap()
}

// ── Runtime behavior ──
pub fn inactivity_timeout_seconds() -> u64 {
    inactivity_timeout_seconds_with_source().0
}
/// (#2165) `inactivity_timeout_seconds` plus WHICH tier resolved it — the
/// host forwards both the value AND this tier into the container (a
/// companion `DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE` env var, mirroring
/// the value var's own forwarding) so the runtime's soft-warning stderr line
/// and any future bound-hit record can name it, not just the number.
pub fn inactivity_timeout_seconds_with_source() -> (u64, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.inactivity_timeout_seconds);
    let (v, s) = pick_parsed_with_source("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", cfg, Some(600));
    (v.unwrap(), s)
}
/// (#1276) Bounded model-load/unload phase for gestalt host-port calls —
/// consumed by `darkmux_profiles::gestalt_host::resolved_load_deadline`,
/// which wraps it into the mandatory `Deadline` every `ModelHost` mutation
/// takes. Mirrors `inactivity_timeout_seconds`' wiring exactly.
pub fn model_load_timeout_seconds() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.model_load_timeout_seconds);
    pick_parsed("DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS", cfg, Some(600)).unwrap()
}
/// (#2361, swarm finding S4-4) Bound on ONE operator-supplied shell command
/// a STEP runs — `mods.gate`'s `test_command`, `procedural.shell`'s
/// `command`. Both used to run unbounded and unregistered, so a hung suite
/// pinned the mission open and neither SIGTERM nor SIGINT could reach it.
/// Consumed by `darkmux_crew::bounded_command::configured_timeout`, which
/// turns this into the `Duration` `bounded_command::run_bounded` enforces.
/// (An earlier revision of this comment named `run_shell_bounded`, which has
/// never existed.) Mirrors [`model_load_timeout_seconds`]' wiring exactly —
/// the sibling bound on a host model load — with ONE deliberate difference:
/// `0` here means UNBOUNDED (#2310 fix-loop E2), the same reading every
/// other darkmux zero-knob has. This accessor returns the raw seconds;
/// `configured_timeout` is where `0` becomes "never fires".
pub fn step_command_timeout_seconds() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.step_command_timeout_seconds);
    pick_parsed("DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS", cfg, Some(600)).unwrap()
}
/// (#2394) How many DISPATCH-FREE steps `darkmux_crew::
/// concurrent_dispatch::run_bounded` runs at once — every step whose
/// `StepKind::seat` claims `SeatClaim::NoModel` (`procedural.shell`,
/// `procedural.noop`, `mods.gate`, `records.gather`,
/// `deliver.github_review`, the crawl planners). Resolves
/// `env(DARKMUX_DISPATCH_FREE_CONCURRENCY) >
/// config.runtime.dispatch_free_concurrency > 8` — mirrors
/// [`remote_concurrent_cap`]'s wiring exactly, and is deliberately a
/// SEPARATE knob from it: that cap protects a hosted endpoint's rate limit,
/// which a shell command does not have. Before this existed, dispatch-free
/// steps rode the remote track, and a mission launch's `remote_cap: 1` made
/// six independent `procedural.shell` waits run strictly one at a time.
///
/// Not unbounded, which is the tempting default. `mods.gate` runs an
/// operator-supplied `test_command` per mod; N of those at once is N test
/// suites on one machine. 8 is generous relative to the bound each of these
/// steps already has (`step_command_timeout_seconds`) without being "as
/// many as the graph happens to contain". Clamped to >= 1: a literal `0`
/// would mean "run nothing, forever".
pub fn dispatch_free_concurrency() -> u32 {
    let cfg = config().runtime.as_ref().and_then(|r| r.dispatch_free_concurrency);
    pick_parsed("DARKMUX_DISPATCH_FREE_CONCURRENCY", cfg, Some(8)).unwrap().max(1)
}
pub fn max_turns() -> Option<u32> {
    max_turns_with_source().0
}
/// (#2165) `max_turns` plus WHICH tier resolved it.
pub fn max_turns_with_source() -> (Option<u32>, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_turns);
    pick_parsed_with_source("DARKMUX_RUNTIME_MAX_TURNS", cfg, None)
}
pub fn max_tokens() -> Option<u32> {
    max_tokens_with_source().0
}
/// (#2165) `max_tokens` plus WHICH tier resolved it.
pub fn max_tokens_with_source() -> (Option<u32>, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_tokens);
    pick_parsed_with_source("DARKMUX_RUNTIME_MAX_TOKENS", cfg, None)
}
/// (#1221) Per-call completion-token cap override. `None` = the runtime's
/// built-in default (`MAX_TOKENS_PER_CALL` = 10000).
pub fn max_tokens_per_call() -> Option<u32> {
    max_tokens_per_call_with_source().0
}
/// (#2165) `max_tokens_per_call` plus WHICH tier resolved it.
pub fn max_tokens_per_call_with_source() -> (Option<u32>, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_tokens_per_call);
    pick_parsed_with_source("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", cfg, None)
}

/// (#1221) How far the model reasons between the runtime's mid-turn check-ins.
/// `None` = the runtime's built-in `REASONING_CHECKPOINT_INTERVAL` (1000).
///
/// Distinct from `max_tokens_per_call` on purpose: that one bounds an ANSWER
/// and wants to be large, this one samples a THOUGHT and wants to be small.
/// They were briefly one number, which is wrong for whichever job it is not
/// tuned for.
pub fn reasoning_checkpoint_interval_tokens() -> Option<u32> {
    reasoning_checkpoint_interval_tokens_with_source().0
}
/// (#2165) `reasoning_checkpoint_interval_tokens` plus WHICH tier resolved
/// it.
pub fn reasoning_checkpoint_interval_tokens_with_source() -> (Option<u32>, Source) {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.reasoning_checkpoint_interval_tokens);
    pick_parsed_with_source("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", cfg, None)
}

/// (#2171) The GENERATION check-in — bounds every call that does NOT carry
/// the reasoning bound above, not just reasoning ones. `None` = the
/// runtime's built-in `GENERATION_CHECKPOINT_INTERVAL` (4000).
pub fn generation_checkpoint_interval_tokens() -> Option<u32> {
    generation_checkpoint_interval_tokens_with_source().0
}
/// (#2171 rebase onto #2165) `generation_checkpoint_interval_tokens` plus
/// WHICH tier resolved it — same `_with_source` pattern
/// `reasoning_checkpoint_interval_tokens_with_source` uses, now that #2167
/// (the sibling PR this rebases onto) has landed and the helper exists.
pub fn generation_checkpoint_interval_tokens_with_source() -> (Option<u32>, Source) {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.generation_checkpoint_interval_tokens);
    pick_parsed_with_source("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", cfg, None)
}

/// (#2190) Per-dispatch budget for intra-turn stall recoveries — how many
/// times the runtime drops a useless turn (empty `tool_calls`, or a
/// runaway-reasoning cut) and nudges before escalating out of local-tier.
/// `None` = the runtime's built-in `MAX_STALL_RECOVERIES` (2).
pub fn max_stall_recoveries() -> Option<u32> {
    max_stall_recoveries_with_source().0
}
/// (#2190) `max_stall_recoveries` plus WHICH tier resolved it.
pub fn max_stall_recoveries_with_source() -> (Option<u32>, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.max_stall_recoveries);
    pick_parsed_with_source("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES", cfg, None)
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
// ── Radio interpreter (#1698 Packet B2) ──
/// The ROUTING seat's explicit profile override. Resolves
/// `env(DARKMUX_RADIO_ROUTER_PROFILE) > config.radio.router_profile >
/// unset`, the standard tier order. `None` when unset — callers pass that
/// straight through as `DispatchOpts.profile_name: None`, which preserves
/// the existing `role_profiles.radio-router` precedence (see
/// `RadioConfig::router_profile`'s own doc).
pub fn radio_router_profile() -> Option<String> {
    pick_string("DARKMUX_RADIO_ROUTER_PROFILE", config().radio.as_ref().and_then(|r| r.router_profile.as_deref()), None)
}
/// The ANSWERING seat's explicit profile override. Resolves
/// `env(DARKMUX_RADIO_ANSWERER_PROFILE) > config.radio.answerer_profile >
/// unset`. `None` when unset, same pass-through contract as
/// [`radio_router_profile`].
pub fn radio_answerer_profile() -> Option<String> {
    pick_string("DARKMUX_RADIO_ANSWERER_PROFILE", config().radio.as_ref().and_then(|r| r.answerer_profile.as_deref()), None)
}
/// The shipped humor default. The middle of the dial on purpose: sampled on
/// the same question, anything under about 40 reads as plain, 50 is the
/// first value with a pulse, and 100 is the full persona. Objective help
/// with a little voice out of the box; `radio.humor` is the dial. (65 before 2026-08-28: the value carried over
/// from the operator's own persona override, never chosen as a default.)
pub const RADIO_HUMOR_DEFAULT: u64 = 50;

/// The RADIO persona's humor dial (0-100). Resolves
/// `env(DARKMUX_RADIO_HUMOR) > config.radio.humor > RADIO_HUMOR_DEFAULT`, clamped to
/// `0..=100` (an out-of-range operator value is clamped rather than
/// rejected — the persona template only ever renders the number as text,
/// so an out-of-range value is a cosmetic surprise, not a correctness
/// break; clamping keeps this accessor infallible like every other numeric
/// accessor in this file).
pub fn radio_humor() -> u8 {
    // `RadioConfig::humor` is stored as `u64` (see that field's own doc on
    // why — matching `config set`'s shared `Ty::Uint` parse avoids a
    // whole-config-reset hazard on an out-of-u8-range hand-edit); clamped
    // and narrowed to `u8` HERE, at the accessor, since every consumer of
    // this value (the persona substitution, the humor picker's presets)
    // only ever needs the already-validated `0..=100` range.
    let cfg = config().radio.as_ref().and_then(|r| r.humor);
    let n: u64 = pick_parsed("DARKMUX_RADIO_HUMOR", cfg, Some(RADIO_HUMOR_DEFAULT)).unwrap();
    n.min(100) as u8
}

// ── ACP process lifecycle (#1698 Packet B2 / #1684 session hygiene) ──
/// How many consecutive idle minutes `darkmux acp` waits (zero live
/// sessions/commands) before self-exiting. Resolves
/// `env(DARKMUX_ACP_IDLE_EXIT_MINUTES) > config.runtime.acp_idle_exit_minutes > 30`.
/// `0` disables self-exit entirely (an explicit opt-out, mirroring
/// `remote.max_tokens_per_execution`'s `0`-means-hard-off convention
/// elsewhere in this file).
pub fn acp_idle_exit_minutes() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.acp_idle_exit_minutes);
    pick_parsed("DARKMUX_ACP_IDLE_EXIT_MINUTES", cfg, Some(30)).unwrap()
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

/// (#1548) Whether the internal runtime should inject feedback (nudge)
/// messages into a struggling dispatch's next turn — an **opt-out**:
/// `env(DARKMUX_FEEDBACK_INJECTION)` falsy (`0`/`off`/`false`/`no`) disables >
/// `config.runtime.feedback_injection` > `true` (default on).
///
/// Before #1548 this had NO accessor here — `runtime.feedback_injection` was
/// settable and typed, but the `config.json` tier was never consulted by
/// anything: the sole reader was `runtime/src/feedback.rs`, reading the raw
/// `DARKMUX_FEEDBACK_INJECTION` env var directly inside the container, and the
/// host never forwarded that var into `docker run` — so neither tier reached
/// the runtime and injection was unconditionally on. This accessor closes the
/// `config.json` half; the docker-spawn site (`dispatch_internal.rs`) forwards
/// its resolved value as `-e DARKMUX_FEEDBACK_INJECTION=<v>` so the container's
/// own reader (whose falsy set this mirrors) sees the resolved tier, not just
/// a host-side env override.
pub fn feedback_injection() -> bool {
    feedback_injection_with_source().0
}
/// (#2165) `feedback_injection` plus WHICH tier resolved it.
pub fn feedback_injection_with_source() -> (bool, Source) {
    if let Some(s) = env_str("DARKMUX_FEEDBACK_INJECTION") {
        return (!matches!(s.as_str(), "0" | "off" | "false" | "no"), Source::Env);
    }
    match config().runtime.as_ref().and_then(|r| r.feedback_injection) {
        Some(v) => (v, Source::Config),
        None => (true, Source::BuiltIn),
    }
}

/// (#2094) The global inter-turn rest, in milliseconds, the internal
/// runtime sleeps between inference turns on every LOCAL dispatch. Resolves
/// `env(DARKMUX_TURN_DELAY_MS) > config.runtime.turn_delay_ms > 0` —
/// mirrors `inactivity_timeout_seconds`'s wiring exactly. `0` (the default)
/// means no rest, the pre-existing behavior; the runtime clamps a
/// configured value at or above the inactivity timeout rather than
/// honoring it verbatim (see `runtime/src/loop_runner.rs`).
pub fn turn_delay_ms() -> u64 {
    turn_delay_ms_with_source().0
}
/// (#2165) `turn_delay_ms` plus WHICH tier resolved it.
pub fn turn_delay_ms_with_source() -> (u64, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.turn_delay_ms);
    let (v, s) = pick_parsed_with_source("DARKMUX_TURN_DELAY_MS", cfg, Some(0));
    (v.unwrap(), s)
}

/// (#2107, #1833) Cadence, in milliseconds, of `darkmux serve`'s daemon-side
/// continuous host sampler (the machine stats drawer's live feed). Resolves
/// `env(DARKMUX_HOST_SAMPLER_INTERVAL_MS) > config.runtime.
/// host_sampler_interval_ms > 5000` — mirrors `turn_delay_ms`'s wiring
/// exactly. `0` disables the sampler entirely (an explicit opt-out, same
/// convention as `remote.max_tokens_per_execution`'s `0`).
pub fn host_sampler_interval_ms() -> u64 {
    let cfg = config().runtime.as_ref().and_then(|r| r.host_sampler_interval_ms);
    pick_parsed("DARKMUX_HOST_SAMPLER_INTERVAL_MS", cfg, Some(5000)).unwrap()
}

/// (#2111) How many `dispatch_internal::run_telemetry_sampler` ticks (its
/// 2s cadence) between `machine.telemetry` SAMPLE flow records — the
/// periodic host-pressure curve (thermal/power/cpu/gpu/mem) a run-detail
/// view can chart alongside `machine.thermal`'s TRANSITION events. Resolves
/// `env(DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES) >
/// config.runtime.telemetry_record_every_samples > 5` (≈10s at the
/// sampler's 2s cadence) — mirrors `turn_delay_ms`'s wiring. `0` disables
/// the periodic curve without touching the sampler thread itself: the
/// thermal governor still reads every tick, and `dispatch complete`'s
/// `host_window` summary is built from every sample regardless.
pub fn telemetry_record_every_samples() -> u64 {
    telemetry_record_every_samples_with_source().0
}
/// `telemetry_record_every_samples` plus WHICH tier resolved it.
pub fn telemetry_record_every_samples_with_source() -> (u64, Source) {
    let cfg = config().runtime.as_ref().and_then(|r| r.telemetry_record_every_samples);
    let (v, s) =
        pick_parsed_with_source("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", cfg, Some(5));
    (v.unwrap(), s)
}

// ── Thermal governor + breaker (#2110/#2109) ──
// `env(DARKMUX_THERMAL_*) > config.runtime.thermal.* > default`, mirroring
// `turn_delay_ms`'s wiring. `enabled` defaults to `true` — see
// `ThermalConfig`'s own doc for why this block is on-by-default rather than
// following the redis/audit off-by-default convention.

/// Whether the thermal governor + breaker are active at all.
pub fn thermal_enabled() -> bool {
    if let Some(s) = env_str("DARKMUX_THERMAL_ENABLED") {
        return !matches!(s.as_str(), "0" | "false" | "no");
    }
    config()
        .runtime
        .as_ref()
        .and_then(|r| r.thermal.as_ref())
        .and_then(|t| t.enabled)
        .unwrap_or(true)
}

/// OS thermal state at or above which the governor pauses. Default `"serious"`.
pub fn thermal_pause_at() -> String {
    env_str("DARKMUX_THERMAL_PAUSE_AT")
        .or_else(|| {
            config()
                .runtime
                .as_ref()
                .and_then(|r| r.thermal.as_ref())
                .and_then(|t| t.pause_at.clone())
        })
        .unwrap_or_else(|| "serious".to_string())
}

/// OS thermal state at or below which the governor is eligible to resume
/// (after `thermal_resume_hold_ms`). Default `"fair"`.
pub fn thermal_resume_at() -> String {
    env_str("DARKMUX_THERMAL_RESUME_AT")
        .or_else(|| {
            config()
                .runtime
                .as_ref()
                .and_then(|r| r.thermal.as_ref())
                .and_then(|t| t.resume_at.clone())
        })
        .unwrap_or_else(|| "fair".to_string())
}

/// How long (ms) the state must hold at/below `thermal_resume_at()` before
/// the governor clears the pause. Default `60000`.
pub fn thermal_resume_hold_ms() -> u64 {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.thermal.as_ref())
        .and_then(|t| t.resume_hold_ms);
    pick_parsed("DARKMUX_THERMAL_RESUME_HOLD_MS", cfg, Some(60_000)).unwrap()
}

/// Cap (ms) on one continuous pause episode before the governor hands off
/// to the breaker. Default `900000` (15 minutes).
pub fn thermal_max_pause_ms() -> u64 {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.thermal.as_ref())
        .and_then(|t| t.max_pause_ms);
    pick_parsed("DARKMUX_THERMAL_MAX_PAUSE_MS", cfg, Some(900_000)).unwrap()
}

/// Breaker floor: `cpu_speed_limit_pct` below this triggers the breaker
/// regardless of the named thermal state. Default `50`.
pub fn thermal_min_cpu_speed_limit_pct() -> u64 {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.thermal.as_ref())
        .and_then(|t| t.min_cpu_speed_limit_pct);
    pick_parsed("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", cfg, Some(50)).unwrap()
}

/// (#2110/#2109 review finding 7) How many CONSECUTIVE samples must read
/// `cpu_speed_limit_pct` below the floor before the breaker trips on that
/// signal — a lone sample below the floor is common noise (a brief DVFS
/// dip under a short burst), and tripping the breaker (a terminal,
/// operator-must-resume event) on one noisy reading is a worse failure
/// mode than a few extra seconds of detection latency. Does NOT apply to
/// the `critical` thermal-state check, which is a discrete OS-reported
/// state and trips immediately as before. Default `3`.
///
/// (N2, final re-check) Clamped to `.max(1)`: a configured `0` would
/// otherwise mean "trip on every sample regardless of the reading"
/// (`streak >= 0` is trivially true before any low sample is ever seen) —
/// the opposite of "disabled." `0` behaves like `1` instead: trips on the
/// first genuinely low sample. See [`thermal_speed_limit_hold_samples_raw`]
/// for the unclamped value `darkmux doctor` warns against.
pub fn thermal_speed_limit_hold_samples() -> u32 {
    thermal_speed_limit_hold_samples_raw().max(1)
}

/// The resolved `runtime.thermal.speed_limit_hold_samples` value WITHOUT
/// the `.max(1)` floor — exists only so `darkmux doctor` can tell the
/// operator their explicit `0` was silently coerced to `1` rather than
/// achieving "disable" semantics (there is no way to fully disable this
/// signal short of disabling the thermal governor overall). Every other
/// caller wants [`thermal_speed_limit_hold_samples`], the clamped one.
pub fn thermal_speed_limit_hold_samples_raw() -> u32 {
    let cfg = config()
        .runtime
        .as_ref()
        .and_then(|r| r.thermal.as_ref())
        .and_then(|t| t.speed_limit_hold_samples);
    pick_parsed("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", cfg, Some(3)).unwrap()
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
/// `env(DARKMUX_FLOWS_DIR) > config.dirs.flows > <darkmux root>/flows`. The
/// root comes from `paths::resolve(Auto)` (below), so a HOME-less
/// environment falls back to `paths::resolve`'s own `/tmp` scoping rather
/// than a separate literal here.
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
/// (#2359) Derived from the SAME root resolution every other darkmux
/// directory resolves through — `paths::resolve(Auto)`, which honors
/// `DARKMUX_HOME` and a project-local `./.darkmux` before `~/.darkmux` —
/// mirroring `findings_dir_default`/`mods_dir_default`/`lab_dir_default`/
/// `hooks_outbox_dir_default`. Before this fix the default went straight to
/// `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped launch with no
/// `DARKMUX_FLOWS_DIR` override still wrote flow records into the operator's
/// REAL `~/.darkmux/flows` regardless — the same bug class #1585 fixed for
/// `lab_dir` and #2265 fixed for `findings_dir`/`mods_dir`, one directory
/// over. Four synthetic reviewer-probe missions leaked into the operator's
/// real flow store exactly this way on 2026-09-05.
#[cfg(not(any(test, feature = "test-support")))]
fn flows_dir_default() -> std::path::PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::Auto).root.join("flows")
}

/// (#994) In test / `test-support` builds the default must NOT be the
/// operator's real `~/.darkmux/flows`. Derived consumers now READ the flow
/// stream during a rebuild — the crew index's `cautions` derive scans
/// `flows_dir()` — so any test that doesn't explicitly set `DARKMUX_FLOWS_DIR`
/// would ingest live operator flow data (machine-dependent, and a ~50 MB scan
/// on CI). Isolating the default to a throwaway path makes an un-set flows dir
/// empty by construction; tests that need real content set `DARKMUX_FLOWS_DIR`
/// (the env tier, which wins). Same #811-style "empty operator state by
/// construction in test builds" move the empty `config()` tier already makes.
///
/// (#2359) But a test that DID isolate itself — by pointing `DARKMUX_HOME` at
/// a tempdir, or via a project-local `./.darkmux` — is honored verbatim, same
/// isolation discipline as `lab_dir_default`'s own test-build variant: a test
/// that isolated itself means it. Only a test that isolated NOTHING (so
/// `paths::resolve` would otherwise land on the real user root) falls back to
/// the throwaway path. This was previously unconditional, which is exactly
/// why `the retired review funnel launcher_sigterm_mid_probe_finalizes_and_reaps_curl`'s
/// own comment (tests/cli.rs) notes "flow records do NOT follow DARKMUX_HOME
/// at all" and sets `DARKMUX_FLOWS_DIR` explicitly as a workaround — it no
/// longer needs to, though existing explicit overrides remain harmless (env
/// still wins).
#[cfg(any(test, feature = "test-support"))]
fn flows_dir_default() -> std::path::PathBuf {
    let resolved = crate::paths::resolve(crate::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return std::path::PathBuf::from("/tmp/darkmux-test-isolated/flows");
    }
    resolved.root.join("flows")
}

/// (#2265) The finding-record store — `env(DARKMUX_FINDINGS_DIR) >
/// config.dirs.findings > <darkmux root>/findings`, the same three-tier shape
/// every sibling dir resolves through.
///
/// One `<dispatch>/<seq>/finding.json` per accepted `create_finding` call. The
/// flow stream remains the audit trail; this directory is the queryable copy
/// (`finding list` / `finding show`), so JSON on disk is the truth the same way
/// it is for roles.
pub fn findings_dir() -> std::path::PathBuf {
    pick_dir(
        env_str("DARKMUX_FINDINGS_DIR"),
        config().dirs.as_ref().and_then(|d| d.findings.as_deref()),
        findings_dir_default,
    )
}

/// Derived from the SAME root resolution every other darkmux directory
/// resolves through — `paths::resolve(Auto)`, which honors `DARKMUX_HOME` and
/// a project-local `./.darkmux` before `~/.darkmux` — mirroring
/// `hooks_outbox_dir_default`. Reaching straight for `dirs::home_dir()` here
/// would put a `DARKMUX_HOME`-scoped install's findings in the operator's real
/// `~/.darkmux/findings`, the bug class #1585 fixed one directory over.
#[cfg(not(any(test, feature = "test-support")))]
fn findings_dir_default() -> std::path::PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::Auto).root.join("findings")
}

/// Test builds must never default onto the operator's real
/// `~/.darkmux/findings` — same isolation discipline as `lab_dir_default`'s own
/// test-build variant (#994). A test that DID isolate itself (a `DARKMUX_HOME`
/// tempdir, or a project-local `./.darkmux`) is honored verbatim, because a
/// test that isolated itself means it.
#[cfg(any(test, feature = "test-support"))]
fn findings_dir_default() -> std::path::PathBuf {
    let resolved = crate::paths::resolve(crate::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return std::path::PathBuf::from("/tmp/darkmux-test-isolated/findings");
    }
    resolved.root.join("findings")
}

/// (#2265) The mod-record store — `env(DARKMUX_MODS_DIR) > config.dirs.mods >
/// <darkmux root>/mods`, the same three-tier shape as `findings_dir`.
///
/// One `<key>/mod.json` per mod, plus that mod's `attachments/`. A mod is a
/// KIT — instructions plus data, in whatever form the proposer chose. darkmux
/// never types a kit and never opens it; this accessor only says where the
/// kits live.
pub fn mods_dir() -> std::path::PathBuf {
    pick_dir(
        env_str("DARKMUX_MODS_DIR"),
        config().dirs.as_ref().and_then(|d| d.mods.as_deref()),
        mods_dir_default,
    )
}

/// Derived from the SAME root resolution every other darkmux directory
/// resolves through — `paths::resolve(Auto)`, which honors `DARKMUX_HOME` and
/// a project-local `./.darkmux` before `~/.darkmux`. Mirrors
/// `findings_dir_default`.
#[cfg(not(any(test, feature = "test-support")))]
fn mods_dir_default() -> std::path::PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::Auto).root.join("mods")
}

/// Test builds must never default onto the operator's real `~/.darkmux/mods`
/// — same isolation discipline as `findings_dir_default`'s own test-build
/// variant. A test that DID isolate itself (a `DARKMUX_HOME` tempdir, or a
/// project-local `./.darkmux`) is honored verbatim, because a test that
/// isolated itself means it.
#[cfg(any(test, feature = "test-support"))]
fn mods_dir_default() -> std::path::PathBuf {
    let resolved = crate::paths::resolve(crate::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return std::path::PathBuf::from("/tmp/darkmux-test-isolated/mods");
    }
    resolved.root.join("mods")
}

/// (#1585) The lab-run scan root — `env(DARKMUX_LAB_DIR) > config.dirs.lab >
/// ~/.darkmux/runs`, the same three-tier shape as its nine sibling dirs.
///
/// It has a real DEFAULT now, where before it had none and resolved to
/// `None`. That absence was the bug: `/lab/runs` answered
/// `{"configured": false}` and `/runs`' lab arm never ran, so 247 on-disk runs
/// were invisible in every surface. Defaulting is safe here in a way a
/// general "guess the operator's directory" would not be — `~/.darkmux/runs`
/// is darkmux-owned by construction (the namespace convention), so reading it
/// assumes nothing about user state.
///
/// A caller that wants "only if the operator named one" should compare against
/// this default explicitly rather than reintroducing an `Option` — the point of
/// #1585 is that unset must not silently mean absent.
pub fn lab_dir() -> std::path::PathBuf {
    pick_dir(
        env_str("DARKMUX_LAB_DIR"),
        config().dirs.as_ref().and_then(|d| d.lab.as_deref()),
        lab_dir_default,
    )
}

/// Derived from the SAME root resolution lab runs are written through —
/// `paths::resolve(Auto)`, which honors `DARKMUX_HOME` and a project-local
/// `./.darkmux` before `~/.darkmux`.
///
/// Deliberately not a hardcoded `~/.darkmux/runs`: that would reintroduce this
/// issue's own bug class one layer down. Under `DARKMUX_HOME=/x`, a lab run
/// WRITES to `/x/runs` while a hardcoded reader scans `~/.darkmux/runs` — the
/// run is invisible again, and now worse than before, because `/lab/runs`
/// would report `configured: true, exists: true` and imply everything is
/// wired. Sharing the resolver makes read and write incapable of disagreeing.
#[cfg(not(any(test, feature = "test-support")))]
fn lab_dir_default() -> std::path::PathBuf {
    crate::paths::resolve(crate::paths::ResolveScope::Auto).runs
}

/// Test builds must never default onto the operator's real `~/.darkmux/runs`
/// — same isolation discipline as `flows_dir_default` (#994).
///
/// But a test that DID isolate itself, by pointing `DARKMUX_HOME` at a
/// tempdir, means it: honor that exactly as production does. An earlier cut
/// returned the throwaway path unconditionally, which silently overrode
/// per-test isolation — `lab run list` then scanned the tmp path while the
/// test had written under its own `DARKMUX_HOME`, and the two could never
/// agree. The throwaway is the fallback for tests that isolated NOTHING, not
/// a replacement for tests that did.
#[cfg(any(test, feature = "test-support"))]
fn lab_dir_default() -> std::path::PathBuf {
    let resolved = crate::paths::resolve(crate::paths::ResolveScope::Auto);
    // Substitute the throwaway ONLY when resolution actually landed on the
    // operator's real user root — that is the case this guard exists for. Both
    // documented isolation forms (a `DARKMUX_HOME` tempdir, or a project-local
    // `./.darkmux` reached via `set_current_dir`) resolve elsewhere and are
    // honored verbatim, because a test that isolated itself means it.
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return std::path::PathBuf::from("/tmp/darkmux-test-isolated/runs");
    }
    resolved.runs
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
        // empty/whitespace cfg is treated as unset (falls through) — a
        // "visible but blank" field (e.g. `"radio": { "router_profile": "" }`)
        // defers to default.
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

    // ── pick_parsed_with_source (#2165): same precedence as pick_parsed,
    //    plus WHICH tier won ──
    #[serial_test::serial]
    #[test]
    fn pick_parsed_with_source_names_the_winning_tier() {
        let k = "DARKMUX_TEST_PICK_PARSED_SOURCE";
        unsafe { std::env::remove_var(k); }
        assert_eq!(pick_parsed_with_source::<u64>(k, None, Some(600)), (Some(600), Source::BuiltIn));
        assert_eq!(pick_parsed_with_source::<u64>(k, Some(120), Some(600)), (Some(120), Source::Config));
        unsafe { std::env::set_var(k, "90"); }
        assert_eq!(pick_parsed_with_source::<u64>(k, Some(120), Some(600)), (Some(90), Source::Env));
        // unparseable env falls through to cfg, same as pick_parsed.
        unsafe { std::env::set_var(k, "not-a-number"); }
        assert_eq!(pick_parsed_with_source::<u64>(k, Some(120), Some(600)), (Some(120), Source::Config));
        unsafe { std::env::remove_var(k); }
        assert_eq!(pick_parsed_with_source::<u64>(k, None, None), (None, Source::BuiltIn));
    }

    #[test]
    fn source_as_str_matches_the_spec_strings() {
        assert_eq!(Source::BuiltIn.as_str(), "built-in");
        assert_eq!(Source::Config.as_str(), "config");
        assert_eq!(Source::Env.as_str(), "env");
    }

    // ── representative `_with_source` accessors honor the env layer live,
    //    same property `redis_stream_env_override_wins_live` below pins for
    //    the value-only accessors (#2165) ──
    #[serial_test::serial]
    #[test]
    fn inactivity_timeout_seconds_with_source_env_overrides_then_built_in() {
        let k = "DARKMUX_INACTIVITY_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        assert_eq!(
            inactivity_timeout_seconds_with_source(),
            (600, Source::BuiltIn),
            "no env, empty test config → the built-in default, tagged built-in"
        );
        unsafe { std::env::set_var(k, "120"); }
        assert_eq!(
            inactivity_timeout_seconds_with_source(),
            (120, Source::Env),
            "env override wins the value AND the source"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn max_tokens_per_call_with_source_env_overrides_then_built_in() {
        let k = "DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        assert_eq!(
            max_tokens_per_call_with_source(),
            (None, Source::BuiltIn),
            "unset everywhere → None, tagged built-in (the runtime's own literal default)"
        );
        unsafe { std::env::set_var(k, "4000"); }
        assert_eq!(max_tokens_per_call_with_source(), (Some(4000), Source::Env));
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn generation_checkpoint_interval_tokens_with_source_env_overrides_then_built_in() {
        let k = "DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        assert_eq!(
            generation_checkpoint_interval_tokens_with_source(),
            (None, Source::BuiltIn),
            "unset everywhere → None, tagged built-in (the runtime's own literal default, 4000)"
        );
        unsafe { std::env::set_var(k, "2500"); }
        assert_eq!(generation_checkpoint_interval_tokens_with_source(), (Some(2500), Source::Env));
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn feedback_injection_with_source_env_overrides_then_built_in() {
        let k = "DARKMUX_FEEDBACK_INJECTION";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k); }
        assert_eq!(feedback_injection_with_source(), (true, Source::BuiltIn));
        unsafe { std::env::set_var(k, "0"); }
        assert_eq!(feedback_injection_with_source(), (false, Source::Env));
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
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

    // ── step_command_timeout_seconds (#2361, swarm S4-4): env > config >
    //    600 default, mirroring model_load_timeout_seconds exactly ──
    #[serial_test::serial]
    #[test]
    fn step_command_timeout_env_overrides_then_default() {
        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        assert_eq!(step_command_timeout_seconds(), 600);
        unsafe { std::env::set_var(k, "2") };
        assert_eq!(step_command_timeout_seconds(), 2, "env wins live");
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(step_command_timeout_seconds(), 600);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── dispatch_free_concurrency (#2394): env > config > 8 default,
    //    mirroring remote_concurrent_cap exactly ──
    #[serial_test::serial]
    #[test]
    fn dispatch_free_concurrency_env_overrides_then_default() {
        let k = "DARKMUX_DISPATCH_FREE_CONCURRENCY";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        assert_eq!(dispatch_free_concurrency(), 8);
        unsafe { std::env::set_var(k, "3") };
        assert_eq!(dispatch_free_concurrency(), 3, "env wins live");
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(dispatch_free_concurrency(), 8, "an unparseable env value falls through, never panics");
        // A 0 would mean "run nothing, forever" — clamped, never honored.
        unsafe { std::env::set_var(k, "0") };
        assert_eq!(dispatch_free_concurrency(), 1, "0 is clamped to 1, not taken literally");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── turn_delay_ms (#2094): env > config > 0 default, mirroring
    //    model_load_timeout_seconds' resolution exactly ──
    #[serial_test::serial]
    #[test]
    fn turn_delay_ms_env_overrides_then_default() {
        let k = "DARKMUX_TURN_DELAY_MS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        // No env + the empty test config (#811) → the built-in 0 default (no rest).
        assert_eq!(turn_delay_ms(), 0);
        unsafe { std::env::set_var(k, "3000") };
        assert_eq!(turn_delay_ms(), 3000, "env wins live");
        // An unparseable env value falls through (here, to the default).
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(turn_delay_ms(), 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── host_sampler_interval_ms (#2107, #1833): env > config > 5000
    //    default, mirroring turn_delay_ms's resolution exactly ──
    #[serial_test::serial]
    #[test]
    fn host_sampler_interval_ms_env_overrides_then_default() {
        let k = "DARKMUX_HOST_SAMPLER_INTERVAL_MS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        // No env + the empty test config (#811) → the built-in 5000ms default.
        assert_eq!(host_sampler_interval_ms(), 5000);
        unsafe { std::env::set_var(k, "2000") };
        assert_eq!(host_sampler_interval_ms(), 2000, "env wins live");
        // `0` is a real, honored value — the explicit disable.
        unsafe { std::env::set_var(k, "0") };
        assert_eq!(host_sampler_interval_ms(), 0, "0 disables the sampler");
        // An unparseable env value falls through (here, to the default).
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(host_sampler_interval_ms(), 5000);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── telemetry_record_every_samples (#2111): env > config > 5 default,
    //    mirroring host_sampler_interval_ms's resolution exactly ──
    #[serial_test::serial]
    #[test]
    fn telemetry_record_every_samples_env_overrides_then_default() {
        let k = "DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        // No env + the empty test config (#811) → the built-in 5-sample default.
        assert_eq!(telemetry_record_every_samples(), 5);
        unsafe { std::env::set_var(k, "10") };
        assert_eq!(telemetry_record_every_samples(), 10, "env wins live");
        // `0` is a real, honored value — the explicit disable.
        unsafe { std::env::set_var(k, "0") };
        assert_eq!(telemetry_record_every_samples(), 0, "0 disables the periodic curve");
        // An unparseable env value falls through (here, to the default).
        unsafe { std::env::set_var(k, "not-a-number") };
        assert_eq!(telemetry_record_every_samples(), 5);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// Ships objective: the RADIO persona's humor is low out of the box and
    /// the dial is there for anyone who wants the personality (operator,
    /// 2026-08-28: "65 is too high for default"). One constant, one test,
    /// so three copies of the number cannot drift apart again.
    #[serial_test::serial]
    #[test]
    fn radio_humor_default_when_unset_is_the_middle_of_the_dial() {
        let prev = std::env::var("DARKMUX_RADIO_HUMOR").ok();
        unsafe { std::env::remove_var("DARKMUX_RADIO_HUMOR"); }
        assert_eq!(u64::from(radio_humor()), RADIO_HUMOR_DEFAULT);
        assert_eq!(RADIO_HUMOR_DEFAULT, 50);
        if let Some(v) = prev { unsafe { std::env::set_var("DARKMUX_RADIO_HUMOR", v); } }
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

    /// (#2359) `flows_dir` must scope under `DARKMUX_HOME`, exactly as its
    /// sibling dirs (`findings_dir`, `mods_dir`, `lab_dir`, `hooks_outbox_dir`)
    /// already do — mirrors `hooks_outbox_dir_honors_darkmux_home` exactly.
    /// Before this fix, `flows_dir_default` went straight to
    /// `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped launch with no
    /// `DARKMUX_FLOWS_DIR` override still wrote flow records into the
    /// OPERATOR'S REAL `~/.darkmux/flows` — four synthetic reviewer-probe
    /// missions leaked into the operator's store exactly this way on
    /// 2026-09-05.
    #[serial_test::serial]
    #[test]
    fn flows_dir_honors_darkmux_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_FLOWS_DIR");
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let dir = flows_dir();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(
            dir,
            tmp.path().join("flows"),
            "must scope under DARKMUX_HOME, not the real user home"
        );
    }

    /// (#2265) `dirs.findings` resolves through the same three tiers as every
    /// sibling dir: `env(DARKMUX_FINDINGS_DIR) > config.dirs.findings >
    /// <root>/findings`. The config tier is exercised through `pick_dir`
    /// directly (the process-wide `config()` is the empty test tier by
    /// construction, #811), the way the sibling `pick_dir` test does.
    #[serial_test::serial]
    #[test]
    fn findings_dir_env_then_config_then_default() {
        let prev = std::env::var("DARKMUX_FINDINGS_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_FINDINGS_DIR", "/custom/findings");
        }
        assert_eq!(findings_dir(), std::path::PathBuf::from("/custom/findings"));

        unsafe {
            std::env::remove_var("DARKMUX_FINDINGS_DIR");
        }
        // config tier beats the built-in default; env (absent) does not shadow it.
        assert_eq!(
            pick_dir(None, Some("/cfg/findings"), findings_dir_default),
            std::path::PathBuf::from("/cfg/findings")
        );
        // Unset everywhere → a real path under the darkmux root, never nothing.
        assert!(
            findings_dir().ends_with("findings"),
            "unset must resolve to a findings dir, not nothing"
        );
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("DARKMUX_FINDINGS_DIR", v);
            }
        }
    }

    /// (#2265) `dirs.mods` resolves through the same three tiers as
    /// `dirs.findings`: `env(DARKMUX_MODS_DIR) > config.dirs.mods >
    /// <root>/mods`. Same shape, same reason.
    #[serial_test::serial]
    #[test]
    fn mods_dir_env_then_config_then_default() {
        let prev = std::env::var("DARKMUX_MODS_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_MODS_DIR", "/custom/mods");
        }
        assert_eq!(mods_dir(), std::path::PathBuf::from("/custom/mods"));

        unsafe {
            std::env::remove_var("DARKMUX_MODS_DIR");
        }
        // config tier beats the built-in default; env (absent) does not shadow it.
        assert_eq!(
            pick_dir(None, Some("/cfg/mods"), mods_dir_default),
            std::path::PathBuf::from("/cfg/mods")
        );
        // Unset everywhere → a real path under the darkmux root, never nothing.
        assert!(
            mods_dir().ends_with("mods"),
            "unset must resolve to a mods dir, not nothing"
        );
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("DARKMUX_MODS_DIR", v);
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn lab_dir_defaults_when_unset_rather_than_resolving_to_nothing() {
        // THE #1585 regression. `lab_dir` was the only directory setting with
        // no config tier and no default, so unset resolved to `None`, the
        // `/runs` lab arm never ran, and 247 on-disk runs were invisible with
        // nothing reporting a missing source. Unset must yield a real path.
        let prev = std::env::var("DARKMUX_LAB_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_LAB_DIR");
        }
        assert!(lab_dir().ends_with("runs"), "unset must resolve to a runs dir, not nothing");
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("DARKMUX_LAB_DIR", v);
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn lab_dir_env_overrides_the_default() {
        // Env stays the top tier, same as its nine sibling dirs.
        let prev = std::env::var("DARKMUX_LAB_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_LAB_DIR", "/custom/lab");
        }
        assert_eq!(lab_dir(), std::path::PathBuf::from("/custom/lab"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LAB_DIR", v),
                None => std::env::remove_var("DARKMUX_LAB_DIR"),
            }
        }
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
    fn cmd_enabled_env_truthy_then_default_false() {
        let prev = std::env::var("DARKMUX_CMD_ENABLED").ok();
        for truthy in ["1", "true", "YES", "On"] {
            unsafe { std::env::set_var("DARKMUX_CMD_ENABLED", truthy); }
            assert!(cmd_enabled(), "{truthy} → true (case-insensitive)");
        }
        unsafe { std::env::set_var("DARKMUX_CMD_ENABLED", "nope"); }
        assert!(!cmd_enabled(), "non-truthy → false");
        unsafe { std::env::remove_var("DARKMUX_CMD_ENABLED"); }
        assert!(!cmd_enabled(), "unset → false default (fail closed)");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn cmd_allowed_verbs_env_comma_separated_then_default_empty() {
        let prev = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe { std::env::set_var("DARKMUX_CMD_ALLOWED", " pr-list, pr-merge ,,pr-approve"); }
        assert_eq!(
            cmd_allowed_verbs(),
            vec!["pr-list".to_string(), "pr-merge".to_string(), "pr-approve".to_string()],
            "trimmed, empty entries dropped"
        );
        unsafe { std::env::remove_var("DARKMUX_CMD_ALLOWED"); }
        assert!(cmd_allowed_verbs().is_empty(), "unset → empty default");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn cmd_allowed_requires_both_enabled_and_named() {
        let prev_enabled = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        // Neither set — fails closed.
        unsafe {
            std::env::remove_var("DARKMUX_CMD_ENABLED");
            std::env::remove_var("DARKMUX_CMD_ALLOWED");
        }
        assert!(!cmd_allowed("pr-merge"), "gate off entirely → refused");
        // Allowlisted but the gate itself is off — still refused.
        unsafe { std::env::set_var("DARKMUX_CMD_ALLOWED", "pr-merge"); }
        assert!(!cmd_allowed("pr-merge"), "enabled=false blocks regardless of allowed");
        // Both set, but a DIFFERENT verb — refused.
        unsafe { std::env::set_var("DARKMUX_CMD_ENABLED", "true"); }
        assert!(!cmd_allowed("pr-approve"), "named elsewhere in the allowlist, not this verb");
        // Both set, matching verb — allowed.
        assert!(cmd_allowed("pr-merge"), "enabled + named → allowed");
        unsafe {
            match prev_enabled {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
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

    /// (#1548) `feedback_injection` mirrors `check_updates`'s opt-out shape:
    /// falsy env disables, any other value (or unset) leaves it on by
    /// default. Before #1548 this accessor didn't exist at all — the
    /// `config.json` tier was never consulted by anything, so this test's
    /// mere existence is part of the fix (a RED run pre-fix is "function not
    /// found", not a failing assertion).
    #[serial_test::serial]
    #[test]
    fn feedback_injection_env_optout_then_default_on() {
        let prev = std::env::var("DARKMUX_FEEDBACK_INJECTION").ok();
        for off in ["0", "off", "false", "no"] {
            unsafe { std::env::set_var("DARKMUX_FEEDBACK_INJECTION", off); }
            assert!(!feedback_injection(), "{off} → disabled");
        }
        unsafe { std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "1"); }
        assert!(feedback_injection(), "non-falsy value → on");
        unsafe { std::env::remove_var("DARKMUX_FEEDBACK_INJECTION"); }
        assert!(feedback_injection(), "unset → on (default)");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FEEDBACK_INJECTION", v),
                None => std::env::remove_var("DARKMUX_FEEDBACK_INJECTION"),
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

    // ── (#2093) Hooks — env/default tier only (the config tier is empty by
    // construction in test builds; per-rule config-tier behavior is tested
    // in darkmux-flow against real `HookRule`/`HookMatch` values instead). ──

    #[serial_test::serial]
    #[test]
    fn hooks_enabled_env_truthy_then_default_false() {
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        for truthy in ["1", "true", "YES", "On"] {
            unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", truthy); }
            assert!(hooks_enabled(), "{truthy} → true (case-insensitive)");
        }
        unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", "nope"); }
        assert!(!hooks_enabled(), "non-truthy → false");
        unsafe { std::env::remove_var("DARKMUX_HOOKS_ENABLED"); }
        assert!(!hooks_enabled(), "unset → false default (fail closed)");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    #[test]
    fn hooks_outbox_dir_default_is_darkmux_hooks() {
        // No config tier in test builds → default only. Lenient suffix
        // (matches `lab_dir`'s own convention) since the test-isolated
        // fallback path differs from the real `~/.darkmux/hooks` shape.
        let dir = hooks_outbox_dir();
        assert!(dir.ends_with("hooks"), "default outbox dir should end in a `hooks` dir, got {}", dir.display());
    }

    /// (#2093 merge-gate finding 8) `hooks_outbox_dir` resolves under
    /// `paths::resolve(Auto)` — same root every other darkmux directory
    /// resolves under — so `DARKMUX_HOME` scopes it too. Before this fix
    /// it went straight to `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped
    /// install (a relocated root, or test isolation) still wrote hook
    /// outbox files to the OPERATOR'S REAL `~/.darkmux/hooks` regardless.
    #[serial_test::serial]
    #[test]
    fn hooks_outbox_dir_honors_darkmux_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let dir = hooks_outbox_dir();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(dir, tmp.path().join("hooks"), "must scope under DARKMUX_HOME, not the real user home");
    }

    #[test]
    fn hooks_rules_empty_by_default() {
        assert!(hooks_rules().is_empty(), "no config tier in test builds → no rules");
    }

    // ─── (#2093 merge-gate finding 5) hard outbox cap ────────────────────

    #[test]
    fn hooks_max_outbox_mb_defaults_to_256() {
        assert_eq!(hooks_max_outbox_mb(), 256, "no config tier in test builds → built-in default");
    }

    #[serial_test::serial]
    #[test]
    fn hooks_max_outbox_mb_env_overrides_the_default() {
        let prev = std::env::var("DARKMUX_HOOKS_MAX_OUTBOX_MB").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", "5");
        }
        assert_eq!(hooks_max_outbox_mb(), 5);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", v),
                None => std::env::remove_var("DARKMUX_HOOKS_MAX_OUTBOX_MB"),
            }
        }
    }
}
