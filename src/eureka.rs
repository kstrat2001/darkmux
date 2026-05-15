//! Auto-Eureka detection — surface known-misconfiguration patterns before
//! the operator hits them.
//!
//! Each rule corresponds to a class of bug an early-adopter operator has
//! actually hit, where surfacing it earlier (at `darkmux doctor` or in the
//! viewer's Anomalies panel) would have saved hours of debugging. The kind
//! of bug that fires here is usually a silent agreement-failure between
//! layers: OpenClaw thinks the model has 200K context, LMStudio loaded it
//! at 100K, neither warns the operator.
//!
//! # Schema versioning
//!
//! `RULES_SCHEMA_VERSION` is semver and ships in the `meta` row of
//! `instruments.jsonl` so the viewer can detect compatibility on file drop.
//! Bump rules:
//!
//! - **patch** — bug fix, message tweak, threshold adjustment that doesn't
//!   change semantics. Old viewers parse unchanged.
//! - **minor** — new rule kind or new optional field. Old viewers can
//!   safely ignore unknown rules (additive change).
//! - **major** — rename/retype a field, change `kind` enum, new required
//!   field. Old viewers must NOT trust the data. The viewer's
//!   `EXPECTED_RULES_SCHEMA_MAJOR` constant must move in the same PR.
//!
//! See `CLAUDE.md` for the full contract.
//!
//! # DRY architecture
//!
//! Rule **metadata** (id, name, kind, message_template, fix_hint) lives
//! here as the single source of truth. The CLI emits the metadata into
//! `instruments.jsonl`; the viewer renders findings using the metadata it
//! reads from the file, not from a duplicated JS copy.
//!
//! Rule **evaluation** is per-side: Rust evaluators here read config files
//! and `lms ps`; the viewer's JS evaluates the live-applicable subset
//! against streaming samples in `instruments.jsonl`. Different input data
//! shapes — same logical rules.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Semver of the rules schema. See module docs for bump rules.
///
/// **When bumping major:** update `EXPECTED_RULES_SCHEMA_MAJOR` in
/// `docs/viewer/index.html` in the same PR. The viewer-release-PR is the
/// contract.
pub const RULES_SCHEMA_VERSION: &str = "1.1.0";

/// Stable rule-id string for `ctx-window-mismatch`. Single source of truth
/// for both the rules table below *and* external dispatchers (like
/// `doctor::try_fix`) that need to recognize this rule without string-typo
/// risk. If the id is ever renamed, this constant is the only edit needed —
/// per the module-level rules-schema versioning policy, an id rename is a
/// **major** bump.
pub const RULE_ID_CTX_WINDOW_MISMATCH: &str = "ctx-window-mismatch";

/// Stable rule-id string for `agents-default-model-resolves`. Same
/// rename-bump-major contract as the constant above.
pub const RULE_ID_AGENTS_DEFAULT_MODEL_RESOLVES: &str = "agents-default-model-resolves";

/// Stable identifiers for the active rule set. Add new ones to the bottom.
/// The `kind` field on `RuleDef` carries this discriminant in the wire
/// format, so renames are a major version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleKind {
    /// OpenClaw config's registered `contextWindow` for a model differs
    /// from the context LMStudio actually loaded that model at.
    ContextWindowMismatch,
    /// `agents.defaults.compaction.mode` is `safeguard` — aggressive
    /// compaction; the `default` mode is safer for typical local-AI
    /// workloads.
    SafeguardCompactionMode,
    /// (Reserved — not evaluated in this version. See follow-up issue.)
    AggressiveSampler,
    /// Configured `contextWindow` exceeds the model architecture's max
    /// (from `lms ls`'s `max_context_length`). LMStudio will silently
    /// clamp.
    NCtxExceedsModelMax,
    /// `agents.defaults.compaction.model` references a model id that
    /// isn't currently loaded in LMStudio. Compaction will fall back to
    /// the primary (the ~30s-per-beat tax described in Article 2).
    CompactorNotLoaded,
    /// An agent's configured `model` field doesn't match any currently
    /// loaded primary. Dispatches to that agent will fail or fall back.
    PrimaryConfigDrift,
    /// Estimated KV pre-allocation + working set exceeds unified memory
    /// budget. Dispatch will likely OOM mid-run.
    MemoryHeadroomTight,
    /// `agents.defaults.model.primary` (or `agents.defaults.compaction.model`)
    /// in openclaw.json points at a bare model id that doesn't appear in
    /// `lms ls` — the pin is orphaned, and dispatch will fail with
    /// "model not found" the first time the default is used.
    AgentsDefaultModelResolves,
}

/// Severity of a rule firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth noting; doesn't block end-to-end use.
    Warn,
    /// Will produce broken or silently-wrong dispatches if not fixed.
    Fail,
}

/// Definition of a single rule. Cloned into the meta row of
/// `instruments.jsonl` so the viewer renders findings with the same
/// labels/messages without duplicating string literals.
///
/// String fields are owned (`String`) rather than `&'static str` so the
/// type round-trips cleanly through serde (the viewer never deserializes
/// this in Rust; the round-trip is for test parity and future tooling).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    /// Stable, human-readable id (e.g. `ctx-window-mismatch`).
    pub id: String,
    /// Discriminant for evaluator dispatch.
    pub kind: RuleKind,
    /// Display label.
    pub name: String,
    /// Grouping category (e.g. `config_drift`, `resource_pressure`).
    pub category: String,
    /// Default severity. Specific findings may downgrade to Warn at runtime.
    pub severity: Severity,
    /// One-line description of what the rule looks for.
    pub description: String,
    /// Hint pointing at the fix, surfaced when the rule fires.
    pub fix_hint: String,
}

/// The canonical rule set. Order is the display order in doctor.
///
/// **Adding a rule:** add an entry to the table, implement an evaluator
/// in `evaluate_one`, bump `RULES_SCHEMA_VERSION` minor.
pub fn all_rules() -> Vec<RuleDef> {
    // (id, kind, name, category, severity, description, fix_hint)
    let table: &[(
        &str,
        RuleKind,
        &str,
        &str,
        Severity,
        &str,
        &str,
    )] = &[
        (
            RULE_ID_CTX_WINDOW_MISMATCH,
            RuleKind::ContextWindowMismatch,
            "Context window mismatch",
            "config_drift",
            Severity::Fail,
            "OpenClaw config registers a contextWindow for a model that doesn't match \
             what LMStudio actually loaded.",
            "Align `models.providers.lmstudio.providers[*].models[].contextWindow` in \
             openclaw.json to match `lms ps` output.",
        ),
        (
            "safeguard-compaction-mode",
            RuleKind::SafeguardCompactionMode,
            "Compaction mode is `safeguard`",
            "compaction",
            Severity::Warn,
            "`agents.defaults.compaction.mode` is `safeguard` — aggressive compaction \
             that can produce silent-truncation failures on long-agentic workloads. \
             The `default` mode is safer for most local-AI workloads.",
            "Set `agents.defaults.compaction.mode: \"default\"` in openclaw.json unless \
             you specifically need safeguard's earlier-and-harder summarization.",
        ),
        (
            "aggressive-sampler",
            RuleKind::AggressiveSampler,
            "Aggressive sampler config",
            "sampler",
            Severity::Warn,
            "(Reserved — sampler heuristic evaluation deferred. Manually verify your \
             Top-K / Min-P / temperature combination is stable for tool-call generation.)",
            "See follow-up issue tracking this rule's implementation.",
        ),
        (
            "n-ctx-exceeds-model-max",
            RuleKind::NCtxExceedsModelMax,
            "n_ctx exceeds model max",
            "config_drift",
            Severity::Fail,
            "Configured `contextWindow` is larger than the model architecture's max \
             context. LMStudio will silently clamp to the architectural max, which \
             invalidates the rest of your compaction math.",
            "Either lower `contextWindow` in openclaw.json to match the model's max \
             (visible in `lms ls` for that model), or pick a model architecture with \
             the context budget you need.",
        ),
        (
            "compactor-not-loaded",
            RuleKind::CompactorNotLoaded,
            "Compactor model not loaded",
            "compaction",
            Severity::Warn,
            "`agents.defaults.compaction.model` references a model id that isn't \
             currently loaded in LMStudio. Compaction will fall back to the primary \
             model (the ~30s-per-beat tax described in Article 2).",
            "Load the compactor with `lms load <id> --context-length <N>`, add it to \
             your darkmux profile so swap loads it, or pick a different compactor id.",
        ),
        (
            "primary-config-drift",
            RuleKind::PrimaryConfigDrift,
            "Agent primary ↔ loaded model drift",
            "config_drift",
            Severity::Fail,
            "An agent's configured `model` field doesn't match any currently loaded \
             primary in LMStudio. Dispatches to that agent will fail or fall back to \
             the default model.",
            "Either update `agents.list[*].model` in openclaw.json to match the loaded \
             identifier, or swap to a darkmux profile that loads the configured model.",
        ),
        (
            "memory-headroom-tight",
            RuleKind::MemoryHeadroomTight,
            "Memory headroom tight",
            "resource_pressure",
            Severity::Warn,
            "Estimated KV pre-allocation (primary + compactor) plus working set is \
             close to the unified memory budget. Heavy dispatches may OOM mid-run.",
            "Either lower the primary or compactor `contextWindow`, unload other \
             models, or pick a slimmer darkmux profile (e.g. `balanced`/`fast`).",
        ),
        (
            RULE_ID_AGENTS_DEFAULT_MODEL_RESOLVES,
            RuleKind::AgentsDefaultModelResolves,
            "Default model pin resolves",
            "config_drift",
            Severity::Fail,
            "`agents.defaults.model.primary` (and `agents.defaults.compaction.model` \
             if set) in openclaw.json must point at a bare model id that exists in \
             `lms ls`. Otherwise dispatch will fail the first time the default is \
             used. This catches both orphaned pins (model evicted from LMStudio's \
             cache) and stale defaults left behind by a `darkmux swap` that didn't \
             rewrite the openclaw pointer.",
            "Either (a) re-download the pinned model in LMStudio, (b) re-run \
             `darkmux swap <profile>` so it loads what the pin expects, or (c) \
             edit `agents.defaults.model.primary` in openclaw.json directly. \
             `lms ls` shows what is currently downloadable.",
        ),
    ];
    table
        .iter()
        .map(|(id, kind, name, category, severity, description, fix_hint)| RuleDef {
            id: (*id).into(),
            kind: *kind,
            name: (*name).into(),
            category: (*category).into(),
            severity: *severity,
            description: (*description).into(),
            fix_hint: (*fix_hint).into(),
        })
        .collect()
}

// ─── Context + evaluation ──────────────────────────────────────────────

/// Inputs needed to evaluate the rule set. Built once per `doctor`/eureka
/// pass and passed to each evaluator.
pub struct Context {
    /// Parsed `~/.openclaw/openclaw.json`, or `None` if the file doesn't
    /// exist or failed to parse. Rules that depend on it return `Pass`
    /// with a "skipped" hint when this is `None`.
    pub openclaw_config: Option<serde_json::Value>,
    /// Loaded-model snapshot from `lms ps`.
    pub loaded_models: Vec<crate::types::LoadedModel>,
    /// Downloaded-model catalog from `lms ls`, used for arch-max lookups.
    /// `None` if the call failed.
    pub available_models: Option<Vec<crate::lms::ModelMeta>>,
    /// System RAM in GB (unified memory total for Apple Silicon).
    pub total_ram_gb: u32,
}

impl Context {
    /// Build a context by reading the current system state. Best-effort —
    /// failures populate `None` fields rather than erroring out, so the
    /// rule pass can degrade gracefully.
    pub fn collect() -> Self {
        let openclaw_config = read_openclaw_config();
        let loaded_models = crate::lms::list_loaded().unwrap_or_default();
        let available_models = crate::lms::list_available().ok();
        let total_ram_gb = crate::hardware::detect().total_ram_gb;
        Self {
            openclaw_config,
            loaded_models,
            available_models,
            total_ram_gb,
        }
    }
}

/// Normalize a model id for cross-source comparison. OpenClaw configs
/// often use a provider-scoped form like `lmstudio/qwen3-4b-instruct-2507`
/// while `lms ps` reports the bare identifier `qwen3-4b-instruct-2507`.
/// Both forms refer to the same model; trim everything before the last
/// `/` so comparisons line up.
fn normalize_model_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Match a model id from openclaw.json against the loaded set, accounting
/// for provider-prefix differences (e.g. `lmstudio/foo` vs `foo`).
fn loaded_match<'a>(
    loaded: &'a [crate::types::LoadedModel],
    want: &str,
) -> Option<&'a crate::types::LoadedModel> {
    let want_norm = normalize_model_id(want);
    loaded.iter().find(|m| {
        normalize_model_id(&m.identifier) == want_norm
            || normalize_model_id(&m.model) == want_norm
    })
}

fn read_openclaw_config() -> Option<serde_json::Value> {
    let home = dirs::home_dir()?;
    let path = home.join(".openclaw").join("openclaw.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Verdict for one rule against the current context.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Rule didn't fire — no misconfiguration matching this pattern.
    Pass,
    /// Pass with an informational message — useful for diagnostics that
    /// aren't a misconfiguration but the operator still benefits from
    /// seeing. Example: `agents-default-model-resolves` resolves the pin
    /// against `lms ls` (Pass), but the model isn't in `lms ps` yet, so
    /// the first dispatch will JIT-load it. The verdict is Pass; the
    /// payload is the JIT-load hint. See issue #101.
    ///
    /// Rust-internal today. When issue #11 lands and verdicts start
    /// shipping to `instruments.jsonl`, this variant becomes part of the
    /// wire format and the `RULES_SCHEMA_VERSION` contract gates that bump.
    PassWith(String),
    /// Rule fired. Message describes the specific finding; severity comes
    /// from the rule def by default but may be downgraded at runtime.
    Fire {
        severity: Severity,
        message: String,
    },
    /// Rule was skipped (e.g. couldn't read openclaw.json). Includes a
    /// short reason so doctor can surface "(skipped: ...)" rather than
    /// silently pretending all-clear.
    Skipped(String),
}

/// Evaluate all rules against the context. Returns a (def, verdict) pair
/// for each rule in `all_rules()` order. Doctor maps these to `Check`s.
pub fn evaluate_all(ctx: &Context) -> Vec<(RuleDef, Verdict)> {
    all_rules()
        .into_iter()
        .map(|def| {
            let verdict = evaluate_one(&def.kind, ctx);
            (def, verdict)
        })
        .collect()
}

fn evaluate_one(kind: &RuleKind, ctx: &Context) -> Verdict {
    match kind {
        RuleKind::ContextWindowMismatch => eval_ctx_window_mismatch(ctx),
        RuleKind::SafeguardCompactionMode => eval_safeguard_compaction(ctx),
        RuleKind::AggressiveSampler => Verdict::Skipped("not yet implemented".into()),
        RuleKind::NCtxExceedsModelMax => eval_n_ctx_exceeds_max(ctx),
        RuleKind::CompactorNotLoaded => eval_compactor_not_loaded(ctx),
        RuleKind::PrimaryConfigDrift => eval_primary_config_drift(ctx),
        RuleKind::MemoryHeadroomTight => eval_memory_headroom(ctx),
        RuleKind::AgentsDefaultModelResolves => eval_agents_default_model_resolves(ctx),
    }
}

// ─── Rule evaluators ───────────────────────────────────────────────────

fn eval_ctx_window_mismatch(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    if ctx.loaded_models.is_empty() {
        return Verdict::Skipped("no models loaded in LMStudio".into());
    }

    let mut mismatches: Vec<String> = Vec::new();
    let providers = config
        .get("models")
        .and_then(|m| m.get("providers"))
        .and_then(|p| p.as_object());
    let Some(providers) = providers else {
        return Verdict::Skipped("no models.providers in openclaw.json".into());
    };

    for (_pname, prov) in providers {
        let Some(models) = prov.get("models").and_then(|m| m.as_array()) else {
            continue;
        };
        for entry in models {
            let id = entry.get("id").and_then(|x| x.as_str());
            let configured = entry.get("contextWindow").and_then(|x| x.as_u64());
            let (Some(id), Some(configured)) = (id, configured) else {
                continue;
            };
            // Match by identifier or modelKey (provider-prefix-aware).
            let loaded = loaded_match(&ctx.loaded_models, id);
            let Some(loaded) = loaded else {
                continue;
            };
            if loaded.context != 0 && loaded.context != configured {
                mismatches.push(format!(
                    "{id}: config={configured}, loaded={}",
                    loaded.context
                ));
            }
        }
    }

    if mismatches.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fire {
            severity: Severity::Fail,
            message: format!("mismatch on {}: {}", mismatches.len(), mismatches.join("; ")),
        }
    }
}

fn eval_safeguard_compaction(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    let mode = config
        .get("agents")
        .and_then(|a| a.get("defaults"))
        .and_then(|d| d.get("compaction"))
        .and_then(|c| c.get("mode"))
        .and_then(|m| m.as_str());
    match mode {
        Some("safeguard") => Verdict::Fire {
            severity: Severity::Warn,
            message: "`agents.defaults.compaction.mode` is `safeguard` (aggressive)".into(),
        },
        Some("default") | None => Verdict::Pass,
        Some(other) => Verdict::Skipped(format!("unknown mode `{other}`")),
    }
}

fn eval_n_ctx_exceeds_max(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    let Some(catalog) = ctx.available_models.as_ref() else {
        return Verdict::Skipped("`lms ls` unavailable".into());
    };

    let mut breaches: Vec<String> = Vec::new();
    let providers = config
        .get("models")
        .and_then(|m| m.get("providers"))
        .and_then(|p| p.as_object());
    let Some(providers) = providers else {
        return Verdict::Pass;
    };

    for (_pname, prov) in providers {
        let Some(models) = prov.get("models").and_then(|m| m.as_array()) else {
            continue;
        };
        for entry in models {
            let id = entry.get("id").and_then(|x| x.as_str());
            let configured = entry.get("contextWindow").and_then(|x| x.as_u64());
            let (Some(id), Some(configured)) = (id, configured) else {
                continue;
            };
            // Look up the arch max from `lms ls` catalog (provider-prefix-aware).
            let id_norm = normalize_model_id(id);
            let max = catalog
                .iter()
                .find(|m| normalize_model_id(&m.model_key) == id_norm)
                .and_then(|m| m.max_context_length)
                .map(|x| x as u64);
            let Some(max) = max else {
                continue;
            };
            if configured > max {
                breaches.push(format!("{id}: config={configured}, model_max={max}"));
            }
        }
    }

    if breaches.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fire {
            severity: Severity::Fail,
            message: format!("{} model(s) over arch max: {}", breaches.len(), breaches.join("; ")),
        }
    }
}

// Companion to `agents-default-model-resolves` (added #91): that rule
// checks the `lms ls` (downloadable) half at Fail severity; this one
// checks the `lms ps` (loaded-now) half at Warn severity. On a fully-
// missing compactor both fire with complementary messages, which is
// intentional — the operator sees both the "won't load at all" Fail
// and the "won't be warm at dispatch time" Warn.
fn eval_compactor_not_loaded(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    let compactor_id = config
        .get("agents")
        .and_then(|a| a.get("defaults"))
        .and_then(|d| d.get("compaction"))
        .and_then(|c| c.get("model"))
        .and_then(|m| m.as_str());
    let Some(compactor_id) = compactor_id else {
        // No compactor configured at all — Pass; that's a valid setup
        // (primary handles its own compaction; user accepts the tax).
        return Verdict::Pass;
    };

    let loaded = loaded_match(&ctx.loaded_models, compactor_id).is_some();
    if loaded {
        Verdict::Pass
    } else {
        Verdict::Fire {
            severity: Severity::Warn,
            message: format!(
                "`agents.defaults.compaction.model = {compactor_id}` not in `lms ps`"
            ),
        }
    }
}

fn eval_primary_config_drift(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    let agents = config
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array());
    let Some(agents) = agents else {
        return Verdict::Pass;
    };

    let mut drifts: Vec<String> = Vec::new();
    for agent in agents {
        let agent_id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("(unnamed)");
        let Some(want) = agent.get("model").and_then(|m| m.as_str()) else {
            continue;
        };
        let matched = loaded_match(&ctx.loaded_models, want).is_some();
        if !matched {
            drifts.push(format!("{agent_id} → `{want}`"));
        }
    }

    if drifts.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fire {
            severity: Severity::Fail,
            message: format!(
                "{} agent(s) reference unloaded model: {}",
                drifts.len(),
                drifts.join(", ")
            ),
        }
    }
}

fn eval_memory_headroom(ctx: &Context) -> Verdict {
    if ctx.total_ram_gb == 0 {
        return Verdict::Skipped("ram total unavailable".into());
    }
    if ctx.loaded_models.is_empty() {
        return Verdict::Skipped("no models loaded".into());
    }
    let total_ram_gb = ctx.total_ram_gb;

    // Rough estimate: per the empirical findings in Article 2, KV
    // pre-allocation on Apple Silicon scales roughly linearly with
    // configured context length, on the order of ~0.5 GB per 32K
    // tokens for a Qwen-class model at typical quantization. We sum
    // (size_gb + 0.5 * ctx_k / 32) per loaded model and flag if the
    // total exceeds 80% of unified memory.
    let mut estimated_gb: f64 = 0.0;
    for m in &ctx.loaded_models {
        let size_gb = parse_size_gb(&m.size).unwrap_or(0.0);
        let kv_gb = 0.5 * (m.context as f64) / 32_768.0;
        estimated_gb += size_gb + kv_gb;
    }
    let budget_gb = total_ram_gb as f64;
    let pct = (estimated_gb / budget_gb) * 100.0;
    if pct > 80.0 {
        Verdict::Fire {
            severity: Severity::Warn,
            message: format!(
                "estimated working set ~{estimated_gb:.1} GB of {budget_gb:.0} GB unified ({pct:.0}%)",
            ),
        }
    } else {
        Verdict::Pass
    }
}

/// Strip the provider prefix (`lmstudio/`, `ollama/`, etc.) and the darkmux
/// namespace prefix (`darkmux:`) from a model id, leaving the bare key as
/// it appears in `lms ls`'s `modelKey` field.
///
/// Examples:
///   `"lmstudio/darkmux:qwen3.6-35b-a3b"` → `"qwen3.6-35b-a3b"`
///   `"darkmux:foo"` → `"foo"`
///   `"foo"` → `"foo"`
///   `"lmstudio/openai/gpt-oss-20b"` → `"openai/gpt-oss-20b"`
///   `"lmstudio/darkmux:openai/gpt-oss-20b"` → `"openai/gpt-oss-20b"`
///
/// LMStudio's `modelKey` is the publisher-prefixed form (`openai/gpt-oss-20b`,
/// `google/gemma-3-4b`) — see `lms.rs::list_available`. The earlier
/// `rsplit('/').next()` implementation over-stripped when the model id
/// included a publisher segment, so any pin pointing at an LMStudio model
/// with a publisher prefix would falsely fail the orphan check.
fn strip_model_id_prefixes(id: &str) -> &str {
    let after_provider = KNOWN_PROVIDER_PREFIXES
        .iter()
        .find_map(|p| id.strip_prefix(p))
        .unwrap_or(id);
    after_provider.strip_prefix("darkmux:").unwrap_or(after_provider)
}

// Provider tokens openclaw uses for routing — split from the model id
// itself so a `<publisher>/<model>` pair (the standard LMStudio catalog
// shape) survives. Add to this list when openclaw gains a new provider.
const KNOWN_PROVIDER_PREFIXES: &[&str] = &[
    "lmstudio/",
    "ollama/",
    "openrouter/",
];

/// Pure verdict logic for the `agents-default-model-resolves` check.
/// Extracted so we can unit-test prefix-stripping + the
/// orphaned/downloaded/loaded matrix without an `lms` round-trip.
///
/// `primary_id`/`compaction_id` are the raw strings as they appear in
/// openclaw.json (i.e. may carry `lmstudio/` or `darkmux:` prefixes).
/// `available_keys` is the `modelKey` set from `lms ls`.
/// `loaded_idents` is the `identifier` set from `lms ps` (these typically
/// carry the `darkmux:` namespace — the helper strips it for comparison).
///
/// Verdict tiers (per #91 spec + #101 follow-up):
///   - Fail   — any pin not in `lms ls`
///   - PassWith — all pins resolve, but at least one is downloaded-only
///     (will JIT-load on first dispatch — operator-actionable signal)
///   - Pass   — all pins resolve AND are already loaded
fn classify_default_model_resolves(
    primary_id: Option<&str>,
    compaction_id: Option<&str>,
    available_keys: &[&str],
    loaded_idents: &[&str],
) -> Verdict {
    // Build (label, raw, bare) triples for whichever pins are configured.
    let mut pins: Vec<(&str, &str, &str)> = Vec::new();
    if let Some(p) = primary_id {
        pins.push(("primary", p, strip_model_id_prefixes(p)));
    }
    if let Some(c) = compaction_id {
        pins.push(("compaction", c, strip_model_id_prefixes(c)));
    }
    if pins.is_empty() {
        return Verdict::Skipped("no agents.defaults.model.primary set".into());
    }

    // Loaded ids normalize through the same prefix-strip pipeline so the
    // pin (`lmstudio/darkmux:openai/gpt-oss-20b`) compares cleanly against
    // the loaded identifier (`darkmux:openai/gpt-oss-20b`).
    let loaded_bare: Vec<&str> = loaded_idents
        .iter()
        .map(|li| strip_model_id_prefixes(li))
        .collect();

    let mut orphans: Vec<String> = Vec::new();
    let mut downloaded_only: Vec<String> = Vec::new();
    for (label, _raw, bare) in &pins {
        if !available_keys.contains(bare) {
            // Fail-tier: pin doesn't even exist in `lms ls`.
            let raw_pin = pins
                .iter()
                .find(|(l, _, _)| l == label)
                .map(|(_, raw, _)| *raw)
                .unwrap_or("?");
            orphans.push(format!("{label} `{raw_pin}` (bare `{bare}`)"));
        } else if !loaded_bare.contains(bare) {
            // Pass-with-hint: downloaded but not currently resident.
            downloaded_only.push(format!("{label} `{bare}`"));
        }
    }

    if !orphans.is_empty() {
        // Top-3 lms-ls suggestions to help the operator pick a real id.
        let suggestions: Vec<&str> = available_keys.iter().take(3).copied().collect();
        let suggestion_line = if suggestions.is_empty() {
            String::from("`lms ls` is empty")
        } else {
            format!("first ids in `lms ls`: {}", suggestions.join(", "))
        };
        Verdict::Fire {
            severity: Severity::Fail,
            message: format!(
                "{} pin(s) not in `lms ls`: {}. {}",
                orphans.len(),
                orphans.join("; "),
                suggestion_line
            ),
        }
    } else if !downloaded_only.is_empty() {
        Verdict::PassWith(format!(
            "downloaded but not loaded — will JIT-load on first dispatch: {}",
            downloaded_only.join(", ")
        ))
    } else {
        Verdict::Pass
    }
}

fn eval_agents_default_model_resolves(ctx: &Context) -> Verdict {
    let Some(config) = ctx.openclaw_config.as_ref() else {
        return Verdict::Skipped("no ~/.openclaw/openclaw.json".into());
    };
    let Some(catalog) = ctx.available_models.as_ref() else {
        return Verdict::Skipped("`lms ls` unavailable".into());
    };

    let defaults = config.get("agents").and_then(|a| a.get("defaults"));
    let primary_id = defaults
        .and_then(|d| d.get("model"))
        .and_then(|m| m.get("primary"))
        .and_then(|p| p.as_str());
    let compaction_id = defaults
        .and_then(|d| d.get("compaction"))
        .and_then(|c| c.get("model"))
        .and_then(|m| m.as_str());

    let available_keys: Vec<&str> = catalog.iter().map(|m| m.model_key.as_str()).collect();
    let loaded_idents: Vec<&str> = ctx
        .loaded_models
        .iter()
        .map(|m| m.identifier.as_str())
        .collect();

    classify_default_model_resolves(
        primary_id,
        compaction_id,
        &available_keys,
        &loaded_idents,
    )
}

pub fn parse_size_gb(s: &str) -> Option<f64> {
    // LMStudio reports sizes like "18.45 GB" or "2.15 GB" — best-effort
    // parse, returns None on anything unexpected.
    let s = s.trim();
    let (num, unit) = s.split_once(' ')?;
    let num: f64 = num.parse().ok()?;
    match unit.to_ascii_uppercase().as_str() {
        "GB" => Some(num),
        "MB" => Some(num / 1024.0),
        "TB" => Some(num * 1024.0),
        _ => None,
    }
}

/// JSON-serializable view of the active rule set, embedded in the meta
/// row of `instruments.jsonl`. The viewer reads this on file drop to
/// drive its Anomalies panel rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesPayload {
    pub schema_version: String,
    pub rules: Vec<RuleDef>,
}

impl RulesPayload {
    pub fn current() -> Self {
        Self {
            schema_version: RULES_SCHEMA_VERSION.to_string(),
            rules: all_rules(),
        }
    }

    /// Wrap into the canonical `{rules_schema_version, rules: [...]}` JSON
    /// shape for embedding in instruments.jsonl meta payload.
    pub fn as_meta_fields(&self) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut out = serde_json::Map::new();
        out.insert(
            "rules_schema_version".to_string(),
            serde_json::Value::String(self.schema_version.clone()),
        );
        out.insert("rules".to_string(), serde_json::to_value(&self.rules)?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_semver_shaped() {
        // Trivial parse check — three dot-separated numeric segments.
        let parts: Vec<&str> = RULES_SCHEMA_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "schema version must be MAJOR.MINOR.PATCH");
        for p in parts {
            assert!(p.parse::<u32>().is_ok(), "segment `{p}` must be numeric");
        }
    }

    #[test]
    fn all_rules_have_unique_ids() {
        let rules = all_rules();
        let mut ids: Vec<String> = rules.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        let dups: Vec<&String> = ids
            .windows(2)
            .filter_map(|w| if w[0] == w[1] { Some(&w[0]) } else { None })
            .collect();
        assert!(dups.is_empty(), "duplicate rule ids: {dups:?}");
    }

    #[test]
    fn rules_payload_serializes() {
        let payload = RulesPayload::current();
        let json = serde_json::to_string(&payload).expect("serialize");
        // Smoke check: contains all rule ids
        for r in all_rules() {
            assert!(json.contains(r.id.as_str()), "missing rule id `{}` in output", r.id);
        }
    }

    #[test]
    fn safeguard_compaction_fires_on_safeguard_mode() {
        let ctx = Context {
            openclaw_config: Some(serde_json::json!({
                "agents": { "defaults": { "compaction": { "mode": "safeguard" } } }
            })),
            loaded_models: vec![],
            available_models: None,
            total_ram_gb: 0,
        };
        let verdict = eval_safeguard_compaction(&ctx);
        assert!(
            matches!(verdict, Verdict::Fire { severity: Severity::Warn, .. }),
            "expected Warn fire, got {verdict:?}"
        );
    }

    #[test]
    fn safeguard_compaction_passes_on_default_mode() {
        let ctx = Context {
            openclaw_config: Some(serde_json::json!({
                "agents": { "defaults": { "compaction": { "mode": "default" } } }
            })),
            loaded_models: vec![],
            available_models: None,
            total_ram_gb: 0,
        };
        assert!(matches!(eval_safeguard_compaction(&ctx), Verdict::Pass));
    }

    #[test]
    fn ctx_window_mismatch_fires_when_configured_diverges_from_loaded() {
        use crate::types::LoadedModel;
        let ctx = Context {
            openclaw_config: Some(serde_json::json!({
                "models": {
                    "providers": {
                        "lmstudio": {
                            "models": [
                                { "id": "qwen3-4b", "contextWindow": 200000 }
                            ]
                        }
                    }
                }
            })),
            loaded_models: vec![LoadedModel {
                identifier: "qwen3-4b".into(),
                model: "qwen3-4b".into(),
                status: "idle".into(),
                size: "2.15 GB".into(),
                context: 120000,
            }],
            available_models: None,
            total_ram_gb: 0,
        };
        match eval_ctx_window_mismatch(&ctx) {
            Verdict::Fire { severity: Severity::Fail, message } => {
                assert!(message.contains("qwen3-4b"));
                assert!(message.contains("200000"));
                assert!(message.contains("120000"));
            }
            other => panic!("expected Fail fire, got {other:?}"),
        }
    }

    #[test]
    fn strip_model_id_prefixes_handles_both_layers() {
        assert_eq!(strip_model_id_prefixes("lmstudio/darkmux:foo-bar"), "foo-bar");
        assert_eq!(strip_model_id_prefixes("darkmux:foo-bar"), "foo-bar");
        assert_eq!(strip_model_id_prefixes("lmstudio/foo-bar"), "foo-bar");
        assert_eq!(strip_model_id_prefixes("foo-bar"), "foo-bar");
        // Known non-lmstudio provider prefixes also strip.
        assert_eq!(strip_model_id_prefixes("ollama/foo-bar"), "foo-bar");
        assert_eq!(strip_model_id_prefixes("openrouter/foo-bar"), "foo-bar");
    }

    /// Regression — publisher-prefixed LMStudio model ids
    /// (`openai/gpt-oss-20b`, `google/gemma-3-4b`, …) must keep the
    /// `<publisher>/<model>` pair after stripping. The earlier
    /// `rsplit('/').next()` implementation over-stripped to the final
    /// segment, so any pin pointing at one of these models flagged as
    /// orphaned in `agents-default-model-resolves` even when the model
    /// was downloaded. Operator-observed on the M1 Max 32 GB Studio with
    /// `gpt-oss-20b` loaded.
    #[test]
    fn strip_model_id_prefixes_keeps_publisher_segment() {
        assert_eq!(
            strip_model_id_prefixes("lmstudio/openai/gpt-oss-20b"),
            "openai/gpt-oss-20b"
        );
        assert_eq!(
            strip_model_id_prefixes("lmstudio/darkmux:openai/gpt-oss-20b"),
            "openai/gpt-oss-20b"
        );
        assert_eq!(
            strip_model_id_prefixes("lmstudio/google/gemma-3-4b"),
            "google/gemma-3-4b"
        );
        assert_eq!(
            strip_model_id_prefixes("lmstudio/darkmux:google/gemma-3-4b"),
            "google/gemma-3-4b"
        );
        // Unknown provider prefix: leave alone rather than over-strip.
        // Avoids misparsing a fresh `<publisher>/<model>` id that
        // doesn't carry a provider segment.
        assert_eq!(
            strip_model_id_prefixes("openai/gpt-oss-20b"),
            "openai/gpt-oss-20b"
        );
    }

    /// End-to-end: the verdict resolves Pass when the pin points at a
    /// publisher-prefixed model that's actually in `lms ls`. This is the
    /// false-positive case the rule was previously producing on the
    /// Studio with `gpt-oss-20b` loaded.
    #[test]
    fn default_model_resolves_passes_for_publisher_prefixed_primary() {
        // Loaded set includes the primary → bare Pass (no JIT hint).
        let v = classify_default_model_resolves(
            Some("lmstudio/darkmux:openai/gpt-oss-20b"),
            None,
            &["openai/gpt-oss-20b", "google/gemma-3-4b"],
            &["darkmux:openai/gpt-oss-20b"],
        );
        assert!(matches!(v, Verdict::Pass), "got {v:?}");
    }

    #[test]
    fn default_model_resolves_passes_when_primary_in_lms_ls() {
        // Primary in catalog AND in loaded set → bare Pass.
        let v = classify_default_model_resolves(
            Some("lmstudio/foo"),
            None,
            &["foo", "bar"],
            &["foo"],
        );
        assert!(matches!(v, Verdict::Pass), "got {v:?}");
    }

    /// Issue #101 regression — pin resolves in `lms ls` but isn't in
    /// `lms ps`. Rule passes (the pin isn't orphaned) but emits a
    /// `PassWith` hint so the operator knows the first dispatch will
    /// pay a JIT-load tax.
    #[test]
    fn default_model_resolves_pass_with_jit_hint_when_downloaded_only() {
        let v = classify_default_model_resolves(
            Some("lmstudio/darkmux:openai/gpt-oss-20b"),
            None,
            &["openai/gpt-oss-20b", "google/gemma-3-4b"],
            &[], // nothing loaded
        );
        match v {
            Verdict::PassWith(message) => {
                assert!(message.contains("downloaded but not loaded"));
                assert!(message.contains("JIT-load"));
                assert!(message.contains("primary"));
                assert!(message.contains("openai/gpt-oss-20b"));
            }
            other => panic!("expected PassWith, got {other:?}"),
        }
    }

    /// Same JIT-load hint path but on the compaction pin specifically —
    /// covers the case where the primary is loaded but the compactor
    /// hasn't been pulled into `lms ps` yet (e.g. operator loaded
    /// primary manually without `darkmux swap`).
    #[test]
    fn default_model_resolves_pass_with_jit_hint_on_compaction_only() {
        let v = classify_default_model_resolves(
            Some("lmstudio/darkmux:openai/gpt-oss-20b"),
            Some("lmstudio/darkmux:google/gemma-3-4b"),
            &["openai/gpt-oss-20b", "google/gemma-3-4b"],
            &["darkmux:openai/gpt-oss-20b"], // primary loaded; compactor not
        );
        match v {
            Verdict::PassWith(message) => {
                assert!(message.contains("compaction"));
                assert!(message.contains("gemma-3-4b"));
                assert!(
                    !message.contains("primary"),
                    "primary is loaded so it shouldn't appear in the hint: {message}"
                );
            }
            other => panic!("expected PassWith, got {other:?}"),
        }
    }

    #[test]
    fn default_model_resolves_fails_when_primary_orphaned() {
        let v = classify_default_model_resolves(
            Some("lmstudio/darkmux:qwen3.6-35b-a3b-turboquant-mlx"),
            None,
            &["qwen3.6-35b-a3b", "qwen3.6-35b-a3b-oq6"],
            &[],
        );
        match v {
            Verdict::Fire { severity: Severity::Fail, message } => {
                assert!(message.contains("primary"));
                assert!(message.contains("turboquant"));
                // Surfaces top suggestions from lms ls.
                assert!(message.contains("qwen3.6-35b-a3b"));
            }
            other => panic!("expected Fail fire, got {other:?}"),
        }
    }

    #[test]
    fn default_model_resolves_fires_for_compaction_orphan_only() {
        let v = classify_default_model_resolves(
            Some("foo"),
            Some("missing-compactor"),
            &["foo", "bar"],
            &["foo"],
        );
        match v {
            Verdict::Fire { severity: Severity::Fail, message } => {
                assert!(message.contains("compaction"));
                assert!(message.contains("missing-compactor"));
                assert!(!message.contains("primary `"), "primary should not fire: {message}");
            }
            other => panic!("expected Fail fire, got {other:?}"),
        }
    }

    #[test]
    fn default_model_resolves_lists_both_orphans_when_both_missing() {
        let v = classify_default_model_resolves(
            Some("missing-primary"),
            Some("missing-compactor"),
            &["foo"],
            &[],
        );
        match v {
            Verdict::Fire { severity: Severity::Fail, message } => {
                assert!(message.contains("2 pin(s)"));
                assert!(message.contains("missing-primary"));
                assert!(message.contains("missing-compactor"));
            }
            other => panic!("expected Fail fire, got {other:?}"),
        }
    }

    #[test]
    fn default_model_resolves_skipped_when_no_primary_set() {
        let v = classify_default_model_resolves(None, None, &["foo"], &[]);
        assert!(matches!(v, Verdict::Skipped(_)), "got {v:?}");
    }

    #[test]
    fn default_model_resolves_skipped_when_no_openclaw_config() {
        let ctx = Context {
            openclaw_config: None,
            loaded_models: vec![],
            available_models: Some(Vec::new()),
            total_ram_gb: 0,
        };
        assert!(matches!(
            eval_agents_default_model_resolves(&ctx),
            Verdict::Skipped(_)
        ));
    }

    #[test]
    fn default_model_resolves_skipped_when_lms_ls_unavailable() {
        let ctx = Context {
            openclaw_config: Some(serde_json::json!({
                "agents": { "defaults": { "model": { "primary": "foo" } } }
            })),
            loaded_models: vec![],
            available_models: None,
            total_ram_gb: 0,
        };
        assert!(matches!(
            eval_agents_default_model_resolves(&ctx),
            Verdict::Skipped(_)
        ));
    }
}
