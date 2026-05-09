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
//! 1. Add a file under `src/heuristics/` (e.g. `nvidia_24gb_vram.rs`)
//! 2. Implement the `HeuristicsProvider` trait
//! 3. Add `pub mod <new>;` below
//! 4. Append `&<new>::PROVIDER` to the `PROVIDERS` static array
//!
//! The provider's `matches` function decides what hardware it claims; ordering
//! in the array is matching priority (most-specific first, generic last).

use crate::hardware::HardwareSpec;
use crate::lms::ModelMeta;
use serde::{Deserialize, Serialize};

pub mod m_series_128;
pub mod m_series_64;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SizeBucket {
    Tiny,   // < 8B
    Small,  // 8 – 15B
    Medium, // 15 – 50B
    Large,  // 50 – 100B
    Xl,     // 100B+
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
    pub compactor: Option<CompactorChoice>,
    pub include_compaction_settings: bool,
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

/// The validated v15.5 compaction.customInstructions string, copied from
/// the article. Used whenever a compactor is paired.
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = "Preserve verbatim: numeric SLAs, version identifiers, codenames, named constants, file paths, magic numbers, and any user-provided values referenced by name elsewhere. These can look like decoration but are often referenced by later turns.";

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

/// One row in a provider's rules table. Pure data — the trait dispatch
/// returns this and the parent module turns it into a `ProfileSuggestion`
/// with shared post-processing (compactor clamp, notes, description).
pub struct RuleResult {
    pub primary_n_ctx: u32,
    pub compactor: Option<CompactorChoice>,
    pub include_compaction_settings: bool,
}

/// A pluggable rules table for a specific hardware shape. Providers are
/// registered statically below and matched in order (first match wins);
/// `generic` matches everything as a fallback. Each provider implements
/// `matches` (claim a hardware shape) and `suggest` (the rules table).
pub trait HeuristicsProvider: Sync {
    /// Stable identifier used in `_notes` and doctor output.
    fn id(&self) -> &'static str;
    /// One-line description for `darkmux doctor` and tooltips.
    fn description(&self) -> &'static str;
    /// Return `true` if this provider's rules apply to the given hardware.
    fn matches(&self, hw: &HardwareSpec) -> bool;
    /// Look up the rules row for `(bucket, arch, task)` capped at `max_ctx`.
    /// Implementations don't need to clamp the compactor or set contextTokens
    /// — the dispatcher does that uniformly.
    fn suggest(
        &self,
        bucket: SizeBucket,
        arch: Architecture,
        task: TaskClass,
        max_ctx: u32,
    ) -> RuleResult;
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
    let hw = crate::hardware::detect();
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
    let RuleResult {
        primary_n_ctx,
        compactor: compactor_raw,
        include_compaction_settings,
    } = provider.suggest(bucket, arch, task, max_ctx);

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
        include_compaction_settings,
        description,
        notes,
    }
}

// Rules tables live in submodules — see `m_series_128.rs`, `m_series_64.rs`,
// `generic.rs`. Constants needed across providers (compactor model id,
// compaction instruction string) are defined above.

fn format_description(
    meta: &ModelMeta,
    bucket: SizeBucket,
    arch: Architecture,
    task: TaskClass,
) -> String {
    let arch_label = match arch {
        Architecture::Moe => "MoE",
        Architecture::Dense => "dense",
        Architecture::Unknown => "unknown-arch",
    };
    let task_label = match task {
        TaskClass::Fast => "single-turn / fast tasks",
        TaskClass::Mid => "mid-range / mixed tasks",
        TaskClass::Long => "long agentic / multi-turn tasks",
    };
    let size_label = match bucket {
        SizeBucket::Tiny => "tiny",
        SizeBucket::Small => "small",
        SizeBucket::Medium => "medium",
        SizeBucket::Large => "large",
        SizeBucket::Xl => "XL",
    };
    let display = if meta.display_name.is_empty() {
        meta.model_key.clone()
    } else {
        meta.display_name.clone()
    };
    format!(
        "{display} ({size_label} {arch_label}) tuned for {task_label}."
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
    if suggestion.include_compaction_settings {
        let mut compaction = serde_json::Map::new();
        compaction.insert("mode".into(), serde_json::Value::String("default".into()));
        if let Some(c) = suggestion.compactor.as_ref() {
            compaction.insert(
                "model".into(),
                serde_json::Value::String(format!("lmstudio/{}", c.model_id)),
            );
        }
        compaction.insert(
            "maxHistoryShare".into(),
            serde_json::json!(0.35),
        );
        compaction.insert(
            "recentTurnsPreserve".into(),
            serde_json::Value::Number(5u32.into()),
        );
        compaction.insert(
            "customInstructions".into(),
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

    #[test]
    fn medium_long_picks_bigctx_shape() {
        // Article 2 reference: 35B-A3B at long → 262K + 120K compactor
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile(&m, TaskClass::Long);
        assert_eq!(s.primary_n_ctx, 262_144);
        assert!(s.compactor.is_some());
        assert_eq!(s.compactor.as_ref().unwrap().n_ctx, 120_000);
        assert!(s.include_compaction_settings);
    }

    #[test]
    fn medium_mid_picks_v15_5_shape() {
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile(&m, TaskClass::Mid);
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
            assert!(!s.include_compaction_settings);
        }
    }

    #[test]
    fn ctx_capped_at_max_context_length() {
        // Model claims maxCtx=40K but rules suggest 100K → should cap at 40K.
        let m = meta("x", Some("35B"), Some("qwen3_5_moe"), 40_000, 0);
        let s = suggest_profile(&m, TaskClass::Mid);
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
        let m = meta("qwen3.6-35b-a3b", Some("35B"), Some("qwen3_5_moe"), 262_144, 0);
        let s = suggest_profile(&m, TaskClass::Long);
        let json = suggestion_to_profile_json("test", "qwen3.6-35b-a3b", &s, None);
        let obj = json.as_object().unwrap().get("test").unwrap();
        let runtime = obj.get("runtime").unwrap();
        let compaction = runtime.get("compaction").unwrap();
        assert_eq!(compaction.get("mode").and_then(|v| v.as_str()), Some("default"));
        assert!(
            compaction.get("model").unwrap().as_str().unwrap().starts_with("lmstudio/")
        );
        assert!(compaction.get("customInstructions").is_some());
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
    #[test]
    fn compactor_n_ctx_clamps_to_capped_primary() {
        let m = meta("constrained-medium", Some("35B"), Some("qwen3_5_moe"), 40_000, 0);
        let s = suggest_profile(&m, TaskClass::Mid);
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
}
