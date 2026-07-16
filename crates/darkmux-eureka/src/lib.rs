//! Auto-Eureka detection — surface known-misconfiguration patterns before
//! the operator hits them.
//!
//! Each rule corresponds to a class of bug an early-adopter operator has
//! actually hit, where surfacing it earlier (at `darkmux doctor` or in the
//! viewer's Anomalies panel) would have saved hours of debugging.
//!
//! (2.0, #1405): the rule set previously included several rules that
//! diagnosed the legacy `openclaw` shell-out runtime's config file
//! (`~/.openclaw/openclaw.json`) — context-window mismatch, safeguard
//! compaction mode, n_ctx-exceeds-model-max, compactor-not-loaded,
//! primary-config drift, and default-model-pin resolution. All six were
//! removed along with the openclaw runtime itself; darkmux's own
//! profile/pin config is validated by other doctor checks
//! (`check_profile_registry`, `check_role_model_pin_drift`'s successors,
//! etc.), not by this rules engine.
//!
//! # Schema versioning
//!
//! `RULES_SCHEMA_VERSION` is semver and ships in the rules `meta` payload
//! the viewer would consume to detect compatibility. (The standalone
//! `instruments.jsonl` sidecar that used to carry the RuleDefs to the viewer
//! was retired in #557; emitting them onto the flow telemetry stream + the
//! viewer Anomalies panel that reads them is tracked separately as #657 — not
//! yet built. `darkmux doctor` is the live surface for these rules today.)
//! Bump rules:
//!
//! - **patch** — bug fix, message tweak, threshold adjustment that doesn't
//!   change semantics. Old viewers parse unchanged.
//! - **minor** — new rule kind or new optional field. Old viewers can
//!   safely ignore unknown rules (additive change).
//! - **major** — rename/retype a field, change `kind` enum, new required
//!   field. A future consumer must NOT trust the data; when the viewer
//!   consumer lands (#657 transport + #12 viewer validation) its version
//!   gate moves in the same PR. There is no such viewer gate today.
//!
//! See `CLAUDE.md` for the full contract.
//!
//! # DRY architecture
//!
//! Rule **metadata** (id, name, kind, message_template, fix_hint) lives
//! here as the single source of truth. The intended split (#657): the CLI
//! emits the metadata in the rules `meta` payload; the viewer renders
//! findings using that metadata, not a duplicated JS copy. (The viewer side
//! isn't built yet — see #657.)
//!
//! Rule **evaluation** is per-side: Rust evaluators here read `lms ps` (the
//! live path, via `darkmux doctor`); the viewer's JS would evaluate the
//! live-applicable subset against the telemetry it reads from the flow
//! stream (#657). Different input data shapes — same logical rules.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Semver of the rules schema. See module docs for bump rules.
///
/// Engine-internal + surfaced by `darkmux doctor` today; there is no
/// viewer-side `EXPECTED_RULES_SCHEMA_MAJOR` gate yet (gated on #657 transport
/// and #12 viewer validation). When the viewer consumer lands, its version
/// gate moves in the same PR.
///
/// Major-bumped to 2.0.0 in #1405: six `RuleKind` variants that diagnosed
/// the removed openclaw shell-out runtime's config file were deleted.
pub const RULES_SCHEMA_VERSION: &str = "2.0.0";

/// Stable identifiers for the active rule set. Add new ones to the bottom.
/// The `kind` field on `RuleDef` carries this discriminant in the wire
/// format, so renames are a major version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleKind {
    /// (Reserved — not evaluated in this version. See follow-up issue.)
    AggressiveSampler,
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

/// Definition of a single rule. Cloned into the rules `meta` payload the
/// viewer consumes, so it renders findings with the same labels/messages
/// without duplicating string literals.
///
/// String fields are owned (`String`) rather than `&'static str` so the
/// type round-trips cleanly through serde (the viewer never deserializes
/// this in Rust; the round-trip is for test parity and future tooling).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    /// Stable, human-readable id (e.g. `memory-headroom-tight`).
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
    /// Loaded-model snapshot from `lms ps`.
    pub loaded_models: Vec<darkmux_types::LoadedModel>,
    /// Downloaded-model catalog from `lms ls`, used for arch-max lookups.
    /// `None` if the call failed.
    pub available_models: Option<Vec<darkmux_profiles::lms::ModelMeta>>,
    /// System RAM in GB (unified memory total for Apple Silicon).
    pub total_ram_gb: u32,
}

impl Context {
    /// Build a context by reading the current system state. Best-effort —
    /// failures populate `None` fields rather than erroring out, so the
    /// rule pass can degrade gracefully.
    pub fn collect() -> Self {
        let loaded_models = darkmux_profiles::lms::list_loaded().unwrap_or_default();
        let available_models = darkmux_profiles::lms::list_available().ok();
        let total_ram_gb = darkmux_hardware::detect().total_ram_gb;
        Self {
            loaded_models,
            available_models,
            total_ram_gb,
        }
    }
}

/// Verdict for one rule against the current context.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Rule didn't fire — no misconfiguration matching this pattern.
    Pass,
    /// Pass with an informational message — useful for diagnostics that
    /// aren't a misconfiguration but the operator still benefits from
    /// seeing.
    ///
    /// Rust-internal today. When verdicts start shipping on the flow
    /// telemetry stream (#657), this variant becomes part of the wire format
    /// and the `RULES_SCHEMA_VERSION` contract gates that bump.
    #[allow(dead_code)]
    PassWith(String),
    /// Rule fired. Message describes the specific finding; severity comes
    /// from the rule def by default but may be downgraded at runtime.
    Fire {
        severity: Severity,
        message: String,
    },
    /// Rule was skipped (e.g. required input unavailable). Includes a
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
        RuleKind::AggressiveSampler => Verdict::Skipped("not yet implemented".into()),
        RuleKind::MemoryHeadroomTight => eval_memory_headroom(ctx),
    }
}

// ─── Rule evaluators ───────────────────────────────────────────────────

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
    let mut unparseable: Vec<String> = Vec::new();
    for m in &ctx.loaded_models {
        match parse_size_gb(&m.size) {
            Some(size_gb) => {
                let kv_gb = 0.5 * (m.context as f64) / 32_768.0;
                estimated_gb += size_gb + kv_gb;
            }
            // (#904) A model whose size we can't parse would silently
            // contribute 0, undercounting the working set and making this
            // warning under-fire in the dangerous direction (a tight system
            // reads as fine). Surface it as Skipped instead of a wrong Pass.
            None => unparseable.push(format!("{} ({})", m.identifier, m.size)),
        }
    }
    if !unparseable.is_empty() {
        return Verdict::Skipped(format!(
            "couldn't parse loaded-model size(s): {} — headroom estimate would be \
             wrong-low, so not firing",
            unparseable.join(", ")
        ));
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
    // LMStudio reports sizes like "18.45 GB", "2.15 GB", or "18.45 GiB".
    // (#904) Tolerate the binary (`GiB`/`MiB`/`TiB`) suffixes and a missing
    // space (`18.45GB`) — the old `split_once(' ')` + GB/MB/TB-only match
    // dropped all of those to `None`, which the caller then summed as 0,
    // undercounting the working set so the headroom warning under-fired. For
    // a headroom *estimate* the binary-vs-decimal gap is within the noise, so
    // the `i`-variants map to the same magnitude. Returns `None` on anything
    // genuinely unparseable (a localized comma, no unit) — the caller now
    // surfaces that as Skipped rather than a silent 0.
    let s = s.trim();
    // Split the leading number from the trailing unit, with or without a space.
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num_str, unit_raw) = s.split_at(split);
    let num: f64 = num_str.trim().parse().ok()?;
    match unit_raw.trim().to_ascii_uppercase().as_str() {
        "GB" | "GIB" => Some(num),
        "MB" | "MIB" => Some(num / 1024.0),
        "TB" | "TIB" => Some(num * 1024.0),
        _ => None,
    }
}

/// JSON-serializable view of the active rule set, built to ride the rules
/// `meta` payload a viewer would consume to drive an Anomalies panel.
/// **Not emitted yet** — the stream transport + viewer panel are #657
/// (the `instruments.jsonl` sidecar that used to carry this was retired in
/// #557).
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
    /// shape for embedding in the rules `meta` payload.
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
    fn parse_size_gb_tolerates_gib_binary_and_missing_space() {
        // (#904) The old parser dropped all of these to None → summed as 0.
        assert_eq!(parse_size_gb("18.45 GB"), Some(18.45));
        assert_eq!(parse_size_gb("18.45GB"), Some(18.45)); // no space
        assert_eq!(parse_size_gb("18.45 GiB"), Some(18.45)); // binary suffix
        assert_eq!(parse_size_gb("512 MB"), Some(0.5));
        assert_eq!(parse_size_gb("512MiB"), Some(0.5));
        assert_eq!(parse_size_gb("2 TB"), Some(2048.0));
        assert_eq!(parse_size_gb("2TiB"), Some(2048.0));
        // Genuinely unparseable → None (caller surfaces it as Skipped, not 0).
        assert_eq!(parse_size_gb("18,45 GB"), None); // localized comma
        assert_eq!(parse_size_gb("nonsense"), None);
        assert_eq!(parse_size_gb("18.45"), None); // no unit
    }

    #[test]
    fn memory_headroom_skips_when_a_model_size_is_unparseable() {
        use darkmux_types::LoadedModel;
        // (#904) An unparseable size must Skip (so the operator sees it),
        // NOT silently contribute 0 and Pass on a system that might be tight.
        let ctx = Context {
            loaded_models: vec![LoadedModel {
                identifier: "weird-model".into(),
                model: "weird-model".into(),
                status: "idle".into(),
                size: "18,45 GB".into(), // localized comma → unparseable
                context: 32_768,
            }],
            available_models: None,
            total_ram_gb: 32,
        };
        match eval_memory_headroom(&ctx) {
            Verdict::Skipped(msg) => {
                assert!(msg.contains("weird-model"), "got: {msg}");
                assert!(msg.contains("couldn't parse"), "got: {msg}");
            }
            other => panic!("expected Skipped on unparseable size, got {other:?}"),
        }
    }
}
