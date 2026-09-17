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
///
/// Major-bumped again to 3.0.0 (simplification batch): the `AggressiveSampler`
/// variant was a permanent stub — `evaluate_one` always returned
/// `Verdict::Skipped("not yet implemented")` for it, with no evaluator ever
/// implemented, so it carried zero operator value while still occupying a
/// `RuleKind` discriminant. Removing a variant changes the `RuleKind` enum
/// encoding, which the schema table calls out explicitly as a major-bump
/// case (see `CLAUDE.md`'s "Versioning — rules schema" section) — not a
/// minor bump, since an old consumer keying on this discriminant would
/// break, not just see one fewer rule to safely ignore.
pub const RULES_SCHEMA_VERSION: &str = "3.0.0";

/// Stable identifiers for the active rule set. Add new ones to the bottom.
/// The `kind` field on `RuleDef` carries this discriminant in the wire
/// format, so renames are a major version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleKind {
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
    /// Loaded-model snapshot from `lms ps`. `None` if the call failed —
    /// the SAME convention `available_models` below already uses, and the
    /// distinction a rule needs to avoid claiming something it cannot see
    /// (#2774 round-9 review C3). An empty `Some` means the host really
    /// reported zero residents.
    pub loaded_models: Option<Vec<darkmux_types::LoadedModel>>,
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
        // (#2774 round-9 review C3) `.ok()`, not `.unwrap_or_default()`:
        // since #2774 round-9 MF3 a failed listing is distinguishable from
        // an empty one, and collapsing them here made the rule pass print
        // `skipped: no models loaded` under a FAILING `lms` — a reason the
        // code could not back, stated to the operator as fact.
        let loaded_models = darkmux_profiles::lms::list_loaded().ok();
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
        RuleKind::MemoryHeadroomTight => eval_memory_headroom(ctx),
    }
}

// ─── Rule evaluators ───────────────────────────────────────────────────

fn eval_memory_headroom(ctx: &Context) -> Verdict {
    if ctx.total_ram_gb == 0 {
        return Verdict::Skipped("ram total unavailable".into());
    }
    let Some(loaded_models) = ctx.loaded_models.as_deref() else {
        return Verdict::Skipped("could not read the loaded-model list".into());
    };
    if loaded_models.is_empty() {
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
    for m in loaded_models {
        match darkmux_types::size::parse_size_gb(&m.size) {
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
    fn memory_headroom_skips_when_a_model_size_is_unparseable() {
        use darkmux_types::LoadedModel;
        // (#904) An unparseable size must Skip (so the operator sees it),
        // NOT silently contribute 0 and Pass on a system that might be tight.
        let ctx = Context {
            loaded_models: Some(vec![LoadedModel {
                identifier: "weird-model".into(),
                model: "weird-model".into(),
                status: "idle".into(),
                size: "18,45 GB".into(), // localized comma → unparseable
                context: 32_768,
            }]),
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

    // ─── Headroom arithmetic (#2724) ───────────────────────────────────
    //
    // The estimate below is what `darkmux doctor` tells the operator about
    // whether the currently-loaded models still fit. Every term in it used
    // to be unpinned: `/`, `*`, `+` and `+=` in `eval_memory_headroom` were
    // each swappable with no test noticing, so a wrong operator there would
    // have shipped as a confident, wrong "you're fine" (or a confident,
    // wrong warning). The fixtures here sit ON the 80%-of-unified-memory
    // decision boundary rather than at absurd numbers, because a fixture far
    // from the boundary keeps returning the same verdict under a swapped
    // operator and therefore proves nothing.
    //
    // The formula under test: per loaded model, `size_gb + 0.5 * ctx /
    // 32_768`; summed across models; fired when the sum exceeds 80% of
    // unified memory. `0.5 * ctx / 32_768` means "half a GB of KV per 32K of
    // configured context", so 128K of context costs 2 GB and 256K costs 4.

    fn loaded(identifier: &str, size: &str, context: u64) -> darkmux_types::LoadedModel {
        darkmux_types::LoadedModel {
            identifier: identifier.into(),
            model: identifier.into(),
            status: "idle".into(),
            size: size.into(),
            context,
        }
    }

    /// (#2774 round-9 review C3) "I could not read the list" is not "the
    /// list was empty", and the operator is told which.
    ///
    /// Measured before the fix, under a failing `lms`: `doctor -v` printed
    /// `✓ eureka: memory-headroom-tight (skipped: no models loaded)` — the
    /// verdict was harmless (Skipped either way), the stated REASON was a
    /// claim the code could not back.
    #[test]
    fn an_unreadable_model_list_skips_for_a_reason_it_can_actually_back() {
        let unknown = Context {
            loaded_models: None,
            available_models: None,
            total_ram_gb: 128,
        };
        match eval_memory_headroom(&unknown) {
            Verdict::Skipped(reason) => {
                assert!(
                    !reason.contains("no models loaded"),
                    "a failed listing must not be reported as an empty one: {reason}"
                );
                assert!(reason.contains("could not read"), "{reason}");
            }
            other => panic!("expected Skipped, got {other:?}"),
        }

        // …and a host that genuinely has nothing loaded keeps its own,
        // different reason, so the guard above cannot be satisfied by
        // renaming every skip.
        match eval_memory_headroom(&ctx_with(128, vec![])) {
            Verdict::Skipped(reason) => assert_eq!(reason, "no models loaded"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    fn ctx_with(total_ram_gb: u32, loaded_models: Vec<darkmux_types::LoadedModel>) -> Context {
        Context {
            loaded_models: Some(loaded_models),
            available_models: None,
            total_ram_gb,
        }
    }

    /// Assert Fire and return the rendered message, so the percentage the
    /// operator actually reads is pinned too — not just the branch taken.
    fn expect_fire(v: Verdict) -> String {
        match v {
            Verdict::Fire { severity, message } => {
                assert_eq!(severity, Severity::Warn, "headroom fires as a warning");
                message
            }
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    #[test]
    fn memory_headroom_fires_just_over_the_threshold() {
        // 24 GB of weights + 128K of context (2 GB of KV) = 26 GB of a
        // 32 GB machine — 81.25%, just past the 80% line. An operator with
        // this loaded is told their next heavy dispatch may OOM.
        //
        // Every arithmetic mutation that makes the estimate SMALLER lands
        // this fixture back under 80% and silently reports all-clear:
        // dropping the `0.5 *` (kv -> ~0), turning the KV `/` into `%`
        // (65536 % 32768 = 0), turning `size + kv` into `size - kv` (22 GB),
        // turning the `+=` accumulator into `-=` or `*=` (a negative or zero
        // total), dividing instead of multiplying by 100, or flipping `>`
        // to `<`/`==`.
        let ctx = ctx_with(32, vec![loaded("primary", "24 GB", 131_072)]);
        let msg = expect_fire(eval_memory_headroom(&ctx));
        assert!(msg.contains("~26.0 GB"), "estimate in message: {msg}");
        assert!(msg.contains("32 GB unified"), "budget in message: {msg}");
        assert!(msg.contains("(81%)"), "percentage in message: {msg}");
    }

    #[test]
    fn memory_headroom_passes_just_under_the_threshold_even_at_long_context() {
        // 21 GB of weights + 256K of context (4 GB of KV) = 25 GB of a
        // 32 GB machine — 78.1%, just under the line. Nothing is said.
        //
        // This is the fixture that catches over-estimation, which fails the
        // other way: a spurious "you may OOM" on a machine that is fine.
        // The tightest such mutation is `0.5 * ctx` becoming `0.5 + ctx`,
        // which roughly DOUBLES the KV term (4 GB -> 8 GB) and pushes this
        // to 90%. The long context is deliberate: at 32K the doubling is
        // 0.5 GB and would not cross any boundary, so a short-context
        // fixture here would prove nothing.
        let ctx = ctx_with(32, vec![loaded("primary", "21 GB", 262_144)]);
        match eval_memory_headroom(&ctx) {
            Verdict::Pass => {}
            other => panic!("expected Pass at 78% of unified memory, got {other:?}"),
        }
    }

    #[test]
    fn memory_headroom_does_not_fire_exactly_at_the_threshold() {
        // 30 GB of weights + 128K of context (2 GB of KV) = exactly 32 GB
        // of a 40 GB machine — exactly 80.0%. The rule fires ABOVE 80%, not
        // AT it, so this operator is not warned. Chosen so the percentage is
        // exactly representable in f64: (32.0 / 40.0) * 100.0 == 80.0.
        //
        // This is the only fixture that separates `>` from `>=`, and the
        // difference is a real one — at `>=` the warning fires on a machine
        // sitting precisely on its stated budget.
        let ctx = ctx_with(40, vec![loaded("primary", "30 GB", 131_072)]);
        match eval_memory_headroom(&ctx) {
            Verdict::Pass => {}
            other => panic!("expected Pass at exactly 80.0%, got {other:?}"),
        }
    }

    #[test]
    fn memory_headroom_sums_across_loaded_models() {
        // Two models, each comfortably fine alone (14 GB = 44%, 13 GB =
        // 41% of 32 GB), together 27 GB = 84%. The warning is about the
        // WORKING SET, so it has to accumulate across everything resident —
        // an accumulator that subtracted, or multiplied into a 0.0 seed,
        // would report each machine-filling pair as healthy.
        let both = ctx_with(
            32,
            vec![
                loaded("primary", "12 GB", 131_072),
                loaded("compactor", "11 GB", 131_072),
            ],
        );
        let msg = expect_fire(eval_memory_headroom(&both));
        assert!(msg.contains("~27.0 GB"), "summed estimate: {msg}");

        for one in [
            loaded("primary", "12 GB", 131_072),
            loaded("compactor", "11 GB", 131_072),
        ] {
            let id = one.identifier.clone();
            match eval_memory_headroom(&ctx_with(32, vec![one])) {
                Verdict::Pass => {}
                other => panic!("expected Pass for `{id}` alone, got {other:?}"),
            }
        }
    }

    // ─── Rule-table plumbing (#2724) ───────────────────────────────────

    #[test]
    fn all_rules_contains_the_memory_headroom_rule() {
        // `all_rules_have_unique_ids` and `rules_payload_serializes` both
        // pass vacuously on an empty table, so neither notices a rule set
        // that has silently gone empty — which an operator experiences as
        // `darkmux doctor` quietly omitting the check rather than reporting
        // anything wrong.
        let rules = all_rules();
        assert!(!rules.is_empty(), "the rule table must not be empty");
        let r = rules
            .iter()
            .find(|r| r.id == "memory-headroom-tight")
            .expect("memory-headroom-tight must be in the rule table");
        assert_eq!(r.kind, RuleKind::MemoryHeadroomTight);
        assert_eq!(r.category, "resource_pressure");
        assert_eq!(r.severity, Severity::Warn);
        assert!(!r.name.is_empty(), "rules render a display label");
        assert!(!r.fix_hint.is_empty(), "a firing rule points at the fix");
    }

    #[test]
    fn evaluate_all_returns_a_verdict_for_every_rule() {
        // Same vacuity, one layer up: an empty verdict list is how doctor
        // shows no eureka findings at all, which is indistinguishable from
        // "all clear" on the surface the operator reads.
        let ctx = ctx_with(32, vec![loaded("primary", "24 GB", 131_072)]);
        let verdicts = evaluate_all(&ctx);
        let defs = all_rules();
        assert_eq!(
            verdicts.len(),
            defs.len(),
            "one verdict per rule, in rule order"
        );
        let ids: Vec<&str> = verdicts.iter().map(|(d, _)| d.id.as_str()).collect();
        let expected: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, expected);

        let (_, verdict) = verdicts
            .iter()
            .find(|(d, _)| d.kind == RuleKind::MemoryHeadroomTight)
            .expect("headroom rule evaluated");
        // Same context as the fires-just-over fixture, so the dispatcher is
        // pinned to the real evaluator rather than to a constant.
        expect_fire(verdict.clone());
    }

    #[test]
    fn as_meta_fields_carries_the_version_and_the_rules() {
        // This map IS the rules `meta` payload a consumer reads to learn
        // which rules exist and whether it can trust their shape. An empty
        // map reads as "no rules, no schema version" — not as an error.
        let fields = RulesPayload::current()
            .as_meta_fields()
            .expect("meta fields serialize");
        assert_eq!(
            fields.get("rules_schema_version").and_then(|v| v.as_str()),
            Some(RULES_SCHEMA_VERSION),
        );
        let rules = fields
            .get("rules")
            .and_then(|v| v.as_array())
            .expect("`rules` must be an array");
        assert_eq!(rules.len(), all_rules().len());
        let ids: Vec<&str> = rules
            .iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_str()))
            .collect();
        assert!(
            ids.contains(&"memory-headroom-tight"),
            "rule ids in payload: {ids:?}"
        );
    }
}
