//! (#2165) Bound provenance — which runtime knob a cap-hit/salvage/detector/
//! checkpoint-continuation/inactivity-warning record names, and where its
//! value came from.
//!
//! **The miss this closes:** a run emitted "⚡ per-turn-cap salvage —
//! completion_tokens=999 hit cap 1000" and a `telemetry.detector {kind:
//! per-turn-cap, ...}` record naming only the NUMBER. A remote reader could
//! not tell whether that 1000 was the #1221 reasoning check-in interval
//! (built-in) or `runtime.max_tokens_per_call` (an operator override), and
//! misdiagnosed it. The runtime already knows the answer at every emitting
//! site — `per_call_cap`/`sent_reasoning_bound` in `loop_runner.rs` say
//! WHICH region's bound was in force — it just never stamped it anywhere.
//!
//! **What the runtime does NOT know on its own:** whether the NUMBER it was
//! handed came from the operator's env, their `config.json`, or nobody set
//! anything (the runtime's own built-in constant). That's a host-side fact
//! (`darkmux_types::config_access`'s `_with_source` accessors resolve it —
//! but this crate is a standalone workspace, see `runtime/Cargo.toml`'s own
//! doc, and deliberately does not depend on `darkmux-types`). So the HOST
//! resolves the tier and forwards it alongside each value — a companion
//! `--<flag>-source <tier>` CLI arg for the CLI-flag-carried knobs, and a
//! companion `DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE` env var for the one
//! knob that already travels as an env var. `main.rs` parses those and calls
//! [`set_bound_sources`] once, before the loop runs; every emission site in
//! `loop_runner.rs` reads them back via [`bound_sources`].
//!
//! Every runtime test that calls `loop_runner::run`/`run_with_sleeper`
//! directly (35+ call sites, none of which go through `main.rs`) never calls
//! [`set_bound_sources`] — [`bound_sources`] then returns the all-`BuiltIn`
//! default, which is the CORRECT provenance for a test that supplied no CLI
//! source flag. This is why sources are process-global state instead of a
//! new parameter threaded through every `run*` signature: threading it would
//! touch 35+ test call sites (many owned by an in-flight sibling PR editing
//! the same cap-selection block) for a fact that is genuinely process-wide —
//! one dispatch, one resolved tier per knob, never changing mid-run.

use serde::Serialize;
use std::sync::OnceLock;

/// Which runtime bound a record names. Serializes to the exact snake_case
/// strings #2165 specifies.
///
/// `MaxTurns`/`MaxTokens` are part of the contract (the host's `bounds`
/// block on `dispatch start`/the envelope names both — see
/// `darkmux_types::config_access`'s `_with_source` accessors, consumed
/// host-side in `dispatch_internal.rs`) but this crate never constructs
/// them: neither cap has a per-hit runtime record in THIS pass (#2165 scope
/// is the four sites named in its own doc — salvage, intra-turn-stall,
/// checkpoint continuation, inactivity warning). `#[allow(dead_code)]`
/// documents that gap rather than dropping the variants and silently
/// narrowing the enum's contract.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BoundKind {
    ReasoningCheckpointInterval,
    /// (#2171 rebase onto #2165) Bounds any call that does NOT carry the
    /// reasoning bound above — restores a check-in for non-thinking models,
    /// which pre-#2171 carried `MaxTokensPerCall` (10000 by default) with
    /// no check-in at all once #2164 gated the reasoning check-in on
    /// `dispatch_has_reasoned`. See `runtime::loop_runner::
    /// GENERATION_CHECKPOINT_INTERVAL`'s doc for the incident this fixes.
    GenerationCheckpointInterval,
    MaxTokensPerCall,
    #[allow(dead_code)]
    MaxTurns,
    #[allow(dead_code)]
    MaxTokens,
    InactivityTimeout,
}

impl BoundKind {
    /// The human-readable clause a stderr line names it by, e.g. "the
    /// reasoning check-in interval" — so a human tailing the log reads the
    /// KNOB, not a bare number.
    fn label(self) -> &'static str {
        match self {
            BoundKind::ReasoningCheckpointInterval => "the reasoning check-in interval",
            BoundKind::GenerationCheckpointInterval => "the generation check-in interval",
            BoundKind::MaxTokensPerCall => "the per-call token cap",
            BoundKind::MaxTurns => "the max-turns cap",
            BoundKind::MaxTokens => "the cumulative max-tokens cap",
            BoundKind::InactivityTimeout => "the inactivity timeout",
        }
    }
}

/// Which tier resolved a bound's value. Serializes to `"built-in"` |
/// `"config"` | `"env"` | `"cli"`.
///
/// (#2480 review, finding 5) `Cli` is a fourth tier above `env > config >
/// built-in`, for a bound the operator named at the point of dispatch —
/// today only `darkmux dispatch --timeout <n>`, which overrides the
/// inactivity budget for that one dispatch. It exists because without it a
/// `--timeout 45` reached the operator's own warning text as "the
/// inactivity timeout (env 45)", telling an operator whose env actually
/// says 1200 that their env produced 45.
///
/// This is deliberately NOT the same enum as
/// `darkmux_types::config_access::Source` — that one models the three
/// tiers of the config-resolution contract and is matched exhaustively
/// across the workspace (`darkmux-doctor` included), where a fourth tier
/// that no config file can ever produce would be noise. This crate is not
/// a workspace member and this enum's only job is rendering provenance the
/// host already resolved, so the tier lands here.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BoundSource {
    #[default]
    BuiltIn,
    Config,
    Env,
    Cli,
}

impl BoundSource {
    pub fn as_str(self) -> &'static str {
        match self {
            BoundSource::BuiltIn => "built-in",
            BoundSource::Config => "config",
            BoundSource::Env => "env",
            BoundSource::Cli => "cli",
        }
    }

    /// Parses the host's `--<flag>-source <tier>` / `..._SOURCE` env value.
    /// Anything unrecognized (including absent/typo'd) falls to `BuiltIn` —
    /// the same "loud in doctor, lenient on read" posture the rest of
    /// darkmux's config surface takes; a malformed provenance hint should
    /// never abort a dispatch.
    ///
    /// (#2480 review, finding 5) That leniency also bounds the blast radius
    /// of the `"cli"` tier against a STALE runtime image: an image built
    /// before this arm existed renders a `--timeout`-sourced budget as
    /// `(built-in 45)` — wrong about the tier, but no more wrong than the
    /// `(env 45)` it printed before, and self-correcting the moment the
    /// image is refreshed.
    pub fn from_cli_str(s: &str) -> Self {
        match s {
            "config" => BoundSource::Config,
            "env" => BoundSource::Env,
            "cli" => BoundSource::Cli,
            _ => BoundSource::BuiltIn,
        }
    }
}

/// A resolved bound + its provenance, stamped onto a cap-hit/salvage/
/// detector/checkpoint-continuation/inactivity-warning record.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct BoundRef {
    pub kind: BoundKind,
    pub value: u64,
    pub source: BoundSource,
}

impl BoundRef {
    pub fn new(kind: BoundKind, value: u64, source: BoundSource) -> Self {
        Self { kind, value, source }
    }

    /// The stderr clause: `"the reasoning check-in interval (built-in
    /// 1000)"`. Callers splice this into their existing eprintln! rather
    /// than this owning the whole line — every emission site's message has
    /// its own shape (salvage counts, stall budgets, elapsed seconds).
    pub fn describe(&self) -> String {
        format!("{} ({} {})", self.kind.label(), self.source.as_str(), self.value)
    }
}

/// The resolved source tier for the two per-call-region knobs
/// (`per_call_cap`'s two possible origins — see `loop_runner.rs`'s
/// cap-selection block), set ONCE (from `main.rs`, before the loop runs) via
/// [`set_bound_sources`]. Unset (every non-`main.rs` test caller) reads back
/// as all-`BuiltIn` via `Default`. The inactivity-timeout source travels
/// separately (a companion `DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE` env
/// var read directly at that knob's own definition site in
/// `loop_runner.rs`) rather than through this struct — it isn't part of the
/// per-call-region selection this struct exists for.
#[derive(Debug, Clone, Copy, Default)]
pub struct BoundSources {
    pub reasoning_checkpoint_interval: BoundSource,
    pub max_tokens_per_call: BoundSource,
    /// (#2171 rebase onto #2165) Same companion-source pattern, for the
    /// generation check-in.
    pub generation_checkpoint_interval: BoundSource,
}

static BOUND_SOURCES: OnceLock<BoundSources> = OnceLock::new();

/// Called once from `main.rs` after parsing the host's
/// `--max-tokens-per-call-source`/`--reasoning-checkpoint-interval-source`
/// args, before the loop starts. A second call (there should never be one
/// in production) is a silent no-op — `OnceLock::set` — rather than a
/// panic, matching this crate's "never abort a dispatch over an
/// observability path" posture.
pub fn set_bound_sources(sources: BoundSources) {
    let _ = BOUND_SOURCES.set(sources);
}

/// Reads back what [`set_bound_sources`] stored, or the all-`BuiltIn`
/// default when it was never called (every test call site).
pub fn bound_sources() -> BoundSources {
    BOUND_SOURCES.get().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_names_kind_source_and_value() {
        let b = BoundRef::new(BoundKind::ReasoningCheckpointInterval, 1000, BoundSource::BuiltIn);
        assert_eq!(b.describe(), "the reasoning check-in interval (built-in 1000)");
    }

    #[test]
    fn kind_serializes_to_spec_strings() {
        assert_eq!(
            serde_json::to_value(BoundKind::ReasoningCheckpointInterval).unwrap(),
            serde_json::json!("reasoning_checkpoint_interval")
        );
        assert_eq!(
            serde_json::to_value(BoundKind::MaxTokensPerCall).unwrap(),
            serde_json::json!("max_tokens_per_call")
        );
        assert_eq!(
            serde_json::to_value(BoundKind::GenerationCheckpointInterval).unwrap(),
            serde_json::json!("generation_checkpoint_interval")
        );
        assert_eq!(serde_json::to_value(BoundKind::MaxTurns).unwrap(), serde_json::json!("max_turns"));
        assert_eq!(serde_json::to_value(BoundKind::MaxTokens).unwrap(), serde_json::json!("max_tokens"));
        assert_eq!(
            serde_json::to_value(BoundKind::InactivityTimeout).unwrap(),
            serde_json::json!("inactivity_timeout")
        );
    }

    #[test]
    fn source_serializes_to_spec_strings() {
        assert_eq!(serde_json::to_value(BoundSource::BuiltIn).unwrap(), serde_json::json!("built-in"));
        assert_eq!(serde_json::to_value(BoundSource::Config).unwrap(), serde_json::json!("config"));
        assert_eq!(serde_json::to_value(BoundSource::Env).unwrap(), serde_json::json!("env"));
        // (#2480 review, finding 5) The fourth tier — a bound the operator
        // named at the point of dispatch (`dispatch --timeout <n>`).
        assert_eq!(serde_json::to_value(BoundSource::Cli).unwrap(), serde_json::json!("cli"));
    }

    #[test]
    fn from_cli_str_round_trips_and_falls_back_to_built_in() {
        assert_eq!(BoundSource::from_cli_str("config"), BoundSource::Config);
        assert_eq!(BoundSource::from_cli_str("env"), BoundSource::Env);
        assert_eq!(BoundSource::from_cli_str("built-in"), BoundSource::BuiltIn);
        // (#2480 review, finding 5) The host writes this string into
        // `DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE` whenever `--timeout`
        // set the budget; without this arm the value fell to the `_` catch-all
        // and the operator's warning named a tier that did not decide it.
        assert_eq!(BoundSource::from_cli_str("cli"), BoundSource::Cli);
        assert_eq!(BoundSource::from_cli_str("bogus"), BoundSource::BuiltIn);
        assert_eq!(BoundSource::from_cli_str(""), BoundSource::BuiltIn);
    }

    /// (#2480 review, finding 5) `describe()` is what gets spliced into the
    /// runtime's operator-facing inactivity warning. A `--timeout 45` on a
    /// machine whose env says 1200 must not read as "(env 45)".
    #[test]
    fn describe_names_the_cli_tier_for_a_per_dispatch_override() {
        let b = BoundRef::new(BoundKind::InactivityTimeout, 45, BoundSource::Cli);
        assert_eq!(b.describe(), "the inactivity timeout (cli 45)");
    }

    #[test]
    fn bound_ref_json_shape_matches_the_issue_spec() {
        let b = BoundRef::new(BoundKind::MaxTokensPerCall, 4000, BoundSource::Config);
        let v = serde_json::to_value(b).unwrap();
        assert_eq!(v["kind"], serde_json::json!("max_tokens_per_call"));
        assert_eq!(v["value"], serde_json::json!(4000));
        assert_eq!(v["source"], serde_json::json!("config"));
    }
}
