//! (#1198) `scores.json` — the bench suite's persisted score artifact (#1197).
//!
//! Two score families ride one substrate, distinguished by what their key
//! means: **capability** rows attach to the ARTIFACT `(model, quant, backend,
//! n_ctx)` and are machine-portable (bench once on the biggest machine, reuse
//! fleet-wide); **fit** rows attach to the PAIRING (artifact + machine
//! fingerprint) and are tier-relative by nature. Wall-clock is never stored
//! as a score — its two components are: `tokens_to_solution` (capability) and
//! `seconds_per_token` (fit); a consumer derives wall-clock per machine by
//! multiplying.
//!
//! Design decisions absorbed at birth (see #1197's discussion trail):
//! - **Outcome is three-class**: `pass` / `capability_fail` / `infra_fail`.
//!   A watchdog kill, timeout, or load failure is a RERUN, never a zero on
//!   the model's record — the #1113 lesson (a SIGKILLed dispatch scored
//!   identically to a clean-but-wrong one).
//! - **The machine fingerprint is rich**, not a hostname: "constant hardware"
//!   is what makes fit rows meaningful, and the constant includes the serving
//!   stack (an engine update shifts perf while `machine_id` stays the same).
//! - **`source` marks provenance of the benchmark itself**: `native` for
//!   darkmux-lab benches, or an external harness id (`bfcl`, `ruler`,
//!   `ifeval`, `aider`, …) whose results are ingested as rows —
//!   adopt-don't-rebuild is structural, not aspirational.
//! - **pass^k is native**: every row carries `(trial, k)`; aggregation is the
//!   reader's job (per-trial rows never collapse at write time).
//! - **Loaded-state provenance**: what `lms ps` reported before/after the
//!   run, so a silent mid-run reload at a different context (#1135 class)
//!   is visible in the artifact instead of corrupting the score.
//!
//! Schema versioning follows the repo's minor-bump + lenient-read discipline:
//! all-`Option` where honest, additive fields are minor bumps, readers
//! tolerate unknown fields.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bump per the data-shape semver discipline (additive field = minor).
pub const SCORES_SCHEMA_VERSION: &str = "1.0.0";

/// Which score family a row belongs to — the load-bearing split (#1197).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreFamily {
    /// Property of the artifact; machine-portable.
    Capability,
    /// Property of the (artifact × machine) pairing; tier-relative.
    Fit,
}

/// Three-class outcome (#1113): infra failures are reruns, never zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    /// The dispatch ran; the model failed the task/contract.
    CapabilityFail,
    /// The harness failed the model: watchdog kill, timeout, load failure,
    /// endpoint unreachable. Excluded from capability aggregation.
    InfraFail,
}

/// What capability scores attach to. `backend` is part of the key — an MLX
/// quant and a GGUF quant of the "same" model are different artifacts, and
/// a remote endpoint is a backend of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArtifactKey {
    /// Model identifier as dispatched (e.g. `qwen3.6-35b-a3b-turboquant-mlx`).
    pub model: String,
    /// Quantization label when known (often embedded in the model id; kept
    /// separate when resolvable so the key is queryable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// Serving backend: `lmstudio-mlx`, `lmstudio-gguf`, `azure-openai`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// The context length the model was (declared) loaded at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u64>,
}

/// Rich machine identity for fit rows + run provenance. Everything
/// best-effort `Option` — a partial fingerprint is still a fingerprint,
/// and lenient-read means older writers never brick newer readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MachineFingerprint {
    /// Operator-facing machine name (`DARKMUX_MACHINE_ID` semantics).
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ram_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// Serving engine + version — the part of "constant hardware" that
    /// changes under your feet (an LMStudio/MLX update shifts perf while
    /// the chip stays the same).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
}

impl MachineFingerprint {
    /// Best-effort detection: hardware via `darkmux-hardware`, OS version via
    /// `sw_vers` (macOS), engine version via `lms version`. Every probe that
    /// fails leaves `None` rather than erroring — a bench must never die on
    /// fingerprinting.
    pub fn detect(machine_id: &str) -> Self {
        let hw = darkmux_hardware::detect();
        let os_version = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        let engine_version = std::process::Command::new("lms")
            .arg("version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            machine_id: machine_id.to_string(),
            machine_uid: darkmux_hardware::machine_uid().map(str::to_string),
            platform: Some(hw.platform.label().to_string()),
            arch: Some(hw.arch),
            total_ram_gb: Some(hw.total_ram_gb),
            physical_cores: Some(hw.physical_cores),
            os_version,
            engine: engine_version.is_some().then(|| "lmstudio".to_string()),
            engine_version,
        }
    }
}

/// Per-run provenance shared by every row in a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunProvenance {
    /// The bench run's identifier (a lab run id, or a bench-generated one).
    pub run_id: String,
    /// RFC3339 timestamp, caller-supplied.
    pub ts: String,
    pub machine: MachineFingerprint,
    /// The profile the dispatch named, when threaded (part of reproducing
    /// the run — #1199 makes this explicit on `lab run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Remote dispatches: the endpoint label + what the endpoint SAID served
    /// the request (a deployment named gpt-4o may serve gpt-5.1 — #1191).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    /// `lms ps`-derived loaded state (model, context) captured before/after
    /// the run; `loaded_drift` flags a mismatch (#1135 class). Free-form
    /// JSON — the shape follows `lms ps` rather than a schema of our own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_before: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_after: Option<serde_json::Value>,
    #[serde(default)]
    pub loaded_drift: bool,
}

/// One scored trial of one bench cell. Aggregation (pass^k, means) is the
/// READER's job — per-trial rows never collapse at write time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreRow {
    /// Bench identifier (`review-bench`, `tool-bench`, …) or the external
    /// harness a row was ingested from.
    pub bench: String,
    /// The bench's own version — scores are only comparable within it.
    pub bench_version: String,
    /// `native` for darkmux-lab benches; an external harness id (`bfcl`,
    /// `ruler`, `ifeval`, `aider`, …) for ingested rows.
    pub source: String,
    pub family: ScoreFamily,
    /// The measured axis (`recall`, `precision`, `case`, `chaining@3`, …).
    pub axis: String,
    pub artifact: ArtifactKey,
    pub outcome: Outcome,
    /// Numeric value when the axis is numeric (rates, counts); `None` for
    /// pure pass/fail axes (the outcome carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Trial index within the cell (0-based) + the cell's planned trial
    /// count — the pass^k substrate.
    pub trial: u32,
    pub k: u32,
    /// Capability-side time component (the model's cost in its own currency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_solution: Option<u64>,
    /// Fit-side time component (the machine's cost). Wall-clock = product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_per_token: Option<f64>,
    /// Budgets are denominated in tokens/turns (machine-independent);
    /// recorded so a budget-limited row is interpretable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    /// Bench-specific payload (per-case detail, sampling params, seeds).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

/// The per-run document: one `scores.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoresDoc {
    pub schema_version: String,
    pub provenance: RunProvenance,
    pub rows: Vec<ScoreRow>,
    /// Forward-compat overflow (lenient read).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl ScoresDoc {
    pub fn new(provenance: RunProvenance, rows: Vec<ScoreRow>) -> Self {
        Self {
            schema_version: SCORES_SCHEMA_VERSION.to_string(),
            provenance,
            rows,
            extras: serde_json::Map::new(),
        }
    }
}

/// Write the document atomically (temp + rename, mirroring the fixture
/// registry's #543 pattern): a crash leaves either the old file or the new
/// one, never a torn write.
pub fn write_scores(path: &Path, doc: &ScoresDoc) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(doc).context("serializing scores.json")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} into place", path.display()))?;
    Ok(())
}

/// Read a document (lenient: unknown fields tolerated by construction).
pub fn read_scores(path: &Path) -> Result<ScoresDoc> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> ScoreRow {
        ScoreRow {
            bench: "review-bench".into(),
            bench_version: "1".into(),
            source: "native".into(),
            family: ScoreFamily::Capability,
            axis: "recall".into(),
            artifact: ArtifactKey {
                model: "test-model".into(),
                quant: None,
                backend: Some("lmstudio-mlx".into()),
                n_ctx: Some(32768),
            },
            outcome: Outcome::Pass,
            value: Some(0.67),
            trial: 0,
            k: 1,
            tokens_to_solution: Some(23662),
            seconds_per_token: None,
            budget_turns: None,
            budget_tokens: None,
            detail: serde_json::Value::Null,
        }
    }

    #[test]
    fn roundtrip_preserves_document() {
        let doc = ScoresDoc::new(
            RunProvenance {
                run_id: "rb-123".into(),
                ts: "2026-07-05T12:00:00Z".into(),
                machine: MachineFingerprint {
                    machine_id: "laptop".into(),
                    total_ram_gb: Some(128),
                    ..Default::default()
                },
                ..Default::default()
            },
            vec![sample_row()],
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        write_scores(&path, &doc).unwrap();
        let back = read_scores(&path).unwrap();
        assert_eq!(back.schema_version, SCORES_SCHEMA_VERSION);
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0].axis, "recall");
        assert_eq!(back.rows[0].outcome, Outcome::Pass);
        assert_eq!(back.rows[0].artifact.n_ctx, Some(32768));
        assert_eq!(back.provenance.machine.total_ram_gb, Some(128));
    }

    #[test]
    fn lenient_read_tolerates_unknown_fields() {
        // A NEWER writer's field must not brick this reader (minor-bump +
        // lenient-read discipline).
        let json = r#"{
            "schema_version": "1.5.0",
            "future_top_level": {"x": 1},
            "provenance": {"run_id": "r", "ts": "t", "machine": {"machine_id": "m"}},
            "rows": []
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        std::fs::write(&path, json).unwrap();
        let doc = read_scores(&path).unwrap();
        assert_eq!(doc.schema_version, "1.5.0");
        assert!(doc.extras.contains_key("future_top_level"));
    }

    #[test]
    fn outcome_serializes_snake_case() {
        // The enum encoding is a wire contract — pin it.
        assert_eq!(
            serde_json::to_string(&Outcome::InfraFail).unwrap(),
            "\"infra_fail\""
        );
        assert_eq!(
            serde_json::to_string(&ScoreFamily::Capability).unwrap(),
            "\"capability\""
        );
    }

    #[test]
    fn atomic_write_replaces_not_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scores.json");
        let doc1 = ScoresDoc::new(RunProvenance::default(), vec![sample_row()]);
        write_scores(&path, &doc1).unwrap();
        let mut doc2 = doc1.clone();
        doc2.rows.clear();
        write_scores(&path, &doc2).unwrap();
        let back = read_scores(&path).unwrap();
        assert!(back.rows.is_empty(), "second write replaces the first");
        assert!(!path.with_extension("json.tmp").exists(), "no temp litter");
    }

    #[test]
    fn fingerprint_detect_is_best_effort_never_panics() {
        let fp = MachineFingerprint::detect("test-machine");
        assert_eq!(fp.machine_id, "test-machine");
        // Hardware fields come from darkmux_hardware::detect() which always
        // returns; the shell-out fields may be None — both are valid.
        assert!(fp.total_ram_gb.is_some());
    }
}
