//! Empirical model→profile heuristics, dispatched through a pluggable
//! provider registry.
//!
//! Each `HeuristicsProvider` claims a subset of hardware shapes (e.g. Apple
//! Silicon at 128 GB unified memory) and supplies its own rules table. The
//! first provider that `matches(&HardwareSpec)` wins. The `generic` provider
//! at the end of the list matches everything as a fallback — it warns
//! honestly that the heuristics aren't validated for the user's platform.
//!
//! The rules here encode empirical knowledge from PERFORMANCE.md and the
//! lab notebook — model size + architecture + task class → recommended
//! n_ctx, compactor pairing, runtime knob settings. These are *defaults*,
//! not laws. The expectation is that a user generates a draft profile via
//! these rules, then tunes from there.
//!
//! ## Adding a new provider
//!
//! 1. Add a file under this crate's `src/` (e.g. `nvidia_24gb_vram.rs`)
//! 2. Implement the `HeuristicsProvider` trait: `id`, `matches`, and a
//!    `RulesTable` of data returned from `rules`
//! 3. Add `pub mod <new>;` below
//! 4. Append `&<new>::PROVIDER` to the `PROVIDERS` static array
//!
//! The provider's `matches` function decides what hardware it claims; ordering
//! in the array is matching priority (most-specific first, generic last).

use darkmux_hardware::HardwareSpec;
use darkmux_profiles::lms::ModelMeta;
use serde::{Deserialize, Serialize};

pub mod m_series_128;
pub mod m_series_64;
pub mod m_series_32;
pub mod generic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskClass {
    /// Single-turn, slim ctx, no compactor. Fastest dispatch, lowest RAM.
    /// Right for: code review, audits, single-Q&A, classification, short summaries.
    Fast,
    /// Mid-range tasks with the v15.5 stack (companion compactor, tuned
    /// compaction knobs). Predictable across mixed workloads.
    /// Right for: TODO fills, focused refactors, file-scoped feature work.
    Mid,
    /// Maximum primary context with companion compactor for safety.
    /// Right for: open-ended audit, multi-file refactor, exploratory work.
    Long,
}

impl TaskClass {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fast" | "single-turn" | "single" => Some(TaskClass::Fast),
            "mid" | "balanced" | "middle" => Some(TaskClass::Mid),
            "long" | "long-agentic" | "deep" | "agentic" => Some(TaskClass::Long),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskClass::Fast => "fast",
            TaskClass::Mid => "mid",
            TaskClass::Long => "long",
        }
    }

    /// Column of this task class in a [`RulesTable`].
    const fn column(self) -> usize {
        match self {
            TaskClass::Fast => 0,
            TaskClass::Mid => 1,
            TaskClass::Long => 2,
        }
    }

    /// The phrase a drafted profile's description uses for this class.
    fn description_label(self) -> &'static str {
        match self {
            TaskClass::Fast => "single-turn / fast tasks",
            TaskClass::Mid => "mid-range / mixed tasks",
            TaskClass::Long => "long agentic / multi-turn tasks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    /// MoE — only a fraction of params active per token; lower per-token
    /// compute but full weights still need to be loaded into memory.
    Moe,
    /// Dense — all parameters active per token; throughput is fully a
    /// function of total params on Apple Silicon's bandwidth-limited regime.
    Dense,
    /// Unknown — fall back to dense assumptions (more conservative).
    Unknown,
}

impl Architecture {
    fn label(self) -> &'static str {
        match self {
            Architecture::Moe => "MoE",
            Architecture::Dense => "dense",
            Architecture::Unknown => "unknown-arch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SizeBucket {
    Tiny,   // < 8B
    Small,  // 8 – 15B
    Medium, // 15 – 50B
    Large,  // 50 – 100B
    Xl,     // 100B+
}

impl SizeBucket {
    /// Row of this bucket in a [`RulesTable`].
    const fn row(self) -> usize {
        match self {
            SizeBucket::Tiny => 0,
            SizeBucket::Small => 1,
            SizeBucket::Medium => 2,
            SizeBucket::Large => 3,
            SizeBucket::Xl => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SizeBucket::Tiny => "tiny",
            SizeBucket::Small => "small",
            SizeBucket::Medium => "medium",
            SizeBucket::Large => "large",
            SizeBucket::Xl => "XL",
        }
    }
}

/// Compactor pairing recommendation. `None` means no compactor — single-turn
/// or small-model workloads don't benefit from offload.
#[derive(Debug, Clone)]
pub struct CompactorChoice {
    pub model_id: String,
    pub n_ctx: u32,
}

#[derive(Debug, Clone)]
pub struct ProfileSuggestion {
    pub primary_n_ctx: u32,
    pub context_tokens: u64,
    /// The paired compactor, if any. A paired compactor is also what puts
    /// a `runtime.compaction` block in the drafted profile.
    pub compactor: Option<CompactorChoice>,
    pub description: String,
    /// Notes worth surfacing to the user about why this shape was picked
    /// (e.g., "context cut to 64K because RAM headroom on this model is
    /// tight at native 262K"). Renders as `_notes` field in the profile JSON.
    pub notes: Vec<String>,
}

/// The canonical compactor model used in the article series. Small (4B),
/// MLX-quantized, and validated to handle compaction summaries cleanly.
/// Public so providers in submodules can reference it.
pub const DEFAULT_COMPACTOR_ID: &str = "qwen3-4b-instruct-2507";

/// Default value for the typed `custom_instructions` darkmux field on
/// the tier-2 structured-slot compactor. V4 of the compactor-prompt
/// iteration (#402) — empirically validated via path-1 synthetic
/// harness (#239) + path-2 long-agentic pepper-test dispatch (issue
/// #402 PR body has the numbers).
///
/// Three guidance blocks, all generic across task shapes (works for
/// coder, code-reviewer, analyst, crawler, radio-router, etc.):
/// 1. Per-slot framing for `active_files` — agent's working knowledge
///    of files/artifacts; prevents post-compaction re-read rampage
/// 2. Per-slot framing for `verify_criteria` — operator-actionable
///    verification step rather than hallucinated tests-as-written
/// 3. Universal reality-discipline anchor — "every slot must reflect
///    what the excerpt SHOWS"; suppresses the optimistic-completion
///    hallucination V0-V3 baseline produced
///
/// Previous value was the v15.5-era openclaw-compatible string
/// ("Preserve verbatim: numeric SLAs..."); migrated 2026-05-26 after
/// Beat 41/45's retrace pattern surfaced the need for per-slot
/// reality-discipline framing. See PR #402 + Beat 45 lab notebook
/// for the empirical comparison.
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = "In `current_truth.active_files`, list each file or artifact the agent has read or modified, with one line per item naming WHAT was learned, decided, or changed about it. Format: `<path or URL>: <what was learned/decided/changed>`. Use a newline between items. This is the agent's working knowledge; the agent must NOT re-read these after compaction.

In `verify_criteria`, describe what an observer would RUN or LOOK AT to verify the agent's work is correct. Be specific about the verification ACTION (a command, an inspection step, a comparison) — NOT the expected outcome. Do NOT assume the work is done or correct; describe how someone would CHECK whether it is.

Reality-discipline (load-bearing): every slot must reflect what the excerpt SHOWS. Identifying an item ≠ completing it. Planning an edit ≠ making it. Naming a verification step ≠ running it. If the excerpt ends mid-task, every \"I will do X\" remains pending.";

pub fn classify_size_from_meta(meta: &ModelMeta) -> SizeBucket {
    if let Some(p) = meta.params_string.as_deref() {
        if let Some(b) = parse_param_bucket(p) {
            return b;
        }
    }
    // Fall back to size-on-disk as a proxy if paramsString is missing.
    classify_size_from_bytes(meta.size_bytes)
}

pub fn parse_param_bucket(params: &str) -> Option<SizeBucket> {
    let params = params.trim();
    let lower = params.to_ascii_lowercase();
    // Strip trailing "B" / "b" then parse as float
    let stripped = lower.trim_end_matches('b').trim();
    let parsed: f32 = stripped.parse().ok()?;
    Some(bucket_from_billions(parsed))
}

fn bucket_from_billions(b: f32) -> SizeBucket {
    if b < 8.0 {
        SizeBucket::Tiny
    } else if b < 15.0 {
        SizeBucket::Small
    } else if b < 50.0 {
        SizeBucket::Medium
    } else if b < 100.0 {
        SizeBucket::Large
    } else {
        SizeBucket::Xl
    }
}

/// Rough disk-size → bucket fallback. Quants vary widely; this is a coarse
/// approximation only. ~1 byte per param at 4-bit quantization is the
/// rule of thumb; double for 8-bit.
fn classify_size_from_bytes(bytes: u64) -> SizeBucket {
    let gb = bytes / (1024 * 1024 * 1024);
    if gb < 8 {
        SizeBucket::Tiny
    } else if gb < 16 {
        SizeBucket::Small
    } else if gb < 60 {
        SizeBucket::Medium
    } else if gb < 100 {
        SizeBucket::Large
    } else {
        SizeBucket::Xl
    }
}

pub fn classify_architecture(meta: &ModelMeta) -> Architecture {
    let Some(arch) = meta.architecture.as_deref() else {
        return Architecture::Unknown;
    };
    let lower = arch.to_ascii_lowercase();
    // Known MoE substrings / exact matches. Add new ones here as families ship.
    const MOE_NEEDLES: &[&str] = &[
        "moe",      // generic suffix (qwen3_5_moe, etc.)
        "a3b",      // Qwen 3.6 35B-A3B family
        "a10b",     // Qwen 3.5 122B-A10B family
        "gpt_oss",  // GPT-OSS family
        "next",     // Qwen3-Next is MoE despite the unmarked architecture tag
    ];
    if MOE_NEEDLES.iter().any(|n| lower.contains(n)) {
        Architecture::Moe
    } else {
        Architecture::Dense
    }
}

/// The looked-up cell of a provider's rules table, with the primary
/// already capped at the model's max context. `suggest_profile_for` turns
/// it into a `ProfileSuggestion` (compactor clamp, notes, description).
pub struct RuleResult {
    pub primary_n_ctx: u32,
    pub compactor: Option<CompactorChoice>,
}

/// One cell of a rules table: the primary's n_ctx before the model's
/// max-context cap, and the paired compactor's n_ctx (`None` = no
/// compactor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub primary_n_ctx: u32,
    pub compactor_n_ctx: Option<u32>,
}

impl Rule {
    /// A primary with no compactor.
    pub const fn solo(primary_n_ctx: u32) -> Self {
        Rule { primary_n_ctx, compactor_n_ctx: None }
    }

    /// A primary paired with a `DEFAULT_COMPACTOR_ID` compactor.
    pub const fn paired(primary_n_ctx: u32, compactor_n_ctx: u32) -> Self {
        Rule { primary_n_ctx, compactor_n_ctx: Some(compactor_n_ctx) }
    }
}

/// A provider's whole rules table as data: one row per [`SizeBucket`]
/// (Tiny, Small, Medium, Large, Xl), one column per [`TaskClass`] (Fast,
/// Mid, Long). Every provider is looked up through [`RulesTable::lookup`],
/// the one implementation of the cap and the compactor pairing.
pub struct RulesTable(pub [[Rule; 3]; 5]);

impl RulesTable {
    /// The `(bucket, task)` cell with the primary capped at `max_ctx`. The
    /// compactor is NOT capped here — `suggest_profile_for` clamps it
    /// against the capped primary.
    pub fn lookup(&self, bucket: SizeBucket, task: TaskClass, max_ctx: u32) -> RuleResult {
        let cell = self.0[bucket.row()][task.column()];
        RuleResult {
            primary_n_ctx: cell.primary_n_ctx.min(max_ctx),
            compactor: cell.compactor_n_ctx.map(|n_ctx| CompactorChoice {
                model_id: DEFAULT_COMPACTOR_ID.to_string(),
                n_ctx,
            }),
        }
    }
}

/// A pluggable rules table for a specific hardware shape. Providers are
/// registered statically below and matched in order (first match wins);
/// `generic` matches everything as a fallback. Each provider implements
/// `matches` (claim a hardware shape) and `rules` (its table, as data).
pub trait HeuristicsProvider: Sync {
    /// Stable identifier used in `_notes` and doctor output.
    fn id(&self) -> &'static str;
    /// Return `true` if this provider's rules apply to the given hardware.
    fn matches(&self, hw: &HardwareSpec) -> bool;
    /// This provider's rules table.
    fn rules(&self) -> &'static RulesTable;
    /// Look up the `(bucket, task)` cell, primary capped at `max_ctx`. The
    /// compactor clamp and contextTokens are `suggest_profile_for`'s job.
    fn suggest(&self, bucket: SizeBucket, task: TaskClass, max_ctx: u32) -> RuleResult {
        self.rules().lookup(bucket, task, max_ctx)
    }
    /// Optional extra notes to include in the suggestion's `_notes` field
    /// (e.g. "this provider's rules are extrapolated, not validated").
    fn extra_notes(&self) -> &[&'static str] {
        &[]
    }
}

/// The provider registry. Order matters — first-match-wins. Add specific
/// providers above `generic`. To register a new provider, see the module
/// docs at the top of this file.
static PROVIDERS: &[&dyn HeuristicsProvider] = &[
    &m_series_128::PROVIDER,
    &m_series_64::PROVIDER,
    &m_series_32::PROVIDER,
    &generic::PROVIDER,
];

/// Pick the provider that claims the given hardware. Walks `PROVIDERS` in
/// order and returns the first match. Always returns *some* provider —
/// `generic` is the last entry and matches everything.
pub fn active_provider(hw: &HardwareSpec) -> &'static dyn HeuristicsProvider {
    for p in PROVIDERS {
        if p.matches(hw) {
            return *p;
        }
    }
    // Unreachable in practice — generic matches anything — but be defensive.
    &generic::PROVIDER
}

/// Generate a profile suggestion for a model + task class. The suggestion
/// is a starting point — emit it as JSON, let the user tune. The `notes`
/// vec captures reasoning about non-obvious choices (e.g., RAM-driven ctx
/// cuts) so the user understands what they can change vs what's load-bearing.
pub fn suggest_profile(meta: &ModelMeta, task: TaskClass) -> ProfileSuggestion {
    let hw = darkmux_hardware::detect();
    suggest_profile_for(meta, task, &hw)
}

/// Same as `suggest_profile` but with an explicit `HardwareSpec` — useful
/// for testing and for tools (e.g. `darkmux doctor`) that have already
/// detected hardware once.
pub fn suggest_profile_for(
    meta: &ModelMeta,
    task: TaskClass,
    hw: &HardwareSpec,
) -> ProfileSuggestion {
    let bucket = classify_size_from_meta(meta);
    let arch = classify_architecture(meta);
    let max_ctx = meta.max_context_length.unwrap_or(32_000);

    let provider = active_provider(hw);
    let RuleResult { primary_n_ctx, compactor: compactor_raw } =
        provider.suggest(bucket, task, max_ctx);

    // Clamp the compactor's n_ctx so it never exceeds the (capped) primary.
    // This matters when max_ctx forces the primary down — e.g., a Medium
    // model with maxContextLength=40K gets a primary capped at 40K but the
    // canonical Mid compactor n_ctx is 68K. Without this clamp the compactor
    // would be larger than the primary, which is nonsensical.
    let compactor = compactor_raw.map(|c| CompactorChoice {
        model_id: c.model_id,
        n_ctx: c.n_ctx.min(primary_n_ctx),
    });

    // Set contextTokens to ~90% of primary loaded ctx, leaving headroom
    // for the runtime's reserve / per-turn overhead.
    let context_tokens = ((primary_n_ctx as f32) * 0.9).round() as u64;

    let mut notes = Vec::new();
    notes.push(format!(
        "Auto-drafted by `darkmux profile draft` from heuristics: provider={}, bucket={:?}, arch={:?}, task={:?}",
        provider.id(), bucket, arch, task
    ));
    for extra in provider.extra_notes() {
        notes.push((*extra).to_string());
    }
    if max_ctx < primary_n_ctx {
        notes.push(format!(
            "Model claims maxContextLength={max_ctx} but suggestion is {primary_n_ctx} — adjust if needed."
        ));
    }
    if matches!(bucket, SizeBucket::Large | SizeBucket::Xl) {
        notes.push(
            "Large/XL model — verify combined RAM (primary + compactor + KV pre-alloc) fits before relying on this."
                .to_string(),
        );
    }
    if !meta.trained_for_tool_use {
        notes.push(
            "Model is NOT marked trainedForToolUse — agentic dispatch may be unreliable."
                .to_string(),
        );
    }

    let description = format_description(meta, bucket, arch, task);

    ProfileSuggestion {
        primary_n_ctx,
        context_tokens,
        compactor,
        description,
        notes,
    }
}

fn format_description(
    meta: &ModelMeta,
    bucket: SizeBucket,
    arch: Architecture,
    task: TaskClass,
) -> String {
    let display = if meta.display_name.is_empty() { &meta.model_key } else { &meta.display_name };
    format!(
        "{display} ({} {}) tuned for {}.",
        bucket.label(),
        arch.label(),
        task.description_label()
    )
}

/// JSON-serializable form of a profile suggestion + the metadata needed to
/// emit a complete profile entry. Used by `darkmux profile draft`.
pub fn suggestion_to_profile_json(
    name: &str,
    model_id: &str,
    suggestion: &ProfileSuggestion,
    config_path: Option<&str>,
) -> serde_json::Value {
    let mut models = vec![serde_json::json!({
        "id": model_id,
        "n_ctx": suggestion.primary_n_ctx,
        "role": "primary",
    })];
    if let Some(c) = suggestion.compactor.as_ref() {
        models.push(serde_json::json!({
            "id": c.model_id,
            "n_ctx": c.n_ctx,
            "role": "compactor",
        }));
    }

    let mut runtime = serde_json::Map::new();
    if let Some(p) = config_path {
        runtime.insert("configPath".into(), serde_json::Value::String(p.into()));
    }
    runtime.insert(
        "contextTokens".into(),
        serde_json::Value::Number(suggestion.context_tokens.into()),
    );
    if let Some(c) = suggestion.compactor.as_ref() {
        let mut compaction = serde_json::Map::new();
        // (#385) Only darkmux-typed fields are written into heuristic-generated
        // profiles.
        compaction.insert(
            "model".into(),
            serde_json::Value::String(format!("lmstudio/{}", c.model_id)),
        );
        compaction.insert(
            "custom_instructions".into(),
            serde_json::Value::String(DEFAULT_COMPACTION_INSTRUCTIONS.into()),
        );
        runtime.insert(
            "compaction".into(),
            serde_json::Value::Object(compaction),
        );
    }

    serde_json::json!({
        name: {
            "_notes": suggestion.notes,
            "description": suggestion.description,
            "models": models,
            "runtime": runtime,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_hardware::{HardwareSpec, Platform};

    /// Synthesized M5 Max 128 GB fixture — falls into the `Xl` RAM tier
    /// so `active_provider` returns `m_series_128`. Used by tests that pin
    /// the M5-class rules table (262K bigctx, v15-5 shape, etc.) to assert
    /// against the right provider regardless of where the test runs.
    fn apple_silicon_128gb() -> HardwareSpec {
        HardwareSpec {
            platform: Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 128,
            physical_cores: 14,
            performance_cores: Some(10),
            efficiency_cores: Some(4),
            has_unified_memory: true,
        }
    }

    /// Synthesized M1 Max 32 GB fixture — falls into the `Small` RAM tier
    /// so `active_provider` returns `m_series_32`.
    fn apple_silicon_32gb() -> HardwareSpec {
        HardwareSpec {
            platform: Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 32,
            physical_cores: 10,
            performance_cores: Some(8),
            efficiency_cores: Some(2),
            has_unified_memory: true,
        }
    }

    fn meta(key: &str, params: Option<&str>, arch: Option<&str>, max_ctx: u32, size: u64) -> ModelMeta {
        ModelMeta {
            model_key: key.into(),
            display_name: format!("{key} display"),
            publisher: "test".into(),
            size_bytes: size,
            params_string: params.map(|s| s.into()),
            architecture: arch.map(|s| s.into()),
            max_context_length: Some(max_ctx),
            trained_for_tool_use: true,
            model_type: "llm".into(),
        }
    }

    /// Every provider's full rules table, pinned cell by cell as literal
    /// data written independently of the tables themselves: `(primary
    /// n_ctx, compactor n_ctx)` per `[bucket][task]`, buckets Tiny..Xl,
    /// tasks Fast/Mid/Long. A compactor, when present, is always
    /// `DEFAULT_COMPACTOR_ID`.
    type Golden = [[(u32, Option<u32>); 3]; 5];
    const GOLDEN_128: Golden = [
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (101_000, Some(68_000)), (262_144, Some(120_000))],
        [(32_000, None), (64_000, Some(32_000)), (101_000, Some(64_000))],
        [(32_000, None), (50_000, Some(32_000)), (101_000, Some(64_000))],
    ];
    const GOLDEN_64: Golden = [
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (64_000, Some(32_000)), (131_072, Some(64_000))],
        [(32_000, None), (32_000, None), (64_000, Some(32_000))],
        [(16_000, None), (32_000, None), (32_000, None)],
    ];
    const GOLDEN_32: Golden = [
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (64_000, None), (131_072, None)],
        [(32_000, None), (64_000, None), (64_000, Some(32_000))],
        [(16_000, None), (32_000, None), (64_000, None)],
        [(8_000, None), (16_000, None), (32_000, None)],
    ];
    const GOLDEN_GENERIC: Golden = [
        [(32_000, None), (32_000, None), (64_000, None)],
        [(32_000, None), (32_000, None), (64_000, None)],
        [(16_000, None), (32_000, None), (64_000, None)],
        [(8_000, None), (16_000, None), (32_000, None)],
        [(8_000, None), (16_000, None), (32_000, None)],
    ];
    const BUCKETS: [SizeBucket; 5] =
        [SizeBucket::Tiny, SizeBucket::Small, SizeBucket::Medium, SizeBucket::Large, SizeBucket::Xl];
    const TASKS: [TaskClass; 3] = [TaskClass::Fast, TaskClass::Mid, TaskClass::Long];

    fn goldens() -> [(&'static dyn HeuristicsProvider, &'static Golden); 4] {
        [
            (&m_series_128::PROVIDER, &GOLDEN_128),
            (&m_series_64::PROVIDER, &GOLDEN_64),
            (&m_series_32::PROVIDER, &GOLDEN_32),
            (&generic::PROVIDER, &GOLDEN_GENERIC),
        ]
    }

    #[test]
    fn every_provider_rules_table_is_pinned_cell_by_cell() {
        for (provider, golden) in goldens() {
            for (b, bucket) in BUCKETS.iter().enumerate() {
                for (t, task) in TASKS.iter().enumerate() {
                    let (want_ctx, want_compactor) = golden[b][t];
                    // The architecture is not an input to any table today:
                    // every arch must read the same cell.
                    let r = provider.suggest(*bucket, *task, u32::MAX);
                    let at = format!("{} {bucket:?}/{task:?}", provider.id());
                    assert_eq!(r.primary_n_ctx, want_ctx, "{at}: primary n_ctx");
                    assert_eq!(r.compactor.as_ref().map(|c| c.n_ctx), want_compactor, "{at}: compactor n_ctx");
                    if let Some(c) = &r.compactor {
                        assert_eq!(c.model_id, DEFAULT_COMPACTOR_ID, "{at}: compactor model");
                    }
                }
            }
        }
    }

    #[test]
    fn every_provider_caps_the_primary_at_max_ctx_but_not_the_compactor() {
        // The provider caps only the PRIMARY at the model's max context;
        // the compactor clamp against the capped primary is
        // `suggest_profile_for`'s job, not the table's.
        for (provider, golden) in goldens() {
            for (b, bucket) in BUCKETS.iter().enumerate() {
                for (t, task) in TASKS.iter().enumerate() {
                    let r = provider.suggest(*bucket, *task, 1_000);
                    let at = format!("{} {bucket:?}/{task:?}", provider.id());
                    assert_eq!(r.primary_n_ctx, 1_000, "{at}: primary capped");
                    assert_eq!(r.compactor.as_ref().map(|c| c.n_ctx), golden[b][t].1, "{at}: compactor uncapped");
                }
            }
        }
    }

    #[test]
    fn active_provider_picks_by_platform_and_ram_tier() {
        let at = |ram: u32| HardwareSpec { total_ram_gb: ram, ..apple_silicon_128gb() };
        assert_eq!(active_provider(&apple_silicon_32gb()).id(), "m-series-32");
        assert_eq!(active_provider(&at(8)).id(), "m-series-32");
        assert_eq!(active_provider(&at(33)).id(), "m-series-64");
        assert_eq!(active_provider(&at(64)).id(), "m-series-64");
        assert_eq!(active_provider(&at(65)).id(), "m-series-128");
        assert_eq!(active_provider(&apple_silicon_128gb()).id(), "m-series-128");
        let linux = HardwareSpec { platform: Platform::Linux, ..apple_silicon_128gb() };
        assert_eq!(active_provider(&linux).id(), "generic");
    }

    #[test]
    fn task_class_as_str_round_trips_through_parse() {
        for task in TASKS {
            assert_eq!(TaskClass::parse(task.as_str()), Some(task));
        }
        assert_eq!(TaskClass::Fast.as_str(), "fast");
        assert_eq!(TaskClass::Mid.as_str(), "mid");
        assert_eq!(TaskClass::Long.as_str(), "long");
    }

    #[test]
    fn format_description_names_size_arch_task_and_falls_back_to_the_key() {
        let mut m = meta("key-x", Some("4B"), None, 32_000, 0);
        let cases = [
            (SizeBucket::Tiny, Architecture::Moe, TaskClass::Fast, "(tiny MoE) tuned for single-turn / fast tasks."),
            (SizeBucket::Small, Architecture::Dense, TaskClass::Mid, "(small dense) tuned for mid-range / mixed tasks."),
            (SizeBucket::Medium, Architecture::Unknown, TaskClass::Long, "(medium unknown-arch) tuned for long agentic / multi-turn tasks."),
            (SizeBucket::Large, Architecture::Moe, TaskClass::Fast, "(large MoE)"),
            (SizeBucket::Xl, Architecture::Moe, TaskClass::Fast, "(XL MoE)"),
        ];
        for (bucket, arch, task, want) in cases {
            let d = format_description(&m, bucket, arch, task);
            assert!(d.starts_with("key-x display "), "{d}");
            assert!(d.contains(want), "{d} should contain {want}");
        }
        m.display_name.clear();
        assert_eq!(
            format_description(&m, SizeBucket::Tiny, Architecture::Dense, TaskClass::Fast),
            "key-x (tiny dense) tuned for single-turn / fast tasks."
        );
    }

    #[test]
    fn task_class_parses_aliases() {
        assert_eq!(TaskClass::parse("fast"), Some(TaskClass::Fast));
        assert_eq!(TaskClass::parse("Single-Turn"), Some(TaskClass::Fast));
        assert_eq!(TaskClass::parse("MID"), Some(TaskClass::Mid));
        assert_eq!(TaskClass::parse("long-agentic"), Some(TaskClass::Long));
        assert_eq!(TaskClass::parse("deep"), Some(TaskClass::Long));
        assert_eq!(TaskClass::parse("nonsense"), None);
    }

    #[test]
    fn parse_param_bucket_handles_real_strings() {
        assert_eq!(parse_param_bucket("4B"), Some(SizeBucket::Tiny));
        assert_eq!(parse_param_bucket("7.5B"), Some(SizeBucket::Tiny));
        assert_eq!(parse_param_bucket("8B"), Some(SizeBucket::Small));
        assert_eq!(parse_param_bucket("13B"), Some(SizeBucket::Small));
        assert_eq!(parse_param_bucket("32B"), Some(SizeBucket::Medium));
        assert_eq!(parse_param_bucket("35B"), Some(SizeBucket::Medium));
        assert_eq!(parse_param_bucket("70B"), Some(SizeBucket::Large));
        assert_eq!(parse_param_bucket("120B"), Some(SizeBucket::Xl));
        assert_eq!(parse_param_bucket("122B"), Some(SizeBucket::Xl));
    }

    #[test]
    fn moe_detected_from_architecture() {
        let m = meta("x", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        assert_eq!(classify_architecture(&m), Architecture::Moe);
        let m = meta("x", Some("70B"), Some("llama"), 131_072, 0);
        assert_eq!(classify_architecture(&m), Architecture::Dense);
        let m = meta("x", Some("120B"), Some("gpt_oss"), 131_072, 0);
        assert_eq!(classify_architecture(&m), Architecture::Moe);
        let m = meta("x", Some("4B"), None, 0, 0);
        assert_eq!(classify_architecture(&m), Architecture::Unknown);
    }

    // These tests pin the M5 Max 128 GB-class rules table (262K bigctx,
    // v15-5 shape, paired-compactor numbers). Previously `#[ignore]`d
    // because they called `suggest_profile` which detects from the running
    // host's hardware — fine on the dev rig, but CI's macos-latest runner
    // lands on `generic` with different numbers. Switching to
    // `suggest_profile_for` with the synthesized 128 GB fixture above
    // makes them hardware-agnostic without losing the M5-class coverage.
    #[test]
    fn medium_long_picks_bigctx_shape() {
        // Article 2 reference: 35B-A3B at long → 262K + 120K compactor
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile_for(&m, TaskClass::Long, &apple_silicon_128gb());
        assert_eq!(s.primary_n_ctx, 262_144);
        assert_eq!(s.compactor.as_ref().unwrap().n_ctx, 120_000);
    }

    #[test]
    fn medium_mid_picks_v15_5_shape() {
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile_for(&m, TaskClass::Mid, &apple_silicon_128gb());
        assert_eq!(s.primary_n_ctx, 101_000);
        let c = s.compactor.as_ref().unwrap();
        assert_eq!(c.n_ctx, 68_000);
    }

    #[test]
    fn fast_never_pairs_compactor() {
        for params in &["4B", "13B", "35B", "70B", "120B"] {
            let m = meta("x", Some(params), Some("qwen3"), 32_000, 0);
            let s = suggest_profile(&m, TaskClass::Fast);
            assert!(s.compactor.is_none(), "fast w/ {params} got compactor");
        }
    }

    #[test]
    fn ctx_capped_at_max_context_length() {
        // Model claims maxCtx=40K but the 128 GB-tier Mid rule suggests
        // 101K → should cap at 40K. Pin to the 128 GB fixture so the cap
        // behavior is exercised against the highest-ctx provider.
        let m = meta("x", Some("35B"), Some("qwen3_5_moe"), 40_000, 0);
        let s = suggest_profile_for(&m, TaskClass::Mid, &apple_silicon_128gb());
        assert_eq!(s.primary_n_ctx, 40_000);
    }

    #[test]
    fn tiny_models_skip_compactor_even_long() {
        let m = meta("phi", Some("4B"), Some("phi"), 131_072, 0);
        let s = suggest_profile(&m, TaskClass::Long);
        assert!(s.compactor.is_none());
    }

    #[test]
    fn suggestion_to_profile_json_emits_compaction_block_when_paired() {
        // 128 GB-tier Long task pairs a compactor → JSON should carry a
        // `runtime.compaction` block. Pin the fixture so the test doesn't
        // depend on whether the local rig's tier pairs a compactor.
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile_for(&m, TaskClass::Long, &apple_silicon_128gb());
        let json = suggestion_to_profile_json("test", "qwen3.6-35b-a3b", &s, None);
        let obj = json.as_object().unwrap().get("test").unwrap();
        let runtime = obj.get("runtime").unwrap();
        let compaction = runtime.get("compaction").unwrap();

        // (#385) Verify darkmux-typed fields are present.
        assert!(compaction.get("model").unwrap().as_str().unwrap().starts_with("lmstudio/"));
        assert!(compaction.get("custom_instructions").is_some());

        // (#385) Verify dead-letter openclaw-shape fields are absent.
        assert!(compaction.get("mode").is_none(), "mode should be absent (openclaw-shape)");
        assert!(compaction.get("maxHistoryShare").is_none(), "maxHistoryShare should be absent (openclaw-shape)");
        assert!(compaction.get("recentTurnsPreserve").is_none(), "recentTurnsPreserve should be absent (openclaw-shape)");
        assert!(compaction.get("customInstructions").is_none(), "customInstructions should be absent (openclaw-shape)");
    }

    #[test]
    fn suggestion_to_profile_json_omits_compaction_when_no_compactor() {
        let m = meta("phi", Some("4B"), Some("phi"), 32_000, 0);
        let s = suggest_profile(&m, TaskClass::Fast);
        let json = suggestion_to_profile_json("phi-fast", "phi", &s, None);
        let obj = json.as_object().unwrap().get("phi-fast").unwrap();
        let runtime = obj.get("runtime").unwrap();
        assert!(runtime.get("compaction").is_none());
    }

    #[test]
    fn fallback_size_classifier_reads_disk_bytes() {
        let m = meta("x", None, None, 32_000, 39 * 1024 * 1024 * 1024);
        // ~39 GB → Medium per the byte-fallback table.
        assert_eq!(classify_size_from_meta(&m), SizeBucket::Medium);
    }

    #[test]
    fn moe_detected_for_qwen3_next() {
        let m = meta("qwen3-coder-next-mlx", Some("80B"), Some("qwen3_next"), 262_144, 0);
        assert_eq!(classify_architecture(&m), Architecture::Moe);
    }

    /// Regression for code review #7: when max_ctx caps the primary down
    /// below the canonical compactor n_ctx, the compactor must clamp too —
    /// otherwise we'd ship "compactor bigger than primary" which is broken.
    /// Pin to the 128 GB fixture to exercise the canonical 101K/68K shape
    /// being capped down to 40K.
    #[test]
    fn compactor_n_ctx_clamps_to_capped_primary() {
        let m = meta("constrained-medium", Some("35B"), Some("qwen3_5_moe"), 40_000, 0);
        let s = suggest_profile_for(&m, TaskClass::Mid, &apple_silicon_128gb());
        assert_eq!(s.primary_n_ctx, 40_000); // capped from canonical 101K
        let c = s.compactor.expect("Mid medium pairs a compactor");
        // Canonical Mid medium compactor is 68K, but primary capped at 40K
        // so the compactor must clamp to ≤ 40K.
        assert!(
            c.n_ctx <= s.primary_n_ctx,
            "compactor n_ctx {} larger than primary {}",
            c.n_ctx,
            s.primary_n_ctx
        );
    }

    #[test]
    fn untrained_for_tool_use_adds_warning_note() {
        let mut m = meta("legacy", Some("7B"), Some("qwen2"), 4096, 0);
        m.trained_for_tool_use = false;
        let s = suggest_profile(&m, TaskClass::Fast);
        assert!(
            s.notes.iter().any(|n| n.contains("trainedForToolUse")),
            "expected tool-use warning in notes: {:?}",
            s.notes
        );
    }

    /// (#402) Pin the content of `DEFAULT_COMPACTION_INSTRUCTIONS` so a
    /// future silent edit of the constant fails CI. V4 was empirically
    /// validated (path-1 synthetic harness + path-2 long-agentic pepper
    /// test produced 100/100 tests pass with rich slot fills); the
    /// invariants below capture the load-bearing content that the
    /// empirical wins depended on:
    ///   1. Per-slot framing for `active_files` (prevents read-rampage)
    ///   2. Per-slot framing for `verify_criteria` (action-focused)
    ///   3. Universal reality-discipline anchor (suppresses optimistic
    ///      completion hallucinations)
    ///
    /// If you intentionally evolve the value, update the invariants
    /// here to match — or replace with an exact string snapshot if a
    /// more stringent pin is needed.
    #[test]
    fn default_compaction_instructions_carries_v4_invariants() {
        let v = DEFAULT_COMPACTION_INSTRUCTIONS;

        // Invariant 1: per-slot framing for active_files
        assert!(
            v.contains("`current_truth.active_files`"),
            "DEFAULT_COMPACTION_INSTRUCTIONS must include per-slot framing for active_files"
        );
        assert!(
            v.contains("must NOT re-read"),
            "DEFAULT_COMPACTION_INSTRUCTIONS must instruct against post-compaction re-reading (the Beat 41 retrace lever)"
        );

        // Invariant 2: per-slot framing for verify_criteria
        assert!(
            v.contains("`verify_criteria`"),
            "DEFAULT_COMPACTION_INSTRUCTIONS must include per-slot framing for verify_criteria"
        );
        assert!(
            v.contains("verification ACTION") || v.contains("verification action"),
            "DEFAULT_COMPACTION_INSTRUCTIONS verify_criteria framing must be action-focused"
        );

        // Invariant 3: universal reality-discipline anchor
        assert!(
            v.contains("Reality-discipline") || v.contains("reality-discipline"),
            "DEFAULT_COMPACTION_INSTRUCTIONS must include a reality-discipline anchor"
        );
        assert!(
            v.contains("Identifying an item ≠ completing it"),
            "DEFAULT_COMPACTION_INSTRUCTIONS must carry the chained negative-constraint pattern (the V4-fingerprint that empirically beat V0-V3)"
        );
    }
}
