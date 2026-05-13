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
pub const RULES_SCHEMA_VERSION: &str = "1.0.0";

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
            "ctx-window-mismatch",
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
}
