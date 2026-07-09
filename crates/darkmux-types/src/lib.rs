//! darkmux-types — foundation crate.
//!
//! Profile / ProfileRegistry / ProfileModel schema (extracted whole from the
//! former `src/types.rs`) plus the workspace-path resolver (`paths`, lifted
//! from `src/lab/paths.rs`) so downstream crates can depend on path resolution
//! without pulling in the lab crate.

pub mod config;
pub mod config_access;
pub mod paths;
pub mod style;
pub mod workdir;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// (#1129) The running build's identifier — single source of truth for the
/// viewer header, `darkmux doctor`, and anywhere the live build needs naming.
/// The bare package version can't distinguish a packaged release from a
/// main-dev build, so `build.rs` bakes a tag (`DARKMUX_BUILD_TAG`):
///
///   * `"<version> (release)"` — a packaged release (Homebrew stable stamps
///     `DARKMUX_RELEASE`).
///   * `"<version> (a1b2c3d✱)"` — a git build (dev / `brew install --HEAD`);
///     short SHA, `✱` when the tree was dirty.
///   * `"<version>"` — a source tarball build (no release flag, no git).
pub fn build_version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    match option_env!("DARKMUX_BUILD_TAG") {
        Some(tag) if !tag.is_empty() => format!("{v} ({tag})"),
        _ => v.to_string(),
    }
}

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileModel {
    pub id: String,
    pub n_ctx: u32,
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
    /// The OpenAI-compatible endpoint this model is served from. Absent ⇒
    /// LMStudio local (`config_access::lmstudio_url()`). A remote model
    /// (Azure OpenAI, OpenAI, a LiteLLM proxy) names its URL + auth here.
    /// When set, `n_ctx` is a *declared* window (darkmux can't load-set a
    /// remote model's context — it asserts the ceiling for overflow
    /// avoidance + compaction-threshold placement) rather than a load
    /// parameter. This is the "interface to the model" the run's dispatch
    /// section surfaces; LMStudio is simply the default value here, not a
    /// hardwired identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<ModelEndpoint>,
    /// Forward-compat overflow — unknown keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The OpenAI-compatible endpoint a model is served from. Absent on a
/// [`ProfileModel`] ⇒ the LMStudio local default. Every hosted provider
/// darkmux targets (Azure OpenAI, OpenAI, OpenRouter, a LiteLLM proxy)
/// speaks OpenAI chat-completions, so one endpoint type covers all of them —
/// LMStudio included, as the zero-config local default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelEndpoint {
    /// Base URL of the OpenAI-compatible server. Absent ⇒ the LMStudio
    /// local default (`config_access::lmstudio_url()`, honoring
    /// `DARKMUX_LMSTUDIO_URL` > `config.lmstudio_url` > `http://localhost:1234`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// API-version query parameter (Azure OpenAI requires one, e.g.
    /// `"2025-01-01-preview"`). Absent for LMStudio / OpenAI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Auth mechanics. Absent ⇒ no auth header (LMStudio local needs none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<EndpointAuth>,
    /// Reasoning effort requested from the endpoint (`"low"`/`"medium"`/
    /// `"high"` — passed through verbatim as the OpenAI-form
    /// `reasoning_effort`). Absent ⇒ the parameter is omitted and the
    /// endpoint's own default applies. This is ARTIFACT configuration, not a
    /// tuning nicety: gpt-5.1-class models default reasoning OFF, so a
    /// judgment bench that omits it measures the model's reflexive mode
    /// (first observed 2026-07-05: 64-completion-token rubber-stamp reviews).
    /// Reasoning tokens bill INSIDE `max_completion_tokens`, so setting this
    /// also raises the single-shot completion-cap default (4096 → 16384) —
    /// an explicit cap still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Forward-compat overflow.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ModelEndpoint {
    /// Effective base URL: the declared [`url`](Self::url), else the LMStudio
    /// local default (env `DARKMUX_LMSTUDIO_URL` > `config.lmstudio_url` >
    /// `http://localhost:1234`).
    pub fn base_url(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(crate::config_access::lmstudio_url)
    }

    /// Whether this is a declared *remote* server (a URL is set) vs the
    /// implicit LMStudio local default. Drives the run's `endpoint` dimension
    /// and the set-vs-declared `n_ctx` semantics.
    pub fn is_remote(&self) -> bool {
        self.url.is_some()
    }

    /// Coherence check intended for the #1177 `darkmux doctor` credential slice /
    /// profile load — NOT yet wired into either, so today it's exercised only by
    /// unit tests. Presence-only on auth: does NOT verify the key is correct
    /// (that's the opt-in live probe, #1177). Returns a human-readable reason.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(u) = &self.url {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err(format!(
                    "endpoint.url must start with http:// or https:// (got {u:?})"
                ));
            }
        }
        if let Some(auth) = &self.auth {
            if auth.auth_type.is_some() && auth.keychain.as_deref().unwrap_or("").is_empty() {
                return Err("endpoint.auth.type is set but endpoint.auth.keychain \
                     (the Keychain item name holding the secret) is missing"
                    .to_string());
            }
        }
        Ok(())
    }
}

/// Auth for a remote endpoint. The secret is **never** stored here — only the
/// macOS Keychain item *name*; the secret is read at runtime via
/// `security find-generic-password` (the same carve-out as the Redis password
/// and serve token). The machine holding the referenced item is the keymaster
/// for the endpoint — see doctor's credential-presence check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointAuth {
    /// Header mechanics: `api-key` (Azure OpenAI) or `bearer` (OpenAI).
    /// Absent ⇒ no auth header emitted.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<EndpointAuthType>,
    /// macOS Keychain item name holding the secret (machine-local by
    /// darkmux's per-machine provisioning convention). Read at runtime;
    /// never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain: Option<String>,
    /// Forward-compat overflow.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The header mechanics for a remote endpoint's auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointAuthType {
    /// Azure OpenAI — `api-key: <secret>`.
    ApiKey,
    /// OpenAI / OpenRouter / most proxies — `Authorization: Bearer <secret>`.
    Bearer,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileRuntime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
/// any single profile so swapping a profile never changes the
/// compaction model. (#590)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryInternal {
    /// The machine's **utility model** — the standing support model the
    /// runtime summons for built-in utility tasks (compaction today;
    /// estimation / mission-compile later). The operator registers it from
    /// lab work; it is **not** capability-scored (a score could re-pick a
    /// large model for compaction and reintroduce the per-beat tax). One
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
    /// The canonical default model id (#590) — the deterministic
    /// fallback `select_model` returns when capability scoring can't
    /// differentiate (no offers, or a tie). Replaces the old `Primary`-role
    /// designation. When `None`, the first model in `models[]` is the implicit
    /// default (mirrors the old Primary-is-first convention). Must name a real
    /// `models[]` id when set (checked by `validate_profile`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ProfileRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_when: Option<serde_json::Value>,
    /// Forward-compat overflow — unknown keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl Profile {
    /// (#590) The profile's canonical default model id: the explicit
    /// `default_model` if set, else the first declared model (mirroring the
    /// old Primary-is-first convention). `None` only when `models[]` is empty.
    /// This is the deterministic fallback `select_model` returns when
    /// capability scoring can't differentiate, and the model that swap /
    /// compaction-context resolution treat as the profile's primary model.
    /// (Replaces the removed `primary_model_id()`; the compactor moved to the
    /// registry's `internal.utility` binding, so `compactor_model_id()` is
    /// gone.)
    pub fn default_model_id(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .or_else(|| self.models.first().map(|m| m.id.as_str()))
    }
}

/// Semver of the `profiles.json` registry shape. Additive field/section adds
/// are a **minor** bump (older binaries safely ignore unknown keys via
/// `extras`); renaming/retyping a field is **major**. Mirrors the
/// `CONFIG_SCHEMA_VERSION` discipline (`darkmux_types::config`) — this is
/// the registry's first formalized constant (the `schema_version` field on
/// [`ProfileRegistry`] predates it as free-text, operator-set text; no
/// prior value was ever asserted in code, so this constant's baseline is
/// where the discipline starts, not a bump off a previously-enforced "1.0").
///
/// Stamped into the registry `darkmux init` writes (via the embedded
/// `profiles.example.json`, drift-guarded by an init test) per the
/// visible-defaults philosophy. Loading stays **lenient**: an absent or
/// older `schema_version` never gates a read — loud validation belongs to
/// `darkmux doctor`, not the hot load path.
// 1.1 (#1222 Phase B packet 1): additive top-level `crews{}` section (saved
// crew assignments — seat staffing). Minor bump — an older binary tolerates
// it (all-Option + `extras` overflow on `ProfileRegistry`), per the
// lenient-read doctrine.
pub const PROFILES_SCHEMA_VERSION: &str = "1.1";

/// A **saved crew assignment**: which models staff which crew-role seats,
/// for multi-seat pipelines (e.g. a review funnel's probe + judge seats).
/// Machine-side because staffing is a hardware-fit decision, not an
/// engagement-shaping one. **Selection is still per-dispatch** (operator
/// sovereignty, #44) — a `Crew` is a reusable lineup an operator can point a
/// dispatch at, not imposed membership a dispatch is forced into.
///
/// `seats` is keyed by **crew role id** (e.g. `"review-probe"`,
/// `"review-judge"`) — the same role-id vocabulary
/// `crates/darkmux-crew` already uses for `crew dispatch <role-id>`, so a
/// crew assignment slots directly onto existing roles rather than inventing
/// a parallel naming scheme. **This packet does not validate role ids
/// against the role manifest registry** — a pipeline's specific seat
/// requirements (e.g. "review-probe needs at least one staffing") are
/// consumer-side validation for a later packet; this schema is generic
/// across any future multi-seat pipeline, not just review.
///
/// Pure data + resolution today — nothing consumes a `Crew` yet; this
/// schema lands ahead of the dispatch machinery a later Phase B packet
/// adds (#1222 Phase B packet 1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Crew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Role id → the model(s) staffing that seat. A `Vec` because a seat can
    /// be staffed by more than one model (e.g. two probe staffings drawing
    /// independently); order within the `Vec` is the staffing's declared
    /// order but carries no dispatch-sequence meaning by itself — that's a
    /// consumer-side concern.
    pub seats: BTreeMap<String, Vec<SeatStaffing>>,
    /// Forward-compat overflow — unknown keys land here and re-serialize
    /// flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// One model staffing a [`Crew`] seat — a [`Profile`] + model binding plus
/// its draw shape. `k` (draws per work item) defaults to 3, matching the
/// config.json visible-defaults doctrine: the knob is a real, documented
/// default, not a hidden code constant, so an operator reading a resolved
/// crew sees what actually ran.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeatStaffing {
    /// The registry [`Profile`] name this staffing dispatches through.
    pub profile: String,
    /// Model id within that profile. `None` ⇒ the profile's
    /// [`Profile::default_model_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Draws per work item. Default 3.
    #[serde(default = "default_k")]
    pub k: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Bundle-selection scope. Valid on any staffing — this schema doesn't
    /// gate it on a draw-shape enum; a consumer decides what it means for
    /// its own seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_selector: Option<BundleSelector>,
    /// Forward-compat overflow — unknown keys land here and re-serialize
    /// flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

fn default_k() -> u32 {
    3
}

/// Scopes a [`SeatStaffing`]'s draws to a subset of fact families, and
/// optionally caps how many bundles it considers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSelector {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fact_families: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bundles: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileRegistry {
    pub profiles: BTreeMap<String, Profile>,
    /// Schema version — additive field/section adds are a minor bump
    /// (older binaries safely ignore unknown keys via `extras`);
    /// renaming/retyping a field is major.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<RegistryHooks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Machine-level internal bindings (the utility model, …) — sibling to
    /// `profiles`, so swapping a profile never disturbs darkmux's own
    /// standing infrastructure. (#590)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<RegistryInternal>,
    /// Named [`Crew`] definitions — saved seat-staffing assignments,
    /// sibling to `profiles`/`internal`. Empty by default; a crew names
    /// `Profile`s declared in `profiles` above, it doesn't replace them.
    /// (#1222 Phase B packet 1)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub crews: BTreeMap<String, Crew>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ProfileRegistry {
    /// The machine's registered utility model id (`internal.utility`), if any.
    /// `None` ⇒ no machine utility model registered; consumers fall back to
    /// their built-in default. (#590)
    ///
    /// Surrounding whitespace is trimmed and a blank value (`""` / whitespace)
    /// is treated as **unset** — an empty binding is meaningless, and since
    /// `swap` now *loads* this model (not just displays it), a blank value
    /// would otherwise attempt to load the bare `darkmux:` identifier. Trimming
    /// also keeps the value matchable against the un-padded loaded-model fields
    /// the doctor + dispatch preflight compare against.
    pub fn utility_model_id(&self) -> Option<&str> {
        self.internal
            .as_ref()
            .and_then(|i| i.utility.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// (#1054) Resolve which profile a dispatch should use, given an optional
    /// explicit `--profile <name>` request.
    ///
    /// Resolution order:
    ///   1. the requested name, if it names a profile defined here;
    ///   2. else `default_profile`, if set and defined;
    ///   3. else `None` (the caller falls back to probing the loaded model).
    ///
    /// A requested name that ISN'T defined here falls through to
    /// `default_profile` rather than erroring — so a machine-agnostic caller
    /// (e.g. a CI workflow) can NAME the profile it wants (`review`) while each
    /// machine decides whether it has defined that profile or degrades to its
    /// default. The returned name lets the caller detect a fallback (resolved
    /// name != requested name) and surface it.
    pub fn resolve_active<'a>(&'a self, requested: Option<&str>) -> Option<(&'a str, &'a Profile)> {
        if let Some(name) = requested {
            if let Some((k, p)) = self.profiles.get_key_value(name) {
                return Some((k.as_str(), p));
            }
        }
        let default_name = self.default_profile.as_deref()?;
        let profile = self.profiles.get(default_name)?;
        Some((default_name, profile))
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

    // (#1054) ProfileRegistry::resolve_active — per-dispatch profile selection
    // with graceful fallback. Profiles are empty here since resolve_active only
    // inspects the registry's keys + default_profile.
    fn reg(profiles: &[&str], default: Option<&str>) -> ProfileRegistry {
        let mut map = std::collections::BTreeMap::new();
        for name in profiles {
            map.insert((*name).to_string(), Profile::default());
        }
        ProfileRegistry {
            profiles: map,
            default_profile: default.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_active_prefers_requested_when_defined() {
        let r = reg(&["review", "deep"], Some("deep"));
        let (name, _) = r.resolve_active(Some("review")).expect("review is defined");
        assert_eq!(name, "review");
    }

    #[test]
    fn resolve_active_falls_back_to_default_when_requested_undefined() {
        // The machine-agnostic-caller case: a workflow asks for `review`, this
        // machine hasn't defined it → degrade to default_profile, not an error.
        let r = reg(&["deep"], Some("deep"));
        let (name, _) = r.resolve_active(Some("review")).expect("falls back to default");
        assert_eq!(name, "deep");
    }

    #[test]
    fn resolve_active_uses_default_when_no_request() {
        let r = reg(&["deep"], Some("deep"));
        let (name, _) = r.resolve_active(None).expect("default resolves");
        assert_eq!(name, "deep");
    }

    #[test]
    fn resolve_active_none_when_undefined_request_and_no_default() {
        let r = reg(&["deep"], None);
        assert!(r.resolve_active(Some("review")).is_none());
        assert!(r.resolve_active(None).is_none());
    }

    #[test]
    fn resolve_active_none_when_default_points_at_undefined_profile() {
        // A dangling default_profile resolves to nothing (caller probes)...
        let r = reg(&["deep"], Some("ghost"));
        assert!(r.resolve_active(None).is_none());
        // ...but a defined request still wins over the dangling default.
        let (name, _) = r.resolve_active(Some("deep")).expect("defined request wins");
        assert_eq!(name, "deep");
    }

    #[test]
    fn profile_model_round_trips() {
        let m = ProfileModel {
            endpoint: None,
            id: "x".to_string(),
            n_ctx: 32_000,
            capabilities: Default::default(),
            identifier: Some("alias".to_string()),
            extras: Default::default(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProfileModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.n_ctx, 32_000);
        assert_eq!(back.identifier.as_deref(), Some("alias"));
    }

    #[test]
    fn profile_model_omits_none_identifier() {
        let m = ProfileModel {
            endpoint: None,
            id: "x".to_string(),
            n_ctx: 1024,
            capabilities: Default::default(),
            identifier: None,
            extras: Default::default(),
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
            r#"{"id":"m","n_ctx":32000,"capabilities":{"code":0.9,"reasoning":0.4}}"#;
        let m: ProfileModel = serde_json::from_str(json).unwrap();
        assert_eq!(m.capabilities.get(&Capability::Code), Some(&0.9));
        assert_eq!(m.capabilities.get(&Capability::Reasoning), Some(&0.4));
        assert_eq!(m.capabilities.get(&Capability::AgenticToolUse), None); // sparse-as-zero
        let back: ProfileModel =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.capabilities, m.capabilities);
    }

    #[test]
    fn profile_model_endpoint_round_trips() {
        // A remote model names its endpoint URL + auth. The Keychain item
        // NAME is stored — never the secret.
        let json = r#"{
            "id": "gpt-5.1",
            "n_ctx": 200000,
            "endpoint": {
                "url": "https://finherogpt.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                "api_version": "2025-01-01-preview",
                "auth": { "type": "api-key", "keychain": "darkmux-azure-finherogpt" }
            }
        }"#;
        let m: ProfileModel = serde_json::from_str(json).unwrap();
        let ep = m.endpoint.as_ref().expect("endpoint parsed");
        assert!(ep.is_remote());
        assert_eq!(
            ep.base_url(),
            "https://finherogpt.cognitiveservices.azure.com/openai/deployments/gpt-4o"
        );
        assert_eq!(ep.api_version.as_deref(), Some("2025-01-01-preview"));
        let auth = ep.auth.as_ref().expect("auth parsed");
        assert_eq!(auth.auth_type, Some(EndpointAuthType::ApiKey));
        assert_eq!(auth.keychain.as_deref(), Some("darkmux-azure-finherogpt"));
        // full round-trip preserves the endpoint
        let back: ProfileModel =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.endpoint, m.endpoint);
    }

    #[test]
    fn profile_model_absent_endpoint_is_local_and_omitted() {
        // Backward-compat: an existing profile with no endpoint parses fine,
        // endpoint is None (⇒ LMStudio local), and re-serializes without the key.
        let m: ProfileModel = serde_json::from_str(r#"{"id":"qwen","n_ctx":32000}"#).unwrap();
        assert!(m.endpoint.is_none());
        assert!(!serde_json::to_string(&m).unwrap().contains("endpoint"));
    }

    #[test]
    fn endpoint_default_is_local_not_remote() {
        // No url ⇒ not remote; base_url resolves to the LMStudio default (a URL).
        let ep = ModelEndpoint::default();
        assert!(!ep.is_remote());
        assert!(
            ep.base_url().contains("://"),
            "base_url should be a URL, got {:?}",
            ep.base_url()
        );
        assert!(ep.validate().is_ok());
    }

    #[test]
    fn endpoint_validate_catches_bad_url_and_authless_keychain() {
        // A URL without a scheme is rejected.
        let bad = ModelEndpoint {
            url: Some("finherogpt.azure.com".into()),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        // Auth type set but no Keychain item name → the "forgot to say where
        // the secret lives" error.
        let no_key = ModelEndpoint {
            url: Some("https://x/".into()),
            auth: Some(EndpointAuth {
                auth_type: Some(EndpointAuthType::ApiKey),
                keychain: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(no_key.validate().is_err());
        // A coherent remote endpoint validates.
        let ok = ModelEndpoint {
            url: Some("https://x/".into()),
            auth: Some(EndpointAuth {
                auth_type: Some(EndpointAuthType::Bearer),
                keychain: Some("darkmux-openai".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
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
                        {"id": "model-a", "n_ctx": 32000}
                    ]
                }
            }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.profiles.len(), 1);
        let p = reg.profiles.get("fast").unwrap();
        assert_eq!(p.models[0].n_ctx, 32_000);
        assert_eq!(p.default_model_id(), Some("model-a"));
        // No `internal` block ⇒ no machine utility model registered.
        assert_eq!(reg.utility_model_id(), None);
    }

    #[test]
    fn registry_parses_machine_internal_utility_binding() {
        // The machine-level `internal.utility` binding (#590) — sibling to
        // `profiles`, carries the standing utility model id.
        let json = r#"{
            "profiles": {
                "fast": { "models": [ {"id": "worker-a", "n_ctx": 32000} ] }
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

    #[test]
    fn registry_blank_utility_is_treated_as_unset() {
        // A blank or whitespace-only `utility` value is meaningless and reads
        // as "no util model" — guards swap (#590) against trying to load the
        // bare `darkmux:` identifier. Surrounding whitespace is also trimmed.
        for blank in ["\"\"", "\"   \"", "\"\\t\\n\""] {
            let json = format!(r#"{{ "profiles": {{}}, "internal": {{ "utility": {blank} }} }}"#);
            let reg: ProfileRegistry = serde_json::from_str(&json).unwrap();
            assert_eq!(reg.utility_model_id(), None, "blank {blank} should be unset");
        }
        // A padded real id trims to the bare id (so it still matches loaded state).
        let json = r#"{ "profiles": {}, "internal": { "utility": "  darkmux:util-4b  " } }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.utility_model_id(), Some("darkmux:util-4b"));
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
                    "models": [{"id": "primary", "n_ctx": 60000}],
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

    // ─── ProfileRegistry forward-compat overflow, #694 ──────────────

    /// A default ProfileRegistry serializes to `{"profiles":{}}` and
    /// round-trips empty — the forward-compat guarantee (mirrors
    /// `default_round_trips_empty`).
    #[test]
    fn registry_default_serializes_empty() {
        let reg = ProfileRegistry::default();
        let json = serde_json::to_string(&reg).unwrap();
        assert_eq!(json, r#"{"profiles":{}}"#);
        let back: ProfileRegistry = serde_json::from_str(&json).unwrap();
        assert!(back.profiles.is_empty());
        assert!(back.schema_version.is_none());
        assert!(back.hooks.is_none());
        assert!(back.default_profile.is_none());
        assert!(back.internal.is_none());
        assert!(back.crews.is_empty());
        assert!(back.extras.is_empty());
    }

    /// Unknown top-level keys land in `extras` and re-serialize flat —
    /// a newer config read by an older binary preserves them.
    #[test]
    fn registry_unknown_keys_land_in_extras_and_reserialize_flat() {
        let json = r#"{"profiles":{},"future_knob":7,"nested_future":{"a":1}}"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.profiles.len(), 0);
        assert_eq!(reg.extras.get("future_knob").and_then(|v| v.as_u64()), Some(7));
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        let obj = out.as_object().unwrap();
        assert!(!obj.contains_key("extras"), "extras must flatten, not nest");
        assert!(obj.contains_key("future_knob"), "unknown key re-serializes flat");
    }

    /// Full round-trip preserves typed fields through serialize→parse cycle.
    #[test]
    fn registry_full_shape_round_trips() {
        let json = r#"{"schema_version":"2.0","profiles":{"fast":{"description":"tiny profile","models":[{"id":"model-a","n_ctx":32000}],"default_model":"model-a"}},"hooks":{"pre_swap":[{"command":"echo swap-start","condition":"always"}],"post_swap":[{"command":"echo swap-end"}]},"default_profile":"fast","internal":{"utility":"darkmux:util-4b"},"future_field":true}"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.schema_version.as_deref(), Some("2.0"));
        assert_eq!(reg.profiles.len(), 1);
        let p = reg.profiles.get("fast").unwrap();
        assert_eq!(p.description.as_deref(), Some("tiny profile"));
        assert_eq!(p.default_model_id(), Some("model-a"));
        assert_eq!(reg.hooks.as_ref().unwrap().pre_swap.len(), 1);
        assert_eq!(reg.default_profile.as_deref(), Some("fast"));
        assert!(reg.internal.is_some());
        assert_eq!(reg.utility_model_id(), Some("darkmux:util-4b"));
        assert!(reg.extras.contains_key("future_field"));

        // Re-serialize → parse back — typed fields survive.
        let round = serde_json::to_string(&reg).unwrap();
        let back: ProfileRegistry = serde_json::from_str(&round).unwrap();
        assert_eq!(back.schema_version, reg.schema_version);
        assert_eq!(back.profiles.len(), 1);
        let p2 = back.profiles.get("fast").unwrap();
        assert_eq!(p2.description, p.description);
        assert_eq!(back.default_profile, reg.default_profile);
        assert_eq!(back.internal.as_ref().unwrap().utility, reg.internal.as_ref().unwrap().utility);
        assert!(back.extras.contains_key("future_field"));
    }
}

#[cfg(test)]
mod crew_tests {
    //! (#1222 Phase B packet 1) Schema-layer coverage for `Crew` and its
    //! nested types. Validation semantics (seat/staffing non-empty, profile
    //! refs resolve, k >= 1, remote-endpoint rejection) live in
    //! `darkmux-profiles::crews` — this module gates serde shape only:
    //! round-trips, `k` defaulting, extras tolerance, and the
    //! value-identical old-shape (no `crews` key) invariant.
    use super::*;

    #[test]
    fn seat_staffing_k_defaults_to_three_when_absent() {
        let json = r#"{"profile":"deep"}"#;
        let s: SeatStaffing = serde_json::from_str(json).unwrap();
        assert_eq!(s.k, 3);
        assert!(s.model.is_none());
        assert!(s.max_tokens.is_none());
        assert!(s.bundle_selector.is_none());
    }

    #[test]
    fn seat_staffing_explicit_k_overrides_default() {
        let json = r#"{"profile":"deep","k":7,"max_tokens":2048,
            "bundle_selector":{"fact_families":["auth","billing"],"max_bundles":5}}"#;
        let s: SeatStaffing = serde_json::from_str(json).unwrap();
        assert_eq!(s.k, 7);
        assert_eq!(s.max_tokens, Some(2048));
        let sel = s.bundle_selector.as_ref().unwrap();
        assert_eq!(sel.fact_families, vec!["auth".to_string(), "billing".to_string()]);
        assert_eq!(sel.max_bundles, Some(5));
    }

    #[test]
    fn seat_staffing_unknown_keys_land_in_extras() {
        let json = r#"{"profile":"deep","future_knob":"x"}"#;
        let s: SeatStaffing = serde_json::from_str(json).unwrap();
        assert_eq!(s.extras.get("future_knob").and_then(|v| v.as_str()), Some("x"));
        let out: serde_json::Value = serde_json::to_value(&s).unwrap();
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("future_knob"));
        assert!(!obj.contains_key("extras"), "extras must flatten, not nest");
    }

    #[test]
    fn crew_round_trips_full_shape() {
        let json = r#"{
            "description": "deep review lineup",
            "seats": {
                "review-probe": [
                    {"profile": "fast", "k": 1, "bundle_selector": {"fact_families": ["security"]}},
                    {"profile": "deep", "model": "reserve-model"}
                ],
                "review-judge": [
                    {"profile": "balanced", "max_tokens": 4096}
                ]
            }
        }"#;
        let crew: Crew = serde_json::from_str(json).unwrap();
        assert_eq!(crew.description.as_deref(), Some("deep review lineup"));
        assert_eq!(crew.seats.len(), 2);
        let probe = crew.seats.get("review-probe").unwrap();
        assert_eq!(probe.len(), 2);
        assert_eq!(probe[0].profile, "fast");
        assert_eq!(probe[0].k, 1);
        assert_eq!(probe[1].model.as_deref(), Some("reserve-model"));
        let judge = crew.seats.get("review-judge").unwrap();
        assert_eq!(judge.len(), 1);
        assert_eq!(judge[0].max_tokens, Some(4096));

        let back: Crew = serde_json::from_str(&serde_json::to_string(&crew).unwrap()).unwrap();
        assert_eq!(back.seats.len(), 2);
        assert_eq!(back.seats.get("review-probe").unwrap().len(), 2);
        assert_eq!(back.seats.get("review-judge").unwrap().len(), 1);
    }

    #[test]
    fn crew_unknown_keys_land_in_extras() {
        // Mirrors `seat_staffing_unknown_keys_land_in_extras` one level up:
        // a Crew-level unknown key survives the round-trip flat (a newer
        // config read by an older binary).
        let json = r#"{"seats":{"review-probe":[{"profile":"fast"}]},"future_knob":"x"}"#;
        let crew: Crew = serde_json::from_str(json).unwrap();
        assert_eq!(crew.extras.get("future_knob").and_then(|v| v.as_str()), Some("x"));
        let out: serde_json::Value = serde_json::to_value(&crew).unwrap();
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("future_knob"));
        assert!(!obj.contains_key("extras"), "extras must flatten, not nest");
    }

    #[test]
    fn crew_empty_seats_parses_fine_at_schema_layer() {
        // Schema-layer only permits this; `darkmux-profiles::crews::resolve_crew`
        // rejects an empty `seats` map at validation time.
        let json = r#"{"seats": {}}"#;
        let crew: Crew = serde_json::from_str(json).unwrap();
        assert!(crew.seats.is_empty());
    }

    #[test]
    fn registry_parses_crews_section() {
        let json = r#"{
            "profiles": {
                "fast": {"models": [{"id": "a", "n_ctx": 1000}]},
                "deep": {"models": [{"id": "b", "n_ctx": 2000}]}
            },
            "crews": {
                "review-deep": {
                    "seats": {
                        "review-probe": [
                            {"profile": "fast"},
                            {"profile": "deep", "k": 1, "bundle_selector": {"fact_families": ["auth"]}}
                        ],
                        "review-judge": [
                            {"profile": "fast"}
                        ]
                    }
                }
            }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.crews.len(), 1);
        let c = reg.crews.get("review-deep").unwrap();
        assert_eq!(c.seats.get("review-probe").unwrap().len(), 2);
        assert_eq!(c.seats.get("review-judge").unwrap().len(), 1);

        // Round-trips.
        let back: ProfileRegistry =
            serde_json::from_str(&serde_json::to_string(&reg).unwrap()).unwrap();
        assert_eq!(back.crews.len(), 1);
    }

    /// The hard-line invariant: an old-shape registry with NO `crews` key
    /// parses and re-serializes structurally identical (compared as
    /// `serde_json::Value` — key order and whitespace aside) — adding the
    /// `crews` field must not disturb any pre-existing profiles.json.
    #[test]
    fn old_shape_registry_without_crews_round_trips_value_identical() {
        // `models: []` (rather than a populated model) sidesteps the
        // pre-existing, unrelated `ProfileModel.capabilities` always-present
        // quirk (`{}` reserializes even when absent on read) — this test's
        // scope is the `crews` field's omission, not that quirk.
        let old_shape = r#"{"profiles":{"fast":{"models":[]}},"default_profile":"fast"}"#;
        let reg: ProfileRegistry = serde_json::from_str(old_shape).unwrap();
        assert!(reg.crews.is_empty());
        let original: serde_json::Value = serde_json::from_str(old_shape).unwrap();
        let reserialized: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(original, reserialized);
    }

    #[test]
    fn profiles_schema_version_constant_is_minor_bump_shaped() {
        let parts: Vec<&str> = PROFILES_SCHEMA_VERSION.split('.').collect();
        assert_eq!(parts.len(), 2, "expected MAJOR.MINOR, got {PROFILES_SCHEMA_VERSION}");
    }

    // ─── coverage additions (#1222 Phase B — packet-1 gap sweep) ────

    /// JSON technically permits duplicate object keys. Deserializing into a
    /// `BTreeMap` (via `MapAccess`) inserts each pair in stream order, so a
    /// later duplicate key silently OVERWRITES the earlier one rather than
    /// merging the two `Vec<SeatStaffing>` lists or erroring. Documents the
    /// actual (default-serde) behavior so a future change is a deliberate
    /// decision, not a surprise.
    #[test]
    fn crew_seats_duplicate_json_key_last_one_wins() {
        let json = r#"{"seats":{
            "review-probe":[{"profile":"fast"}],
            "review-probe":[{"profile":"deep","k":5}]
        }}"#;
        let crew: Crew = serde_json::from_str(json).unwrap();
        assert_eq!(crew.seats.len(), 1, "duplicate key collapses to one entry, not merged");
        let staffings = crew.seats.get("review-probe").unwrap();
        assert_eq!(staffings.len(), 1);
        assert_eq!(staffings[0].profile, "deep");
        assert_eq!(staffings[0].k, 5);
    }

    /// `fact_families` absent, or explicitly `[]`, both default to an empty
    /// `Vec` and are elided on re-serialize (`skip_serializing_if =
    /// "Vec::is_empty"`); `max_bundles` survives independently.
    #[test]
    fn bundle_selector_empty_fact_families_with_max_bundles_round_trips() {
        let json = r#"{"max_bundles":5}"#;
        let sel: BundleSelector = serde_json::from_str(json).unwrap();
        assert!(sel.fact_families.is_empty());
        assert_eq!(sel.max_bundles, Some(5));
        let out: serde_json::Value = serde_json::to_value(&sel).unwrap();
        let obj = out.as_object().unwrap();
        assert!(!obj.contains_key("fact_families"), "empty Vec is elided on write");
        assert_eq!(obj.get("max_bundles").and_then(|v| v.as_u64()), Some(5));

        // Explicit `[]` parses identically to the field being absent.
        let json2 = r#"{"fact_families":[],"max_bundles":5}"#;
        let sel2: BundleSelector = serde_json::from_str(json2).unwrap();
        assert!(sel2.fact_families.is_empty());
        assert_eq!(sel2.max_bundles, Some(5));
    }

    /// `"crews":{}` present in the input is value-equivalent to the key
    /// being absent (`crews.is_empty()` either way), but is NOT
    /// byte-identical on re-serialize — `skip_serializing_if =
    /// "BTreeMap::is_empty"` elides the key entirely on write. Distinct
    /// from `old_shape_registry_without_crews_round_trips_value_identical`
    /// above, which starts from an input with no `crews` key at all.
    #[test]
    fn registry_crews_key_present_but_empty_elides_on_reserialize() {
        let json = r#"{"profiles":{},"crews":{}}"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert!(reg.crews.is_empty());
        let out = serde_json::to_string(&reg).unwrap();
        assert_eq!(out, r#"{"profiles":{}}"#);
        assert_ne!(
            out, json,
            "explicit empty crews key is elided on write, not preserved verbatim"
        );
    }

    /// Unicode (incl. non-Latin scripts + emoji) in `Crew::description` and
    /// a `seats` key round-trips untouched — the schema has no ASCII
    /// assumption baked in anywhere.
    #[test]
    fn crew_unicode_description_and_seat_id_round_trips() {
        let json = r#"{
            "description": "レビュー・チーム 🔎",
            "seats": {
                "審査-probe": [{"profile":"fast"}]
            }
        }"#;
        let crew: Crew = serde_json::from_str(json).unwrap();
        assert!(crew.description.as_deref().unwrap().contains("チーム"));
        assert!(crew.seats.contains_key("審査-probe"));
        let back: Crew = serde_json::from_str(&serde_json::to_string(&crew).unwrap()).unwrap();
        assert_eq!(back.description, crew.description);
        assert!(back.seats.contains_key("審査-probe"));
    }

    /// `max_tokens` is an unconstrained `u32` at the schema layer — no
    /// validation clamps it (`resolve_staffing` in `darkmux-profiles::crews`
    /// only checks `k`). Both extremes round-trip verbatim.
    #[test]
    fn seat_staffing_max_tokens_extremes_round_trip() {
        let json_zero = r#"{"profile":"fast","max_tokens":0}"#;
        let s0: SeatStaffing = serde_json::from_str(json_zero).unwrap();
        assert_eq!(s0.max_tokens, Some(0));

        let json_max = format!(r#"{{"profile":"fast","max_tokens":{}}}"#, u32::MAX);
        let smax: SeatStaffing = serde_json::from_str(&json_max).unwrap();
        assert_eq!(smax.max_tokens, Some(u32::MAX));
        let out: serde_json::Value = serde_json::to_value(&smax).unwrap();
        assert_eq!(out["max_tokens"].as_u64(), Some(u64::from(u32::MAX)));
    }

    /// KNOWN GAP found while writing this coverage sweep — NOT fixed here
    /// per the coverage-only mandate (#1222 Phase B packet-1 coverage PR).
    ///
    /// `Crew` and `SeatStaffing` both carry `#[serde(flatten)] extras` for
    /// forward-compat overflow (see `crew_unknown_keys_land_in_extras` and
    /// `seat_staffing_unknown_keys_land_in_extras` above, and the doc
    /// comments on both types) — matching the project's stated "lenient on
    /// read, all-Option + flatten-extras overflow" schema doctrine
    /// (CLAUDE.md, "Configuration (config.json)"). `BundleSelector` does
    /// NOT have an `extras` field. An unknown key nested inside
    /// `bundle_selector` is silently DROPPED on parse (default serde
    /// behavior for unrecognized fields with no `deny_unknown_fields`)
    /// instead of being preserved and re-emitted on write. This test
    /// documents the expected-per-doctrine behavior and is `#[ignore]`d
    /// because it currently fails against the real type.
    #[test]
    #[ignore = "BUG (found 2026-07-09, not fixed — coverage PR only): BundleSelector has \
                no forward-compat `extras` field, unlike sibling Crew/SeatStaffing; an \
                unknown key inside bundle_selector is silently dropped, not preserved"]
    fn bundle_selector_unknown_keys_are_preserved_like_its_siblings() {
        let json = r#"{"fact_families":["auth"],"max_bundles":2,"future_knob":"x"}"#;
        let sel: BundleSelector = serde_json::from_str(json).unwrap();
        let out: serde_json::Value = serde_json::to_value(&sel).unwrap();
        assert!(
            out.as_object().unwrap().contains_key("future_knob"),
            "future_knob was dropped — BundleSelector has no extras/flatten field"
        );
    }
}
