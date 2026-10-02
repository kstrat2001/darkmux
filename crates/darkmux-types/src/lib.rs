//! darkmux-types — foundation crate.
//!
//! Profile / ProfileRegistry / ProfileModel schema (extracted whole from the
//! former `src/types.rs`) plus the workspace-path resolver (`paths`, lifted
//! from `src/lab/paths.rs`) so downstream crates can depend on path resolution
//! without pulling in the lab crate.

#[cfg(unix)]
pub mod child_registry;
pub mod config;
pub mod config_access;
pub mod config_enum;
pub mod daemon_record;
pub mod data_version;
pub mod diagnostics;
pub mod dispatch_liveness;
pub mod endpoint;
#[cfg(any(test, feature = "test-support"))]
pub mod env_audit;
pub mod execution_id;
#[cfg(unix)]
pub mod flock;
#[cfg(unix)]
pub mod interrupt;
pub mod param_scalar;
pub mod paths;
pub mod profile_address;
pub mod residency_lease;
pub mod run_pause;
pub mod session_id;
pub mod shell;
pub mod size;
/// (#2695/#2697/#2698) The single test-isolation guard. Gated the same
/// way `env_audit` is: available to a crate's TEST build via the
/// `test-support` feature, absent from every release build.
#[cfg(any(test, feature = "test-support"))]
pub mod test_isolation;
pub mod style;
pub mod user_files;
pub mod workdir;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use endpoint::{
    CredentialSource, Dialect, Lenient, EndpointAuth, EndpointAuthType, EndpointError, EndpointKind, EndpointSource,
    BudgetPolicy, ManagedBackend, ModelEndpoint, UsageLimits, UsageWindow, WindowBudget,
};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema)]
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

/// The schema of a [`CapabilityProfile`] field, for the unknown-key gate
/// (`user_files`). Derived schemas lose a map's key type when the key enum
/// documents its variants, which would let a misspelled capability pass the
/// gate and then fail the load; this names each [`Capability`] token (read
/// from that enum's own schema, never listed here) as the only valid key.
/// Used only as `#[schemars(with)]`.
pub struct CapabilityProfileSchema;

impl schemars::JsonSchema for CapabilityProfileSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CapabilityProfile".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let enum_schema = schemars::schema_for!(Capability).to_value();
        let tokens = user_files::enum_tokens(&enum_schema);
        // A map (so `_comment` is an entry here, not a note) whose keys must
        // be a capability token.
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": {"type": "number"},
            "propertyNames": {"enum": tokens},
        })
    }
}

// (#2310 P1) `PartialEq` added so `ProfileModel` can compose into
// `ResolvedSeatStaffing`/`ResolvedReviewRoles` (darkmux-crew's
// resourcing.rs) and, through those, into `darkmux-lab`'s `ReviewContext`
// step-output body — see that struct's own doc. Every field already
// supports it (`CapabilityProfile` is a `BTreeMap<Capability, f32>`,
// `Capability`/`ModelEndpoint` both derive `PartialEq`, and
// `serde_json::Map`'s does too), so this is additive, not a new
// obligation on existing fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProfileModel {
    pub id: String,
    /// Context window. For a LOCAL model this is the load parameter (a
    /// minimum — see `darkmux_gestalt::ctx_sufficient`) and is REQUIRED at resolution
    /// time (`require_n_ctx`); for an endpoint-bearing model it is an
    /// optional *declared* ceiling (the provider owns the real window, so
    /// operators aren't forced to invent a number — #1282, needed by the
    /// #1260 remote seats). Optional at the SCHEMA layer either way: the
    /// local-requires-n_ctx rule is enforced where the value is consumed
    /// (dispatch load, swap, crew resolution) and surfaced by
    /// `darkmux doctor`, never on the hot load path (config-leniency
    /// contract, #1269).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Capability vector — what kinds of work this model is good at, and
    /// at what relative weights (code / reasoning / instruction_following
    /// / agentic_tool_use). The operator populates it from lab results
    /// (which models run well on their machine, at what context). Empty
    /// by default. **This is LIVE, not additive** (#450 phase 2 landed):
    /// `darkmux_crew::select::select_model` matches a role's requested
    /// capabilities against this vector — treating a wholly-unvectored model
    /// as a 0.5-everywhere generalist — with no machine tier in the decision,
    /// only capability + lab-vetted fit (#322). Reached from both dispatch
    /// paths, so populating this on ONE model in a profile can change WHICH
    /// model a role's dispatch runs on. Scoring is a no-op only while the
    /// role requests no capabilities or no model in the profile carries an
    /// offer vector. (#1530: this doc previously claimed "nothing scores
    /// against it," which read as an inert field and made a live selection
    /// knob look safe to populate blindly.)
    #[serde(default)]
    #[schemars(with = "CapabilityProfileSchema")]
    pub capabilities: CapabilityProfile,
    /// The endpoint this model is served from. Absent ⇒ the managed LM
    /// Studio default. Written as an id naming an entry of the registry's
    /// `endpoints` map (`"endpoint": "azure-east"`); an inline object is
    /// refused (`ProfileRegistry::inline_endpoint_rewrites` names the move).
    /// The loader materializes an id into the definition's fields; see
    /// [`endpoint`]. On an unmanaged endpoint `n_ctx` is a *declared* window
    /// (darkmux cannot load-set it) rather than a load parameter.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "endpoint::endpoint_field")]
    #[schemars(with = "Option<endpoint::EndpointFieldSchema>")]
    pub endpoint: Option<ModelEndpoint>,
    /// Forward-compat overflow — unknown keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ProfileModel {
    /// A model on the named endpoint `endpoint_json` declares (as an
    /// `endpoints` entry does), built without a registry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hosted_for_test(id: &str, n_ctx: Option<u32>, endpoint_json: serde_json::Value) -> Self {
        let mut ep: ModelEndpoint = serde_json::from_value(endpoint_json).expect("a valid endpoint definition");
        ep.source = EndpointSource::Named("azure".to_string());
        ProfileModel { id: id.to_string(), n_ctx, endpoint: Some(ep), ..Default::default() }
    }

    /// (#2902 step 3) What darkmux does at this model's endpoint. No endpoint
    /// ⇒ the managed LM Studio default. THE classification every consumer
    /// reads (dispatch routing, residency, doctor, `profile list`); an
    /// endpoint named by an id no `endpoints` entry defines is an error.
    pub fn endpoint_kind(&self) -> Result<EndpointKind, EndpointError> {
        match &self.endpoint {
            None => Ok(EndpointKind::Managed(ManagedBackend::Lmstudio)),
            Some(ep) => ep.kind(),
        }
    }

    /// Whether darkmux manages this model's residency (it loads it). `false`
    /// for an unmanaged endpoint AND for an unresolvable one: a model darkmux
    /// cannot place is never loaded on a guess.
    pub fn is_managed(&self) -> bool {
        self.endpoint_kind().is_ok_and(EndpointKind::is_managed)
    }

    /// (#2902 step 4) THE n_ctx rule: a managed model must declare the window
    /// it is loaded at. The one predicate `require_n_ctx`, the registry's
    /// validation and every placement path share.
    pub fn missing_managed_n_ctx(&self) -> bool {
        self.is_managed() && self.n_ctx.is_none()
    }

    /// (#1282) The declared context window a managed load requires.
    ///
    /// `n_ctx` is optional at the schema layer (an unmanaged endpoint has no
    /// load to size), so every path that LOADS the model resolves the window
    /// through this helper and gets ONE uniform, named error when it is
    /// missing. Unmanaged paths must not call this.
    pub fn require_n_ctx(&self) -> anyhow::Result<u32> {
        self.n_ctx.ok_or_else(|| {
            anyhow::anyhow!(
                "darkmux: model \"{}\" is local (no remote endpoint) but declares no `n_ctx` — \
                 a local load needs a context window. Add `n_ctx` to the model's registry \
                 entry (or an `endpoint` block if it's actually hosted); `darkmux doctor` \
                 lists affected entries. (#1282)",
                self.id
            )
        })
    }
}

/// Runtime block of a profile. (A `config_path` field — the removed
/// openclaw-config patch target — was deleted with the openclaw path in
/// #1405; the stale key still loads, and the unknown-key gate refuses it.)
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProfileRuntime {
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ReserveConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_token_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_compactions: Option<u32>,
}

/// Profile-level compaction config — typed surface for the v0.1
/// commitments (see #354) plus an `extras` overflow that carries any key
/// darkmux doesn't recognize (lenient on read, contract 7).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionStrategy {
    /// Today's default — narrative middle-replace via prose summary.
    Narrative,
    /// #352 tier-2 — structured-slot fact extraction.
    StructuredSlot,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Range 0.1-0.9. Defaults: unset → only the absolute threshold
    /// trigger applies. (#368)
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
    /// See DESIGN.md "Schema isolation: darkmux owns its own config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    /// Unrecognized keys, kept so a hand-edited `profiles.json` round-trips
    /// unchanged. Nothing reads them: a key with no typed field has no
    /// effect on compaction.
    #[serde(flatten)]
    #[schemars(skip)]
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
            ("phase_id", 256),
        ];
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }
}

// (#1426 phase 3) `RegistryHooks`/`ProfileHookCommand` (the registry's
// `hooks.pre_swap`/`hooks.post_swap` blocks) were deleted with the retired
// `swap` verb — swap's executor was their ONLY trigger, so the hooks retire
// with it. An operator profiles.json still carrying a `hooks` block parses
// fine (the `#[serde(flatten)] extras` overflow on `ProfileRegistry`), and
// the unknown-key gate refuses it, naming the removal.

/// Machine-level internal bindings — darkmux's own standing infrastructure
/// for this machine, a sibling to the operator's `profiles`. Decoupled from
/// any single profile so swapping a profile never changes the
/// compaction model. (#590)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegistryInternal {
    /// The machine's **utility model** — the standing support model darkmux
    /// runs its OWN jobs on (compaction, radio routing; #2914). The operator
    /// registers it from lab work; it is **not** capability-scored (a score
    /// could re-pick a large model for compaction and reintroduce the
    /// per-beat tax), and (#2914) it is NEVER selectable for a task: every
    /// task/step selection path excludes it, so a profile's `models[]` are
    /// work models only. One global utility model serves every utility job.
    ///
    /// `{ "id": .., "n_ctx": .. }` ([`UtilityBinding`]). The window is declared HERE, since #2914 the
    /// only place the compactor's own context comes from — not from a
    /// profile's `models[]` entry, which would make the utility model a work
    /// model. Absent ⇒ (#2571) NOT a fallback to a built-in default compactor
    /// — there is no runtime default any more.
    /// `CompactionDispatchArgs::apply_utility_model` leaves `compactor_model`
    /// unset, and an unset compactor means compaction is OFF outright for the
    /// dispatch (disclosed loudly at dispatch time, not silently defaulted).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_utility_binding")]
    pub utility: Option<UtilityBinding>,
}

/// The `internal.utility` value: `{ "id": "<model-id>", "n_ctx": <u32> }`. The
/// bare-string spelling was removed in 4.0 ([`deserialize_utility_binding`]
/// refuses it, naming this shape).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UtilityBinding {
    pub id: String,
    /// The window the utility model is loaded at, and the size a compaction
    /// payload is bounded by. `None` ⇒ undeclared: consumers that need one
    /// fall back to a NAMED default and say so (never silently).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    /// Overflow for keys this binary does not know, re-serialized flat: a
    /// registry written by a NEWER binary that added a field to this object
    /// still reads here without losing the field.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The operator line for a bare-string `internal.utility` naming `id`: what
/// 4.0 removed and the object to write in its place.
fn bare_utility_line(id: &str) -> String {
    format!(
        "`internal.utility` is a bare string (\"{id}\"), which 4.0 removed: write \
         `\"utility\": {{ \"id\": \"{id}\", \"n_ctx\": <the window it is loaded at> }}`"
    )
}

/// The dotted path of a registry document's `internal.utility` and its
/// operator line, when the document writes it as a bare string. Reads the
/// document, not the typed registry: such a document does not parse into one.
pub fn bare_utility_in(doc: &serde_json::Value) -> Option<(&'static str, String)> {
    let id = doc.get("internal")?.get("utility")?.as_str()?;
    Some(("internal.utility", bare_utility_line(id)))
}

/// `serde(deserialize_with)` for `internal.utility`: the object form only. A
/// bare string is refused with the object to write in its place, since the
/// typed parse of `internal` is not per-entry-quarantined and a vague error
/// here stops every dispatch.
fn deserialize_utility_binding<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<UtilityBinding>, D::Error> {
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(d)? {
        None => Ok(None),
        Some(serde_json::Value::String(id)) => Err(D::Error::custom(bare_utility_line(&id))),
        Some(other) => serde_json::from_value(other).map(Some).map_err(D::Error::custom),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The profile author's routing hint, kept as written. Free-form by contract
    /// (#3035), and nothing in darkmux reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_when: Option<serde_json::Value>,
    /// Forward-compat overflow — unknown keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    #[schemars(skip)]
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
/// are a **minor** bump; renaming/retyping a field is **major**. An unknown
/// key still loads (`extras`), but every dispatching entry point refuses it
/// (`user_files`), so an older binary refuses a newer registry's new key. Mirrors the
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
// 1.2 (#1282): `ProfileModel.n_ctx` is optional (endpoint-bearing models
// have no local context to declare; the local-requires-n_ctx rule moved to
// resolution time + doctor). Minor bump: every 1.1 registry parses
// unchanged under 1.2. Caveat for the reverse direction: a 1.2 registry
// that actually OMITS `n_ctx` fails an entry-level parse on a pre-1.2
// binary — the per-entry quarantine (also #1282) scopes that to the one
// entry on binaries that have it.
// 1.3 (#1266): additive `passes` on `SeatStaffing` (the judge seat's
// consensus depth — `passes: 1` single / `2` double-confirm default / `N`
// unanimous consensus). Minor bump: every 1.2 registry parses unchanged (the
// field defaults to 2 on read via `default_judge_passes`, reproducing today's
// double-confirm), per the lenient-read doctrine.
// 1.4 (#1302): additive `request_changes` on `Crew` (opt-in to a blocking
// `REQUEST_CHANGES` review event for confirmed findings; default `false` =>
// non-blocking `COMMENT` event). Minor bump: every 1.3 registry parses
// unchanged (the field defaults to `false` on read), per the lenient-read
// doctrine.
// 1.5 (#1312): additive `key_env` on `EndpointAuth` (the NAME of an env var
// holding an endpoint's API key — the headless-runner escape hatch, resolved
// ahead of the Keychain). Minor bump: every 1.4 registry parses unchanged (the
// field is `Option`, absent on read), per the lenient-read doctrine.
// 2.0 (#2914, darkmux 4.0): `internal.utility` is the object
// `{ "id": <model-id>, "n_ctx": <u32> }` (`UtilityBinding`), declaring the
// utility model's own context window where the binding lives. The bare
// string form is refused, naming the object to write. MAJOR bump: this
// RETYPES a value, and an older binary (`utility: Option<String>`) given the
// object form fails the whole registry's typed parse (`internal` is a typed
// field, not a quarantined per-entry one), which is a hard stop on every
// dispatch (#1269). Alongside it, the utility model stopped being a legal work
// model: no profile's `models[]` should list it (`darkmux doctor` flags
// one that does), and every task/step selection path excludes it.
// Also in 2.0 (#2902 step 4, same unreleased major): the top-level
// `endpoints` map, and `ProfileModel.endpoint` also accepting a STRING naming
// one of its entries. The map alone would be additive, but the string form
// retypes a value: a binary from before it reads `"endpoint": "<id>"` as a
// type error and quarantines that profile (#1282). Folded into 2.0 rather
// than a 2.1 because no binary has shipped 2.0 yet, so there is no released
// 2.x reader it could break. A profile model's inline endpoint OBJECT is
// removed in the same major (refused, naming the rewrite to `endpoints`), and
// an endpoint declares `managed` or a `url`: there is no implicit kind. An
// endpoint gained three optional fields (`managed`, `dialect`, `limits`).
// Also in 2.0 (#2902 step 5, same unreleased major, so no bump of its own):
// `limits` gained two optional fields, `policy` (`off` / `warn` / `wait`, a
// registered `ConfigEnum`, read leniently and refused at preflight when
// unregistered) and `warn_at` (a fraction in (0, 1)), and its rolling
// `window` budget is now ENFORCED on an unmanaged endpoint named by id. A
// `window` with neither `tokens` nor `calls` (the shipped all-null shape) is
// no budget. Additive: every earlier 2.0 registry reads unchanged.
pub const PROFILES_SCHEMA_VERSION: &str = "2.0";

/// Scopes a review probe seat's draws to a subset of fact families, and
/// optionally caps how many bundles it considers. Carried on
/// [`crate`]-external `ResolvedSeatStaffing` (the resourcing resolver's
/// per-seat output); the declared `crews` map that once held it retired in
/// the 2.0 crew-registry dissolution (#1426 ship-2).
// (#2310 P1) `PartialEq` added — same reason as `ProfileModel`'s own note:
// this is a field of `ResolvedSeatStaffing`, which needs it to compose into
// `ReviewContext`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BundleSelector {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fact_families: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bundles: Option<u32>,
    /// Forward-compat overflow — unknown keys land here and re-serialize
    /// flat (a newer config read by an older binary).
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#1282) One registry entry that failed its per-entry typed parse and was
/// quarantined instead of failing the whole registry load (config-leniency
/// contract #1269, one layer down). Runtime-only — populated by the
/// `darkmux-profiles` loader, never serialized. `error` preserves serde's
/// field-level message verbatim (e.g. ``missing field `id```) so the
/// operator can fix the entry in one look; `darkmux doctor` lists each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedEntry {
    pub kind: QuarantinedEntryKind,
    /// The entry's registry key (a profile name).
    pub name: String,
    /// serde's entry-level parse error, verbatim.
    pub error: String,
}

/// Which registry section a [`QuarantinedEntry`] came from. Only `Profile`
/// remains after the 2.0 crew-registry dissolution (#1426 ship-2) — the
/// `crews` map no longer parses into a typed field, so it can't quarantine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantinedEntryKind {
    Profile,
    /// (#2902 review M1) An `endpoints` entry: a model naming it reads as an
    /// unresolved reference, refused at use.
    Endpoint,
}

impl std::fmt::Display for QuarantinedEntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuarantinedEntryKind::Profile => write!(f, "profile"),
            QuarantinedEntryKind::Endpoint => write!(f, "endpoint"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProfileRegistry {
    pub profiles: BTreeMap<String, Profile>,
    /// Schema version — additive field/section adds are a minor bump;
    /// renaming/retyping a field is major (see `PROFILES_SCHEMA_VERSION`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    // (#1426 phase 3) A `hooks` field (RegistryHooks — pre/post-swap shell
    // commands) lived here until the `swap` verb retired; a registry still
    // carrying that block parses fine (it lands in `extras`) and the
    // unknown-key gate refuses it, naming the removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Machine-level internal bindings (the utility model, …) — sibling to
    /// `profiles`, so swapping a profile never disturbs darkmux's own
    /// standing infrastructure. (#590)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<RegistryInternal>,
    /// (#2902 step 4) The endpoints profile models name by id
    /// (`"endpoint": "<id>"`): where requests go, what darkmux does there
    /// (`managed`), the request dialect, where the credential lives, and the
    /// standard usage limits (parsed and shown by `darkmux doctor`, not
    /// enforced until #2902 step 5). The loader materializes each reference
    /// into the definition's fields (`materialize_endpoints`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, ModelEndpoint>,
    // (#1426 ship-2) The `crews` map retired from the profiles schema — a crew
    // is now a DERIVED VIEW of a mission's resourcing, not a declared entity
    // (round-4 decision). A profiles.json still carrying a `crews` key parses
    // fine (the key overflows into `extras` below), and the unknown-key gate
    // refuses it, naming the removal. Review staffing now comes from the
    // role→profile resolver (`darkmux_crew::resourcing`, #1475) — each review
    // role resolves via its binding: a `--param <role>=<profile>` launch
    // override, else the `role_profiles` map, else `default_profile`.
    /// (#1282) Entries whose per-entry typed parse failed — quarantined by
    /// the `darkmux-profiles` loader instead of blasting the whole file.
    /// Runtime-only state (never serialized): a quarantined name is absent
    /// from `profiles`; lookups on it surface the parse error here.
    #[serde(skip)]
    pub quarantined: Vec<QuarantinedEntry>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ProfileRegistry {
    /// The machine's registered utility model id (`internal.utility`), if any.
    /// `None` ⇒ no machine utility model registered. (#590) (#2571: NOT "consumers
    /// fall back to their built-in default" — there is no runtime default any
    /// more; consumers that overlay this onto a compactor binding
    /// (`CompactionDispatchArgs::apply_utility_model`) are left with
    /// `compactor_model: None`, which means compaction is OFF outright for the
    /// dispatch, disclosed loudly rather than silently defaulted.)
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
            .and_then(|i| i.utility.as_ref())
            .map(|u| u.id.trim())
            .filter(|s| !s.is_empty())
    }

    /// (#2914) The utility model's declared context window
    /// (`internal.utility.n_ctx`), when one is declared. `None`
    /// for an undeclared window, and whenever
    /// [`Self::utility_model_id`] is `None` (a window with no model is not a
    /// binding). Since #2914 this is the ONLY source of the compactor's own
    /// window; a profile's `models[]` is never consulted for it.
    pub fn utility_model_n_ctx(&self) -> Option<u32> {
        self.utility_model_id()?;
        self.internal.as_ref().and_then(|i| i.utility.as_ref()).and_then(|u| u.n_ctx)
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

    /// (#1282) The full, operator-facing error message for `name` when that
    /// PROFILE was quarantined at load (its per-entry typed parse failed).
    /// `None` ⇒ `name` is not a quarantined profile.
    ///
    /// Shared by every profile-resolution surface (`get_profile`, the
    /// dispatch resolvers around `resolve_active`) so a quarantined profile
    /// fails everywhere with ONE message shape — the entry's own parse error
    /// plus a `darkmux doctor` pointer — instead of a misleading "not
    /// found", a silent #1054 fallback to a different profile, or a probe of
    /// whatever LMStudio happens to have loaded. (Since #1426 ship-2 only
    /// profiles quarantine — the `crews` map retired from the schema.)
    /// (#2902 step 4) Match every `"endpoint": "<id>"` reference to its
    /// `endpoints` definition: the model's endpoint then carries the
    /// definition's fields and `EndpointSource::Named(id)`. An id the map
    /// does not define stays `Unresolved`, which every consumer refuses
    /// loudly (and `validate` reports). Idempotent. Run by the registry
    /// loader; a registry parsed some other way reads id references as
    /// unresolved, never as the managed default.
    pub fn materialize_endpoints(&mut self) {
        let endpoints = std::mem::take(&mut self.endpoints);
        for profile in self.profiles.values_mut() {
            for model in &mut profile.models {
                let Some(ep) = model.endpoint.as_mut() else { continue };
                let Some(id) = ep.named_id().map(str::to_string) else { continue };
                *ep = named_endpoint(&endpoints, &id);
            }
        }
        self.endpoints = endpoints;
    }

    /// (#2902) THE id lookup: `endpoints.<id>` as a named endpoint carrying
    /// the definition's fields, or an unresolved reference when the map does
    /// not define it (or its entry was quarantined). Shared by
    /// [`Self::materialize_endpoints`] and a mission step's `config.endpoint`
    /// id (`darkmux_crew::target::step_unmanaged_endpoint`).
    pub fn endpoint_named(&self, id: &str) -> ModelEndpoint {
        named_endpoint(&self.endpoints, id)
    }

    /// (#2902 step 4) THE registry validation: every rule about endpoints and
    /// windows, in one pass, with no I/O (credential PRESENCE is doctor's live
    /// check). `darkmux doctor` prints these; resolution refuses the same
    /// problems at use. Assumes [`Self::materialize_endpoints`] has run.
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (id, def) in &self.endpoints {
            if let Err(reason) = def.validate() {
                out.push(format!("endpoint \"{id}\": {reason}"));
            }
        }
        for (pname, profile) in &self.profiles {
            for m in &profile.models {
                if let Some(EndpointSource::Unresolved(id)) = m.endpoint.as_ref().map(|e| &e.source) {
                    out.push(format!(
                        "profile \"{pname}\" model \"{}\" names endpoint \"{id}\", {}",
                        m.id,
                        self.unresolved_reason(id)
                    ));
                }
                if m.missing_managed_n_ctx() {
                    out.push(format!(
                        "profile \"{pname}\" model \"{}\" is local (no endpoint) but declares no n_ctx — \
                         swap/dispatch on it will fail at resolution",
                        m.id
                    ));
                }
            }
        }
        out
    }

    /// Why endpoint `id` did not resolve: its entry (else, when `endpoints`
    /// was not an object, the whole value) is quarantined, or `endpoints`
    /// does not define it.
    fn unresolved_reason(&self, id: &str) -> String {
        let entry = self.quarantined.iter().filter(|q| q.kind == QuarantinedEntryKind::Endpoint);
        match entry.clone().find(|q| q.name == id).or_else(|| entry.clone().find(|q| q.name == "endpoints")) {
            Some(q) => format!("whose `endpoints` entry is quarantined ({})", q.error),
            None => "which `endpoints` does not define".to_string(),
        }
    }

    /// Every profile model in the profiles.json document `doc` that declares
    /// its endpoint as an inline object, which 4.0 refuses, with the id to
    /// move it to: each DISTINCT definition gets its host (or `lmstudio` for
    /// one with no `url`); when several distinct definitions share a host,
    /// the host plus the URL's last path segment (an Azure deployment name);
    /// then a numeric suffix until the id is unique, never reusing an id
    /// `endpoints` already defines. The same definition used by several
    /// models gets one id. Reads the document, not the typed registry: a
    /// profile with an inline object does not parse into one.
    pub fn inline_endpoint_rewrites(doc: &serde_json::Value) -> Vec<InlineEndpointRewrite> {
        let mut found: Vec<(String, ModelEndpoint)> = Vec::new(); // (path, definition)
        let profiles = doc.get("profiles").and_then(|p| p.as_object()).into_iter().flatten();
        for (pname, profile) in profiles {
            let models = profile.get("models").and_then(|m| m.as_array()).into_iter().flatten();
            for (i, model) in models.enumerate() {
                let Some(obj) = model.get("endpoint").filter(|e| e.is_object()) else { continue };
                let ep = serde_json::from_value::<ModelEndpoint>(obj.clone()).unwrap_or_default();
                found.push((format!("profiles.{pname}.models[{i}].endpoint"), ep));
            }
        }
        let taken: std::collections::BTreeSet<String> =
            doc.get("endpoints").and_then(|e| e.as_object()).into_iter().flat_map(|m| m.keys().cloned()).collect();
        let mut defs: Vec<(String, String, String)> = Vec::new(); // (key, host base, last segment)
        for (_, ep) in &found {
            let key = endpoint_key(ep);
            if defs.iter().any(|(k, _, _)| *k == key) {
                continue;
            }
            let base = ep.host().unwrap_or_else(|| "lmstudio".to_string());
            // The last segment of the URL's PATH (never the host).
            let last = ep
                .url
                .as_deref()
                .and_then(|u| u.split_once("://"))
                .and_then(|(_, rest)| rest.trim_end_matches('/').split_once('/'))
                .and_then(|(_, path)| path.rsplit('/').next())
                .filter(|seg| !seg.is_empty() && !seg.eq_ignore_ascii_case("v1"))
                .unwrap_or_default()
                .to_string();
            defs.push((key, base, last));
        }
        let mut taken = taken;
        let mut ids: BTreeMap<String, String> = BTreeMap::new();
        for (key, base, last) in &defs {
            let shared = defs.iter().filter(|(_, b, _)| b == base).count() > 1;
            let mut id = if shared && !last.is_empty() { format!("{base}-{last}") } else { base.clone() };
            let stem = id.clone();
            let mut n = 2;
            while taken.contains(&id) {
                id = format!("{stem}-{n}");
                n += 1;
            }
            taken.insert(id.clone());
            ids.insert(key.clone(), id);
        }
        found
            .into_iter()
            .map(|(path, ep)| {
                let suggested_id = ids.get(&endpoint_key(&ep)).cloned().unwrap_or_default();
                InlineEndpointRewrite { path, suggested_id }
            })
            .collect()
    }

    pub fn quarantine_error_for(&self, name: &str) -> Option<String> {
        self.quarantined
            .iter()
            .find(|q| q.kind == QuarantinedEntryKind::Profile && q.name == name)
            .map(|q| {
                format!(
                    "darkmux: profile \"{}\" is quarantined — its registry entry failed to \
                     parse: {}. Fix the entry, then verify with `darkmux doctor`. (#1282)",
                    name, q.error
                )
            })
    }
}

/// A definition's identity for de-duplicating inline endpoints: its
/// serialized form (the runtime-only `source` is not serialized).
fn endpoint_key(ep: &ModelEndpoint) -> String {
    serde_json::to_string(ep).unwrap_or_default()
}

fn named_endpoint(endpoints: &BTreeMap<String, ModelEndpoint>, id: &str) -> ModelEndpoint {
    match endpoints.get(id) {
        Some(def) => ModelEndpoint { source: EndpointSource::Named(id.to_string()), ..def.clone() },
        None => ModelEndpoint::reference(id),
    }
}

/// One profile model whose `endpoint` is an inline object (refused in 4.0):
/// where it is, and the `endpoints` id to move it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineEndpointRewrite {
    /// The dotted path of the model's `endpoint` (`profiles.p.models[0].endpoint`).
    pub path: String,
    pub suggested_id: String,
}

impl InlineEndpointRewrite {
    /// The operator line: what was removed and the exact rewrite.
    pub fn line(&self) -> String {
        format!(
            "an inline endpoint object was removed in 4.0: declare it once under `endpoints` and name it by id. \
             Move this object to `endpoints.\"{id}\"` and write `\"endpoint\": \"{id}\"` on the model",
            id = self.suggested_id
        )
    }
}

// `Serialize` so `darkmux serve`'s `/machine/status` endpoint (#87; renamed
// from `/model/status` in #1426) can
// return loaded-model state as JSON for the flow viewer's toolbar pill.
// `Deserialize` (#1426) so `darkmux machine status <id>` can read a roster
// peer's residents back out of that same endpoint's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LoadedModel {
    pub identifier: String,
    pub model: String,
    pub status: String,
    pub size: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub context: u64,
    /// (#2917) How many requests LM Studio reports WAITING on this
    /// instance (`lms ps --json`'s `queued`, from its CLI's
    /// `modelProcessingStateSchema = {status, queued}`). `None` when the
    /// listing did not carry the field (an older `lms`, or the text
    /// fallback): absent is "not reported", never "zero". Optional on the
    /// wire both ways, so `/machine/status` stays readable by an older peer
    /// and an older peer's payload stays readable here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub queued: Option<u64>,
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

    /// (#1282) `quarantine_error_for` — the shared message every
    /// profile-resolution surface raises on a quarantined name: carries the
    /// entry's own parse error + the `darkmux doctor` pointer, matches only
    /// PROFILE-kind quarantine entries, and is `None` for healthy names.
    #[test]
    fn quarantine_error_for_names_error_and_doctor_for_profiles_only() {
        let mut r = reg(&["fast"], Some("fast"));
        r.quarantined.push(QuarantinedEntry {
            kind: QuarantinedEntryKind::Profile,
            name: "broken".to_string(),
            error: "missing field `id`".to_string(),
        });
        let msg = r.quarantine_error_for("broken").expect("quarantined profile has a message");
        assert!(msg.contains("\"broken\""), "got: {msg}");
        assert!(msg.contains("missing field `id`"), "got: {msg}");
        assert!(msg.contains("darkmux doctor"), "got: {msg}");
        // Healthy profile ⇒ no message.
        assert_eq!(r.quarantine_error_for("fast"), None);
    }

    #[test]
    fn profile_model_round_trips() {
        let m = ProfileModel {
            endpoint: None,
            id: "x".to_string(),
            n_ctx: Some(32_000),
            capabilities: Default::default(),
            identifier: Some("alias".to_string()),
            extras: Default::default(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProfileModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.n_ctx, Some(32_000));
        assert_eq!(back.identifier.as_deref(), Some("alias"));
    }

    #[test]
    fn profile_model_omits_none_identifier() {
        let m = ProfileModel {
            endpoint: None,
            id: "x".to_string(),
            n_ctx: Some(1024),
            capabilities: Default::default(),
            identifier: None,
            extras: Default::default(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("identifier"));
    }

    /// (#1282) `n_ctx` is optional at the schema layer: an endpoint-bearing
    /// model parses without one, round-trips with the key ABSENT (not
    /// `null`/`0`), and `require_n_ctx` — the local-load resolution gate —
    /// errors with the model named.
    #[test]
    fn profile_model_n_ctx_absent_parses_and_round_trips_absent() {
        let json = r#"{
            "id": "gpt-4o",
            "endpoint": "azure"
        }"#;
        let m: ProfileModel = serde_json::from_str(json).unwrap();
        assert_eq!(m.n_ctx, None);
        assert!(!m.is_managed());
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("n_ctx"), "absent n_ctx must stay absent: {out}");
        let back: ProfileModel = serde_json::from_str(&out).unwrap();
        assert_eq!(back.n_ctx, None);
    }

    #[test]
    fn require_n_ctx_errors_on_local_model_without_one() {
        let m: ProfileModel = serde_json::from_str(r#"{"id":"qwen"}"#).unwrap();
        assert!(m.is_managed());
        let err = m.require_n_ctx().unwrap_err().to_string();
        assert!(err.contains("qwen"), "error names the model: {err}");
        assert!(err.contains("n_ctx"), "error names the field: {err}");
        // A declared window resolves cleanly.
        let ok: ProfileModel =
            serde_json::from_str(r#"{"id":"qwen","n_ctx":32000}"#).unwrap();
        assert_eq!(ok.require_n_ctx().unwrap(), 32_000);
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
        // A remote model names an `endpoints` entry by id; the entry carries
        // the URL + auth. The Keychain item NAME is stored, never the secret.
        let json = r#"{
            "profiles": {"p": {"models": [{"id": "gpt-5.1", "n_ctx": 200000, "endpoint": "azure"}]}},
            "endpoints": {"azure": {
                "url": "https://example-aoai.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                "api_version": "2025-01-01-preview",
                "auth": { "type": "api-key", "keychain": "darkmux-azure-example" }
            }}
        }"#;
        let mut r: ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        let m = &r.profiles["p"].models[0];
        let ep = m.endpoint.as_ref().expect("endpoint parsed");
        assert_eq!(ep.kind().unwrap(), EndpointKind::Unmanaged);
        assert_eq!(
            ep.url.as_deref(),
            Some("https://example-aoai.cognitiveservices.azure.com/openai/deployments/gpt-4o")
        );
        assert_eq!(ep.api_version.as_deref(), Some("2025-01-01-preview"));
        let auth = ep.auth.as_ref().expect("auth parsed");
        assert_eq!(auth.auth_type, Some(EndpointAuthType::ApiKey));
        assert_eq!(auth.keychain.as_deref(), Some("darkmux-azure-example"));
        // full round-trip preserves the endpoint, written back as its id
        let out = serde_json::to_value(&r).unwrap();
        assert_eq!(out["profiles"]["p"]["models"][0]["endpoint"], "azure");
        let mut back: ProfileRegistry = serde_json::from_value(out).unwrap();
        back.materialize_endpoints();
        assert_eq!(back.profiles["p"].models[0].endpoint, m.endpoint);
    }

    #[test]
    fn profile_model_absent_endpoint_is_local_and_omitted() {
        // Backward-compat: an existing profile with no endpoint parses fine,
        // endpoint is None (⇒ LMStudio local), and re-serializes without the key.
        let m: ProfileModel = serde_json::from_str(r#"{"id":"qwen","n_ctx":32000}"#).unwrap();
        assert!(m.endpoint.is_none());
        assert!(!serde_json::to_string(&m).unwrap().contains("endpoint"));
    }

    #[serial_test::serial]
    #[test]
    fn managed_lmstudio_endpoint_uses_the_configured_address() {
        // The managed endpoint's chat URL is the configured LM Studio address
        // (a URL).
        let ep = ModelEndpoint::managed_lmstudio();
        assert_eq!(ep.kind().unwrap(), EndpointKind::Managed(ManagedBackend::Lmstudio));
        let url = ep.chat_url().unwrap();
        assert!(url.contains("://"), "chat_url should be a URL, got {url:?}");
        assert!(ep.validate().is_ok());
    }

    #[test]
    fn endpoint_validate_catches_bad_url_and_authless_keychain() {
        // A URL without a scheme is rejected.
        let bad = ModelEndpoint {
            url: Some("example-aoai.azure.com".into()),
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
        // (#1312) `key_env` alone (no keychain) is a valid credential source.
        let via_env = ModelEndpoint {
            url: Some("https://x/".into()),
            auth: Some(EndpointAuth {
                auth_type: Some(EndpointAuthType::Bearer),
                keychain: None,
                key_env: Some("OPENAI_API_KEY".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(via_env.validate().is_ok(), "key_env alone should satisfy the credential source");
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
        assert_eq!(p.models[0].n_ctx, Some(32_000));
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
            "internal": { "utility": { "id": "darkmux:qwen3-4b-instruct-2507" } }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.utility_model_id(), Some("darkmux:qwen3-4b-instruct-2507"));
        // Round-trips back to the same shape.
        let back: ProfileRegistry =
            serde_json::from_str(&serde_json::to_string(&reg).unwrap()).unwrap();
        assert_eq!(back.utility_model_id(), Some("darkmux:qwen3-4b-instruct-2507"));
    }

    /// The bare-string `internal.utility` was removed in 4.0: the typed load
    /// refuses it, naming the object form with the id the file wrote.
    #[test]
    fn a_bare_string_utility_is_refused_naming_the_object_form() {
        let err = serde_json::from_str::<ProfileRegistry>(r#"{ "profiles": {}, "internal": { "utility": "util-4b" } }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare string") && err.contains("removed"), "{err}");
        assert!(err.contains(r#""utility": { "id": "util-4b", "n_ctx": "#), "names the object to write: {err}");
        for ok in [r#"{ "id": "u" }"#, r#"{ "id": "u", "n_ctx": 8 }"#, "null"] {
            let json = format!(r#"{{ "profiles": {{}}, "internal": {{ "utility": {ok} }} }}"#);
            serde_json::from_str::<ProfileRegistry>(&json).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let wrong = serde_json::from_str::<ProfileRegistry>(r#"{ "profiles": {}, "internal": { "utility": 5 } }"#).unwrap_err();
        assert!(wrong.to_string().contains("invalid type"), "{wrong}");
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
            let json = format!(r#"{{ "profiles": {{}}, "internal": {{ "utility": {{ "id": {blank} }} }} }}"#);
            let reg: ProfileRegistry = serde_json::from_str(&json).unwrap();
            assert_eq!(reg.utility_model_id(), None, "blank {blank} should be unset");
        }
        // A padded real id trims to the bare id (so it still matches loaded state).
        let json = r#"{ "profiles": {}, "internal": { "utility": { "id": "  darkmux:util-4b  " } } }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.utility_model_id(), Some("darkmux:util-4b"));
    }

    /// (#2914) `internal.utility` also accepts `{id, n_ctx}`: the utility
    /// model declares its own context window HERE, never in a profile's
    /// `models[]` (the compactor's window used to be looked up there, which
    /// made the utility model a work model). A bare string is refused.
    #[test]
    fn registry_internal_utility_accepts_id_and_n_ctx_object() {
        let json = r#"{
            "profiles": {},
            "internal": { "utility": { "id": "darkmux:util-4b", "n_ctx": 120000 } }
        }"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.utility_model_id(), Some("darkmux:util-4b"));
        assert_eq!(reg.utility_model_n_ctx(), Some(120_000));
        // Round-trips as the object form, window kept.
        let back: ProfileRegistry =
            serde_json::from_str(&serde_json::to_string(&reg).unwrap()).unwrap();
        assert_eq!(back.utility_model_id(), Some("darkmux:util-4b"));
        assert_eq!(back.utility_model_n_ctx(), Some(120_000));

        // An object with no `n_ctx` declares no window.
        let undeclared: ProfileRegistry =
            serde_json::from_str(r#"{ "profiles": {}, "internal": { "utility": { "id": "util-4b" } } }"#).unwrap();
        assert_eq!(undeclared.utility_model_id(), Some("util-4b"));
        assert_eq!(undeclared.utility_model_n_ctx(), None);

        // A blank/padded id in the object form gets the same treatment as a
        // blank/padded bare string.
        let blank: ProfileRegistry =
            serde_json::from_str(r#"{ "profiles": {}, "internal": { "utility": { "id": "  " } } }"#).unwrap();
        assert_eq!(blank.utility_model_id(), None);
        assert_eq!(blank.utility_model_n_ctx(), None, "no id, no binding, no window");
        let padded: ProfileRegistry =
            serde_json::from_str(r#"{ "profiles": {}, "internal": { "utility": { "id": " util-4b ", "n_ctx": 8 } } }"#).unwrap();
        assert_eq!(padded.utility_model_id(), Some("util-4b"));
        assert_eq!(padded.utility_model_n_ctx(), Some(8));
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

    /// (#2578) The READ-side twin the fix's own commit note names: the
    /// test above (`runtime_compaction_config_v0_1_shape_round_trips`)
    /// always supplies `slot_caps` explicitly, so it never exercises the
    /// omission path — which is exactly the shape a real operator writes.
    /// `Tier2Config::slot_caps` pairs `#[serde(default)]` with
    /// `skip_serializing_if = "BTreeMap::is_empty"`; an operator writing
    /// `{"tier2": {"schema_version": "0.1"}}` — the profiles registry's
    /// own on-disk shape (`~/.darkmux/profiles.json`), NOT a fixture — is
    /// the document that stops loading if `default` is ever dropped.
    ///
    /// **Proved failing first** (2026-09-10, this packet): dropping
    /// `default` from `Tier2Config::slot_caps` while keeping
    /// `skip_serializing_if`, rebuilding (`cargo build -p darkmux-types
    /// --tests`, confirmed exit 0), and running `cargo test -p
    /// darkmux-types --lib` left all 206 other tests in this crate green —
    /// this omission carried zero coverage of its own before this test.
    /// Restored before writing this test.
    #[test]
    fn tier2_config_with_slot_caps_omitted_round_trips() {
        // The exact operator-authored shape named in #2578: `tier2` present,
        // `slot_caps` omitted entirely (never even an empty `{}`).
        let json = r#"{"tier2": {"schema_version": "0.1"}}"#;
        let cfg: RuntimeCompactionConfig = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("an operator profile with slot_caps omitted must still deserialize: {e}"));
        let t2 = cfg.tier2.as_ref().expect("tier2 set");
        assert_eq!(t2.schema_version.as_deref(), Some("0.1"));
        assert!(t2.slot_caps.is_empty(), "an omitted slot_caps must default to empty, not fail to parse");

        // Round-trip fidelity: re-serializing must still omit the empty map.
        let out = serde_json::to_value(&cfg).unwrap();
        assert!(
            out["tier2"].as_object().unwrap().get("slot_caps").is_none(),
            "BTreeMap::is_empty should have skipped slot_caps on re-serialize"
        );
    }

    /// (#2578) The same READ-side omission-path gap, for
    /// `BundleSelector::fact_families` — persisted on every review run
    /// record. `bundle_selector_unknown_keys_are_preserved` (below)
    /// always supplies `fact_families` explicitly; this is the document
    /// that omits it entirely, which is what every unscoped probe seat's
    /// serialized selector actually looks like on disk.
    ///
    /// **Proved failing first** (2026-09-10, this packet): dropping
    /// `default` from `BundleSelector::fact_families` while keeping
    /// `skip_serializing_if`, rebuilding (confirmed exit 0), and running
    /// the crate's lib suite left all 206 other tests green. Restored
    /// before writing this test.
    #[test]
    fn bundle_selector_with_fact_families_omitted_round_trips() {
        let json = r#"{"max_bundles": 2}"#;
        let sel: BundleSelector = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("a selector with fact_families omitted must still deserialize: {e}"));
        assert!(sel.fact_families.is_empty(), "an omitted fact_families must default to empty");
        assert_eq!(sel.max_bundles, Some(2));

        let out = serde_json::to_value(&sel).unwrap();
        assert!(
            out.as_object().unwrap().get("fact_families").is_none(),
            "Vec::is_empty should have skipped fact_families on re-serialize"
        );
    }

    /// Unrecognized keys deserialize into `.extras` and re-serialize at
    /// the same nesting level as typed fields. A pre-#357 `profiles.json`
    /// parses unchanged.
    #[test]
    fn runtime_compaction_config_unknown_keys_land_in_extras() {
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

    /// Mixed v0.1 typed fields + unrecognized keys serialize to a
    /// single flat JSON object — the wire-format invariant that
    /// keeps `profiles.json` files diff-stable when an operator
    /// adds tier-2 fields next to keys darkmux does not read.
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
        assert_eq!(caps.get("phase_id"), Some(&256));
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
            queued: None,
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
        assert!(back.default_profile.is_none());
        assert!(back.internal.is_none());
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
        let json = r#"{"schema_version":"2.0","profiles":{"fast":{"description":"tiny profile","models":[{"id":"model-a","n_ctx":32000}],"default_model":"model-a"}},"hooks":{"pre_swap":[{"command":"echo swap-start","condition":"always"}],"post_swap":[{"command":"echo swap-end"}]},"default_profile":"fast","internal":{"utility":{"id":"darkmux:util-4b"}},"future_field":true}"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.schema_version.as_deref(), Some("2.0"));
        assert_eq!(reg.profiles.len(), 1);
        let p = reg.profiles.get("fast").unwrap();
        assert_eq!(p.description.as_deref(), Some("tiny profile"));
        assert_eq!(p.default_model_id(), Some("model-a"));
        // (#1426 phase 3) A legacy `hooks` block (retired with `swap`) is
        // lenient-on-read: it parses into `extras` and is ignored.
        assert!(reg.extras.contains_key("hooks"));
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
mod resourcing_residue_tests {
    //! (#1426 ship-2) Post-dissolution schema coverage: the `crews` map
    //! retired from `ProfileRegistry`, so a `crews` key must parse LENIENTLY
    //! (overflow into `extras`, re-serialize flat) rather than error. Plus the
    //! surviving `BundleSelector` forward-compat overflow.
    use super::*;

    /// A profiles.json still carrying a `crews` key (operator state we never
    /// touch) parses fine after dissolution: the key lands in `extras` and
    /// round-trips flat — the lenient-on-read contract.
    #[test]
    fn legacy_crews_key_overflows_into_extras_and_round_trips() {
        let json = r#"{"profiles":{},"crews":{"review-deep":{"seats":{"review-probe":[{"profile":"fast"}]}}}}"#;
        let reg: ProfileRegistry = serde_json::from_str(json).unwrap();
        // No typed `crews` field any more — the key is residue in `extras`.
        assert!(reg.extras.contains_key("crews"), "crews key must land in extras");
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out.as_object().unwrap().contains_key("crews"),
            "crews residue re-serializes flat, harmless"
        );
    }

    /// `BundleSelector` keeps its `#[serde(flatten)] extras` forward-compat
    /// overflow (it survives the dissolution as `ResolvedSeatStaffing`'s
    /// selector).
    #[test]
    fn bundle_selector_unknown_keys_are_preserved() {
        let json = r#"{"fact_families":["auth"],"max_bundles":2,"future_knob":"x"}"#;
        let sel: BundleSelector = serde_json::from_str(json).unwrap();
        let out: serde_json::Value = serde_json::to_value(&sel).unwrap();
        assert!(out.as_object().unwrap().contains_key("future_knob"));
    }
}
