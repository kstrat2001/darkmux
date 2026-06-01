//! darkmux-types — foundation crate.
//!
//! Profile / ProfileRegistry / ProfileModel schema (extracted whole from the
//! former `src/types.rs`) plus the workspace-path resolver (`paths`, lifted
//! from `src/lab/paths.rs`) so downstream crates can depend on path resolution
//! without pulling in the lab crate.

pub mod paths;
pub mod workdir;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// AI-industry-conventional model capabilities — orthogonal optimization
/// dimensions that map to how Anthropic, OpenAI, HuggingFace, Google,
/// Cohere, Mistral, and Meta describe their models. A *role* requests
/// capabilities (via the skills it declares); a *model* offers them
/// (via its profile entry). Lives in the foundation crate so both sides
/// — the crew/role layer and the profile/model layer — speak one
/// vocabulary (E14 / #450).
///
/// Serialized form: `snake_case` (e.g. `"code"`, `"agentic_tool_use"`).
/// Unknown variant names fail to deserialize with a clear error — no
/// silent typo-induced zero-weight bugs. Pre-1.0 schema growth is fine;
/// removing a variant is breaking, so seed conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Code generation + understanding. Benchmarks: HumanEval, MBPP,
    /// BigCodeBench.
    Code,
    /// Multi-step reasoning + judgment. Benchmarks: MMLU, GPQA,
    /// ARC-Challenge, MMLU-Pro.
    Reasoning,
    /// Adherence to structured prompts. Benchmarks: IFEval, ChatRAG.
    InstructionFollowing,
    /// Tool-call quality + agent loops. Benchmarks: SWE-Bench,
    /// AgentBench, Berkeley Function Calling Leaderboard.
    AgenticToolUse,
}

/// A weighted capability profile — maps each capability to a non-negative
/// weight. **Sparse-as-zero**: an absent key means weight 0. **Relative
/// weights**: magnitudes are advisory; ratios drive scoring (normalized
/// at scoring time). `BTreeMap` for deterministic iteration (display /
/// flow-record stamping benefit from stable ordering; the map is small,
/// capped by the `Capability` variant count).
pub type CapabilityProfile = BTreeMap<Capability, f32>;

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
    /// Capability vector — what kinds of work this model is good at, and
    /// at what relative weights (code / reasoning / instruction_following
    /// / agentic_tool_use). The operator populates it from lab results
    /// (which models run well on their machine, at what context). Empty
    /// by default. `select_model` (phase 2, #450) WILL match a role's
    /// requested capabilities against this — treating a wholly-unvectored
    /// model as a 0.5-everywhere generalist — with no machine tier in the
    /// decision, only capability + lab-vetted fit (#322). Additive today:
    /// nothing scores against it until that slice lands.
    #[serde(default)]
    pub capabilities: CapabilityProfile,
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
/// Compaction strategy — which compactor implementation runs when
/// the trigger fires. (#352 tier-2 work, scaffolded T2-A #372)
///
/// - **`Narrative`** (default, today's behavior): single-call to the
///   companion model with a prose-summary prompt; replaces middle
///   messages with a synthetic user-role message carrying the prose.
///   Article-2-era compactor shape.
/// - **`StructuredSlot`**: JSON-mode call to the compactor; output is
///   a typed `StructuredCompactionOutput` matching #354 v0.1 schema;
///   replaces middle messages with a synthetic SYSTEM-role message
///   carrying a labeled-markdown rendering of the slots. Operator-
///   opt-in via `profile.runtime.compaction.strategy: "structured-
///   slot"`. T2-B implements the compactor; T2-C wires the routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionStrategy {
    /// Today's default — narrative middle-replace via prose summary.
    Narrative,
    /// #352 tier-2 — structured-slot fact extraction.
    StructuredSlot,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCompactionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<CompactionStrategy>,
    /// Absolute compaction trigger in tokens (hard ceiling). When
    /// `latest_prompt_tokens` reaches this, compaction fires —
    /// regardless of loaded context window. Sibling to
    /// `threshold_ratio` (the adaptive scale-with-context-window
    /// variant); either fires first wins. Documented as the rarely-
    /// needed power-user override; ratio is the recommended primary
    /// tuning surface. (#357 + #368)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<u64>,
    /// Compaction trigger as a fraction of the loaded primary's
    /// context window (e.g. 0.6 = fire when prompt reaches 60% of
    /// context). Adaptive — scales naturally across 50K / 100K / 200K
    /// model loads without per-load retuning. Sibling to
    /// `threshold_tokens` (the absolute hard ceiling); either fires
    /// first wins.
    ///
    /// Range 0.1-0.9 (mirrors openclaw's range for similar fields).
    /// Defaults: unset → only the absolute threshold trigger applies.
    /// Replaces the operator-frustrating openclaw-shape
    /// `maxHistoryShare` extras passthrough — darkmux owns this knob
    /// with a name that reflects what it actually does (a TRIGGER, not
    /// openclaw's post-compaction history cap). (#368 clean break)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1: Option<Tier1Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2: Option<Tier2Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve: Option<ReserveConfig>,
    /// Operator-tunable text appended to the compactor's system prompt
    /// at compaction time. When set, darkmux injects this guidance into
    /// the structured-slot compactor's system prompt so operators can
    /// steer what the compactor preserves (e.g. "Preserve verbatim X /
    /// list active files with what was learned").
    ///
    /// **Schema isolation**: each runtime owns its own config — this
    /// field is the typed replacement for the dead-letter
    /// `extras["customInstructions"]` openclaw passthrough. The
    /// internal runtime now reads this typed field only; the
    /// migration off the extras shape (heuristic generator + doctor
    /// warning for legacy operator profiles) is tracked under
    /// [#380](https://github.com/kstrat2001/darkmux/issues/380) and
    /// has not landed yet — operators with `extras["customInstructions"]`
    /// in their profile silently get the value ignored until that
    /// follow-up ships.
    /// See DESIGN.md "Schema isolation: each runtime owns its own config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    /// Openclaw-shape passthrough + forward-compat overflow. Read by
    /// existing consumers (`sprint_cli` reads `maxHistoryShare`; openclaw
    /// runtime config-patching reads `model` / `mode` /
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

/// Machine-level internal bindings — darkmux's own standing infrastructure
/// for this machine, a sibling to the operator's `profiles`. Decoupled from
/// any single profile so swapping a worker profile never changes the
/// compaction model. (#590)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryInternal {
    /// The machine's **utility model** — the standing support model the
    /// runtime summons for built-in utility tasks (compaction today;
    /// estimation / mission-compile later). The operator registers it from
    /// lab work; it is **not** capability-scored (a score could re-pick a
    /// large worker for compaction and reintroduce the per-beat tax). One
    /// global util model serves all utility hooks. Absent ⇒ the runtime
    /// falls back to its built-in default compactor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utility: Option<String>,
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

impl Profile {
    /// (#450, E14 refactor 1b) Returns the id of this profile's
    /// Primary-role model — the worker model dispatch routes to by
    /// default. Profile schema enforces "exactly one primary" at load
    /// time (see `profiles::validate_profile`), so this is the model
    /// every worker-class role uses under phase-1 trivial selection.
    /// Phase-2 scoring layers on top: candidate set comes from ALL
    /// `models[]` entries, scoring picks per-role, primary is the
    /// fallback when no candidate scores above threshold.
    pub fn primary_model_id(&self) -> Option<&str> {
        self.models
            .iter()
            .find(|m| matches!(m.role, ModelRole::Primary))
            .map(|m| m.id.as_str())
    }

    /// (#450, E14 refactor 1b) Returns the id of this profile's
    /// Compactor-role model — the utility model darkmux uses for its
    /// internal AI-first functions (compaction today; future
    /// mission-planner-auto, scribe-helper, etc.). Lives in a
    /// separate `[internal]`-style layer from worker-tier roles;
    /// always-loaded for the runtime session per Beat 50 doctrine.
    ///
    /// Unused in refactor 1b's dispatch wiring (compactor selection
    /// today flows through `RuntimeCompactionConfig.compactor_model`
    /// directly). Phase-1c will wire this when the utility-loading
    /// orchestration lands; the helper exists now as the public API
    /// surface phase-1c will consume.
    #[allow(dead_code)]
    pub fn compactor_model_id(&self) -> Option<&str> {
        self.models
            .iter()
            .find(|m| matches!(m.role, ModelRole::Compactor))
            .map(|m| m.id.as_str())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileRegistry {
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<RegistryHooks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Machine-level internal bindings (the utility model, …) — sibling to
    /// `profiles`, so swapping a worker profile never disturbs darkmux's own
    /// standing infrastructure. (#590)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<RegistryInternal>,
}

impl ProfileRegistry {
    /// The machine's registered utility model id (`internal.utility`), if any.
    /// `None` ⇒ no machine utility model registered; consumers fall back to
    /// their built-in default. (#590)
    pub fn utility_model_id(&self) -> Option<&str> {
        self.internal.as_ref().and_then(|i| i.utility.as_deref())
    }
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
mod compaction_strategy_tests {
    //! (#372 T2-A) Tests gate the new `CompactionStrategy` typed enum.
    //! Pre-T2-A: `strategy: Option<String>` — accepts any string,
    //! consumer has to parse + validate. T2-A: typed enum with
    //! kebab-case serde names matching the user-facing strings, so
    //! serde rejects unknown values at parse time.
    use super::*;

    #[test]
    fn strategy_narrative_serializes_kebab() {
        let s = CompactionStrategy::Narrative;
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"narrative\"");
    }

    #[test]
    fn strategy_structured_slot_serializes_kebab() {
        let s = CompactionStrategy::StructuredSlot;
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"structured-slot\"");
    }

    #[test]
    fn strategy_round_trip_through_compaction_config() {
        let cfg = RuntimeCompactionConfig {
            strategy: Some(CompactionStrategy::StructuredSlot),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"strategy\":\"structured-slot\""));
        let back: RuntimeCompactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.strategy, Some(CompactionStrategy::StructuredSlot));
    }

    #[test]
    fn strategy_default_unset_is_none() {
        let cfg = RuntimeCompactionConfig::default();
        assert_eq!(cfg.strategy, None);
    }

    #[test]
    fn strategy_unknown_value_errors_at_parse() {
        let json = r#"{"strategy": "fancy-new-thing"}"#;
        let result: Result<RuntimeCompactionConfig, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "unknown strategy must error at parse time, not silently fall back"
        );
    }

    #[test]
    fn strategy_parses_narrative_from_kebab_string() {
        let json = r#"{"strategy": "narrative"}"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.strategy, Some(CompactionStrategy::Narrative));
    }
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
            capabilities: Default::default(),
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
            capabilities: Default::default(),
            identifier: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("identifier"));
    }

    #[test]
    fn profile_model_parses_capability_vector() {
        // A model carries an inline capability vector; absent keys are
        // sparse-as-zero. select_model (phase 2) matches a role's needs
        // against this — no machine tier in the decision (#450/#322).
        let json =
            r#"{"id":"m","n_ctx":32000,"role":"primary","capabilities":{"code":0.9,"reasoning":0.4}}"#;
        let m: ProfileModel = serde_json::from_str(json).unwrap();
        assert_eq!(m.capabilities.get(&Capability::Code), Some(&0.9));
        assert_eq!(m.capabilities.get(&Capability::Reasoning), Some(&0.4));
        assert_eq!(m.capabilities.get(&Capability::AgenticToolUse), None); // sparse-as-zero
        let back: ProfileModel =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.capabilities, m.capabilities);
    }

    #[test]
    fn capability_rejects_unknown_variant() {
        // A typo'd capability fails loud rather than silently scoring zero.
        let r: Result<Capability, _> = serde_json::from_str("\"coding\"");
        assert!(r.is_err());
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
        // No `internal` block ⇒ no machine utility model registered.
        assert_eq!(reg.utility_model_id(), None);
    }

    #[test]
    fn registry_parses_machine_internal_utility_binding() {
        // The machine-level `internal.utility` binding (#590) — sibling to
        // `profiles`, carries the standing utility model id.
        let json = r#"{
            "profiles": {
                "fast": { "models": [ {"id": "worker-a", "n_ctx": 32000, "role": "primary"} ] }
            },
            "internal": { "utility": "darkmux:qwen3-4b-instruct-2507" }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.utility_model_id(), Some("darkmux:qwen3-4b-instruct-2507"));
        // Round-trips back to the same shape.
        let back: ProfileRegistry =
            serde_json::from_str(&serde_json::to_string(&reg).unwrap()).unwrap();
        assert_eq!(back.utility_model_id(), Some("darkmux:qwen3-4b-instruct-2507"));
    }

    #[test]
    fn registry_internal_present_but_utility_absent_is_none() {
        // An `internal` block with no `utility` key ⇒ no util model.
        let json = r#"{ "profiles": {}, "internal": {} }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert!(reg.internal.is_some());
        assert_eq!(reg.utility_model_id(), None);
        // The inner `utility` key skips when None, so `internal` reserializes
        // as an empty object — `{"internal":{}}` — and never carries a
        // `utility` key. (The empty `internal` block itself is NOT dropped.)
        assert!(!serde_json::to_string(&reg).unwrap().contains("utility"));
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
        assert_eq!(cfg.strategy, Some(CompactionStrategy::StructuredSlot));
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
        assert_eq!(comp.strategy, Some(CompactionStrategy::StructuredSlot));
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

    /// (#383) custom_instructions field round-trips through JSON:
    /// parses when present, omits from output when None.
    #[test]
    fn custom_instructions_round_trips() {
        // With value set — parses and round-trips.
        let json = r#"{"custom_instructions":"Preserve verbatim X / list active files"}"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.custom_instructions.as_deref(),
            Some("Preserve verbatim X / list active files")
        );
        // Re-serialize — value appears in output.
        let out: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            out["custom_instructions"],
            "Preserve verbatim X / list active files"
        );

        // Without value — None, omits from output.
        let json2 = r#"{"strategy":"structured-slot"}"#;
        let cfg2: RuntimeCompactionConfig = serde_json::from_str(json2).unwrap();
        assert!(cfg2.custom_instructions.is_none());
        let out2: serde_json::Value = serde_json::to_value(&cfg2).unwrap();
        assert!(!out2.as_object().unwrap().contains_key("custom_instructions"));
    }
}
