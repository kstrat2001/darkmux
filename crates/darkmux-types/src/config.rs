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
// 1.1 (#933): additive `fleet{}` block (fleet.mode). Minor bump — an older
// binary tolerates it (all-Option + `extras` overflow), per the lenient-read
// doctrine.
// 1.2 (#1260/#1177): additive `remote{}` block (remote.max_tokens_per_execution
// — the per-pipeline-stage remote token allowance for endpoint-staffed crew
// seats). Minor bump, same lenient-read reasoning.
pub const CONFIG_SCHEMA_VERSION: &str = "1.2";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,

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
    /// (#1276) Bounded model-load/unload phase for gestalt host-port calls:
    /// the `LmsHost` adapter hard-kills the `lms load`/`lms unload` child at
    /// expiry and surfaces a typed timeout naming the phase — a wrong model
    /// id can no longer hang a dispatch until the workflow's outer kill.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub model_load_timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens: Option<u32>,
    /// (#1221) Per-CALL completion-token cap (reasoning + content of one
    /// model turn). Absent = the runtime's built-in default (10000) — which
    /// E19 measured truncating PRODUCTIVE reasoning on thinking-family
    /// models, so benches raise it explicitly per run.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens_per_call: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub strict_selection: Option<bool>,
    // (#1311) Verbosity for the diagnostic surfaces. `"info"` (default) emits
    // the informative dispatch-liveness phase markers; `"debug"` additionally
    // logs per-call detail (hosted call host/model/tokens/wall_ms). NEVER
    // carries a secret at any level. Resolved via `config_access::log_level`
    // (`env(DARKMUX_LOG) > this > "info"`); surfaced by `darkmux doctor`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub log_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub feedback_injection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub default_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub check_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub daemon_cors_origins: Option<String>,
    // (#881) Gate for reading the `darkmux-serve-token` Keychain item (the env
    // token `DARKMUX_SERVE_TOKEN` needs no gate). Visible `false` so the
    // security toggle is discoverable; the token itself is NEVER a config field.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub daemon_auth_enabled: Option<bool>,
    // (#1011) Fraction (0–1) of the dispatch model's context window budgeted for
    // the injected-context blocks (detector cautions + authored lessons + prior
    // corrections) in the coder brief. A fraction auto-scales across profiles
    // from one value — a large-window profile gets proportionally more room.
    // `env(DARKMUX_INJECTED_CONTEXT_FRACTION) > this > 0.15`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub injected_context_fraction: Option<f64>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Fleet position (#933) — the machine's declared place in a multi-node fleet,
/// a `fleet{}` block beside `redis{}`/`audit{}`/`runtime{}`. The operator
/// **declares** `mode`; detection (a machine running Redis + the always-on
/// daemon looks like a hub) is only a `darkmux doctor` cross-check that flags
/// declared ≠ observed — never the source of truth (operator sovereignty).
/// Downstream work keys on it: the turnkey hub supervises its own Redis when
/// `mode: hub` (#936); `doctor --fleet` uses it for two-hub split-brain
/// detection (#935). `darkmux init` writes `mode: "standalone"` visible, so the
/// fleet surface is discoverable and one edit from `hub`/`peer`.
///
/// `mode` is stored as a **string, not a typed enum**, deliberately: the
/// lenient-read doctrine says a typo'd value must never fail the whole-config
/// parse (which would brick every setting). The raw token is kept so `darkmux
/// doctor` can flag it against what the operator actually wrote (#934);
/// `FleetMode::parse` does the typed interpretation at the accessor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mode: Option<String>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#1260/#1177) Remote (hosted-endpoint) dispatch knobs — the config home
/// for the per-execution token bucket the review funnel enforces on
/// endpoint-staffed seats. Unlike `redis{}`/`audit{}` there is NO `enabled`
/// gate: remote staffing is enabled by the profile itself (endpoint present
/// on the staffing's model — contract 1, profile uniformity), so the block
/// carries only the allowance knob. `darkmux init` writes it visible with
/// the default populated, per the visible-defaults doctrine.
///
/// **What an "execution" is (operator decision, 2026-07-10 design chat):**
/// one pipeline stage — the funnel's probe pass, each judge pass, the
/// verify pass; a bare `crew dispatch` is one execution. Each stage's
/// REMOTE calls draw from their own allowance, so a runaway stage is caught
/// at the cap without starving later stages. Tokens only — never currency.
///
/// **Which paths this meters (1.18.0 scope — be precise):** the review
/// funnel's remote seats (probe / judge-pass1 / judge-pass2 / verify) AND the
/// tool-less single-shot remote `crew dispatch` path (`dispatch_remote`). The
/// AGENTIC-remote container path (#1187 — a tool-granting role on an endpoint
/// profile, driven by the multi-call container loop) is NOT metered by this
/// bucket in 1.18.0; metering that loop is tracked as a follow-up. A path
/// this bucket does not yet meter is documented as such, never silently
/// counted "off the meter".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Max remote `total_tokens` one pipeline stage may spend (default
    /// 500000). When a stage exhausts it, that stage's remaining remote
    /// calls stop with the reason named in the run's envelope: a
    /// load-bearing stage (judge/verify) exhausting is an honest degraded
    /// run; probe exhaustion is a reduced-coverage warning.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens_per_execution: Option<u64>,
    /// (#1230 Packet 1) Max CONCURRENT remote (hosted-endpoint) dispatches
    /// `darkmux_crew::concurrent_dispatch::run_bounded` runs at once — remote
    /// jobs aren't RAM-bound (gestalt's wave scheduler only governs LOCAL
    /// co-residency), so they run in their own separately-capped batch
    /// instead of being serialized behind local waves. Default 4 is a
    /// placeholder pending an operator call informed by real Azure/hosted
    /// rate-limit tiers — unlike `max_tokens_per_execution` this is not yet
    /// empirically tuned.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub concurrent_cap: Option<u32>,
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// A machine's declared fleet position. `Standalone` (default) = a
/// single-machine install with no fleet; `Hub` = the always-on coordinator
/// (and, per #936, supervises its own Redis); `Peer` = points at a hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FleetMode {
    #[default]
    Standalone,
    Hub,
    Peer,
}

impl FleetMode {
    /// The canonical lowercase token — the `config.json` value and the
    /// `DARKMUX_FLEET_MODE` env token.
    pub fn as_str(self) -> &'static str {
        match self {
            FleetMode::Standalone => "standalone",
            FleetMode::Hub => "hub",
            FleetMode::Peer => "peer",
        }
    }

    /// Parse an operator-declared token (trimmed, case-insensitive). Returns
    /// `None` for an unrecognized value — kept distinct from "standalone" so a
    /// caller (e.g. `darkmux doctor`, #934) can flag a typo against the raw
    /// string rather than this silently coercing it.
    pub fn parse(s: &str) -> Option<FleetMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Some(FleetMode::Standalone),
            "hub" => Some(FleetMode::Hub),
            "peer" => Some(FleetMode::Peer),
            _ => None,
        }
    }
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
    /// - caps (`max_turns`/`max_tokens`/`max_tokens_per_call`), `default_role`,
    ///   `daemon_cors_origins` — absent is a real behavior (uncapped / the
    ///   runtime's built-in per-call default), not a value to default.
    /// - `feedback_injection` — read in-container directly from
    ///   `DARKMUX_FEEDBACK_INJECTION` (the runtime crate can't depend on
    ///   `config_access`), so it does NOT yet honor the `config.json` tier
    ///   (#661). Writing it here would advertise a knob that silently no-ops;
    ///   omitted until the flag is plumbed. Absent = the env default (on).
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
                model_load_timeout_seconds: Some(600),
                max_turns: None,
                max_tokens: None,
                max_tokens_per_call: None,
                strict_selection: Some(false),
                log_level: Some("info".to_string()),
                feedback_injection: None,
                default_role: None,
                check_updates: Some(true),
                daemon_cors_origins: None,
                daemon_auth_enabled: Some(false),
                injected_context_fraction: Some(0.15),
                extras: Default::default(),
            }),
            fleet: Some(FleetConfig {
                mode: Some("standalone".to_string()),
                extras: Default::default(),
            }),
            remote: Some(RemoteConfig {
                max_tokens_per_execution: Some(500_000),
                concurrent_cap: Some(4),
                extras: Default::default(),
            }),
            extras: Default::default(),
        }
    }

    /// Load the config.json at the USER-scope location (`~/.darkmux/config.json`
    /// or `$DARKMUX_HOME`), never a project-local one. Missing or malformed →
    /// default-empty (loud validation belongs to `darkmux doctor`, not the hot
    /// load path; a bad config must never brick the CLI — accessors fall through
    /// to env/built-in defaults).
    ///
    /// (#1323) `ForceUser`, NOT `Auto`: config.json carries user/machine-level
    /// state (redis/audit/lms/machine_id) — there is no legitimate per-project
    /// config. Under `Auto`, the mere existence of a `<cwd>/.darkmux/` created
    /// for an unrelated purpose (project-tier missions/sprints/lessons) silently
    /// resolved the "home" to the project dir, defaulting redis+audit OFF — a
    /// real audit-trail hole on a self-hosted-runner checkout. Same shadowing
    /// class as #1012/#1016; this is the config/flow-sink resolution path.
    pub fn load_resolved() -> Self {
        let path = crate::paths::resolve(crate::paths::ResolveScope::ForceUser).config;
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

    /// (#1323) The config seam's self-defending conformance test: a project-local
    /// `.darkmux/config.json` (created for missions/sprints/lessons) must NEVER
    /// shadow the user-scope config. `DARKMUX_HOME` is UNSET on purpose — with it
    /// set, `paths::resolve` short-circuits to the same root for every scope, so
    /// Auto and ForceUser wouldn't diverge and this guard would be hollow. If
    /// `load_resolved` regresses to `ResolveScope::Auto`, it reads the project
    /// shadow → the marker → this fails.
    #[serial_test::serial]
    #[test]
    fn config_load_resolved_ignores_project_darkmux_shadow() {
        use std::env;
        let proj = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(proj.path().join(".darkmux")).unwrap();
        std::fs::write(
            proj.path().join(".darkmux").join("config.json"),
            r#"{"machine_id":"PROJECT-SHADOW-MUST-NOT-LOAD"}"#,
        )
        .unwrap();

        let prev_home = env::var("DARKMUX_HOME").ok();
        let prev_cwd = env::current_dir().unwrap();
        unsafe { env::remove_var("DARKMUX_HOME") };
        env::set_current_dir(proj.path()).unwrap();

        // Sanity: in THIS setup Auto and ForceUser genuinely diverge (Auto sees
        // the project shadow), so the guard below actually exercises the choice.
        let auto = crate::paths::resolve(crate::paths::ResolveScope::Auto).config;
        let force_user = crate::paths::resolve(crate::paths::ResolveScope::ForceUser).config;
        let cfg = DarkmuxConfig::load_resolved();

        // Restore env FIRST so a failed assert can't poison other serial tests.
        env::set_current_dir(prev_cwd).unwrap();
        match prev_home {
            Some(h) => unsafe { env::set_var("DARKMUX_HOME", h) },
            None => unsafe { env::remove_var("DARKMUX_HOME") },
        }

        assert_ne!(
            auto, force_user,
            "sanity: with a project .darkmux/ and no DARKMUX_HOME, Auto must diverge from ForceUser"
        );
        // The real guard: under the pre-#1323 `Auto`, load_resolved reads the
        // project shadow → the marker → FAIL. Under `ForceUser` it never does.
        assert_ne!(
            cfg.machine_id.as_deref(),
            Some("PROJECT-SHADOW-MUST-NOT-LOAD"),
            "#1323: load_resolved must ignore a project-local .darkmux/config.json"
        );
    }

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
        assert_eq!(cfg.orchestrator.as_deref(), Some(""), "visible but unset");
        // Fields where a written literal would be wrong stay absent.
        assert!(cfg.dirs.is_none(), "dirs are derived → surfaced by doctor, not frozen");
        assert!(cfg.runtime.as_ref().unwrap().max_turns.is_none(), "uncapped, not defaulted");
        assert!(
            cfg.runtime.as_ref().unwrap().feedback_injection.is_none(),
            "feedback_injection is env-only (not yet config_access-backed, #661) → omitted, not advertised"
        );
        // `enabled` reads at the TOP of each feature block.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            json.find("\"enabled\"").unwrap() < json.find("\"host\"").unwrap(),
            "enabled must precede the connection knobs"
        );
        // (#933) The fleet block is written visible at the standalone default,
        // so the fleet surface is discoverable + one edit from hub/peer.
        assert_eq!(cfg.fleet.as_ref().unwrap().mode.as_deref(), Some("standalone"));
        // (#1260) The remote block is written visible with the per-execution
        // token allowance populated — no `enabled` gate, since remote staffing
        // is enabled by the profile's own endpoint declaration (contract 1).
        assert_eq!(cfg.remote.as_ref().unwrap().max_tokens_per_execution, Some(500_000));
        // (#1230 Packet 1) The concurrent-dispatch remote cap, same
        // visible-default treatment as its token-allowance sibling.
        assert_eq!(cfg.remote.as_ref().unwrap().concurrent_cap, Some(4));
        // Lossless round-trip.
        let back: DarkmuxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.redis.as_ref().unwrap().enabled, Some(false));
        assert_eq!(back.audit.as_ref().unwrap().dir.as_deref(), Some("~/.darkmux/audit"));
        assert_eq!(back.fleet.as_ref().unwrap().mode.as_deref(), Some("standalone"));
        assert_eq!(back.remote.as_ref().unwrap().max_tokens_per_execution, Some(500_000));
        assert_eq!(back.remote.as_ref().unwrap().concurrent_cap, Some(4));
    }

    /// (#933) `FleetMode::parse` is lenient (trim + case-insensitive) and
    /// returns `None` for an unrecognized token so doctor can flag the typo
    /// rather than silently coercing it; `as_str` round-trips the canonical
    /// lowercase token.
    #[test]
    fn fleet_mode_parse_and_roundtrip() {
        assert_eq!(FleetMode::parse("hub"), Some(FleetMode::Hub));
        assert_eq!(FleetMode::parse("  PEER "), Some(FleetMode::Peer));
        assert_eq!(FleetMode::parse("standalone"), Some(FleetMode::Standalone));
        assert_eq!(FleetMode::parse("hubb"), None, "typo → None, not silently standalone");
        assert_eq!(FleetMode::default(), FleetMode::Standalone);
        for m in [FleetMode::Standalone, FleetMode::Hub, FleetMode::Peer] {
            assert_eq!(FleetMode::parse(m.as_str()), Some(m));
        }
    }

    /// (#933) A typo'd `fleet.mode` must NOT fail the whole-config parse (the
    /// lenient-read doctrine) — it lands as a plain string the accessor/doctor
    /// interpret, never bricking the other settings.
    #[test]
    fn bad_fleet_mode_does_not_brick_config() {
        let json = r#"{ "machine_id": "x", "fleet": { "mode": "hubb" } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("x"), "other fields still parse");
        assert_eq!(cfg.fleet.as_ref().unwrap().mode.as_deref(), Some("hubb"), "raw token preserved for doctor");
        assert_eq!(FleetMode::parse(cfg.fleet.unwrap().mode.as_deref().unwrap()), None);
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
            "orchestrator": "claude-code",
            "lms_bin": "/usr/local/bin/lms",
            "lmstudio_url": "http://localhost:1234",
            "dirs": { "flows": "~/dm/flows", "audit": "~/dm/audit" },
            "redis": { "host": "100.74.208.36", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
            "runtime": { "inactivity_timeout_seconds": 600, "max_turns": 40, "strict_selection": true, "daemon_auth_enabled": true }
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("studio"));
        assert_eq!(cfg.redis.as_ref().unwrap().host.as_deref(), Some("100.74.208.36"));
        assert_eq!(cfg.redis.as_ref().unwrap().port, Some(6379));
        assert_eq!(cfg.dirs.as_ref().unwrap().flows.as_deref(), Some("~/dm/flows"));
        assert_eq!(cfg.runtime.as_ref().unwrap().max_turns, Some(40));
        assert_eq!(cfg.runtime.as_ref().unwrap().strict_selection, Some(true));
        // (#881) the daemon-auth gate deserializes from the config tier.
        assert_eq!(cfg.runtime.as_ref().unwrap().daemon_auth_enabled, Some(true));
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
