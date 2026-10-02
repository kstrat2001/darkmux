//! A lab run's `manifest.json`, typed (#3035).
//!
//! Three providers write it and three later steps enrich it (the fixture
//! provenance, the write-the-tests work gate, an escalation), so its shape
//! used to live in each writer's `json!` literal and every reader's string
//! keys. This is the one definition.
//!
//! It reads leniently: every field is optional (a manifest predates each field
//! that was added to it), a key this type does not name lands in `extras` and
//! re-serializes flat, and a field a version wrote as an explicit `null`
//! (`verify`, `final_hash`) stays distinguishable from one it never wrote,
//! because `null` there means "checked, nothing to report" while absent means
//! "predates the feature".

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

/// The file a run's manifest is written to.
pub const MANIFEST_FILE: &str = "manifest.json";

/// `Some(inner)` for a key that is present, so an explicit `null` reads as
/// `Some(None)` and an absent key (the field's `default`) as `None`.
fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// `true` only for a JSON `true`.
fn failed_unless_true<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)?.as_bool().unwrap_or(false))
}

/// The string, or empty for anything else.
fn text_or_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Value::deserialize(d)?.as_str().unwrap_or_default().to_string())
}

/// What a lab run recorded about itself.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RunManifest {
    /// The manifest's own revision (4 added `fixture`, 5 `verify`, 6 the work
    /// gate, 7 dropped `metrics.json`, 8 `escalation`). The enrichers only
    /// raise it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    /// The run-manifest data version a newer darkmux is refused on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_schema_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The profile NAME the run was requested under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Whether the dispatch path completed (not whether the workload's own
    /// verify passed: that is `verify`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// Absent: the run predates `verify`. `null`: the workload declares no
    /// verify command, so nothing was checked.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub verify: Option<Option<ManifestVerify>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// The sandbox's content hash after the run; `null` when it could not be hashed.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub final_hash: Option<Option<String>>,
    /// The runtime's `escalation_*` result, written only when the dispatch
    /// stopped on purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<String>,
    /// Runtime artifacts the run refused to preserve; present only when something was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_artifacts: Option<Vec<RefusedArtifactRecord>>,
    /// What the run started from, stamped by the lab after the provider finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<ManifestFixture>,
    /// A key this type does not name (an older run's camelCase spelling, a newer
    /// darkmux's field), kept and re-serialized flat.
    #[serde(flatten)]
    pub extras: Map<String, Value>,
}

/// A run's verify result. A run from before the object form wrote a bare
/// string, and a hand edit can leave anything, so a reading never fails on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ManifestVerify {
    Report(Box<ManifestVerifyReport>),
    /// The pre-object spelling: one word.
    Legacy(String),
    /// A shape no darkmux version wrote.
    Unrecognized(Value),
}

/// The `{passed, details}` verify object, plus the work gate's evidence once it has run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestVerifyReport {
    /// Missing or not a boolean reads as FAILED, never passed (#2833): a verdict that cannot
    /// be read is not one.
    #[serde(default, deserialize_with = "failed_unless_true")]
    pub passed: bool,
    #[serde(default, deserialize_with = "text_or_empty")]
    pub details: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_gate: Option<WorkGate>,
    #[serde(flatten)]
    pub extras: Map<String, Value>,
}

/// What the write-the-tests work gate recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkGate {
    /// The gate could not be applied and failed the run closed.
    Forced { forced_failure_reason: String },
    Evidence(Box<WorkGateEvidence>),
}

/// The gate's measurements. Every key is always written, `null` for one that
/// could not be read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkGateEvidence {
    #[serde(default)]
    pub baseline_test_count: Option<u64>,
    #[serde(default)]
    pub tests_total: Option<u64>,
    #[serde(default)]
    pub tests_passed: Option<u64>,
    /// Signed: a shrinking suite is visible.
    #[serde(default)]
    pub tests_added: Option<i64>,
    #[serde(default)]
    pub tests_failed: Option<u64>,
    #[serde(default)]
    pub tests_skipped: Option<u64>,
    #[serde(default)]
    pub tests_todo: Option<u64>,
    #[serde(default)]
    pub sandbox_changed: Option<bool>,
    #[serde(default)]
    pub coverage_min_pct: Option<f32>,
    #[serde(default)]
    pub coverage_pct: Option<f64>,
    #[serde(default)]
    pub command_tampered: Option<String>,
    /// Whether the verify command itself exited 0, kept apart from the gated `passed`.
    #[serde(default)]
    pub command_passed: Option<bool>,
    #[serde(flatten)]
    pub extras: Map<String, Value>,
}

/// What a run started from.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ManifestFixture {
    /// The canonical source directory; `null` for a self-contained workload with none.
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub baseline_hash: Option<String>,
    #[serde(flatten)]
    pub extras: Map<String, Value>,
}

/// One runtime artifact the run refused to copy, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefusedArtifactRecord {
    pub file: String,
    pub reason: String,
}

impl RunManifest {
    /// Read `<run_dir>/manifest.json`.
    pub fn read(run_dir: &Path) -> Result<Self> {
        let path = run_dir.join(MANIFEST_FILE);
        let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// [`Self::read`], except that a run with no manifest reads as an empty one (a corrupt
    /// manifest is still an error).
    pub fn read_or_default(run_dir: &Path) -> Result<Self> {
        if run_dir.join(MANIFEST_FILE).exists() {
            Self::read(run_dir)
        } else {
            Ok(Self::default())
        }
    }

    /// [`Self::read`] for a reader that treats a missing or unreadable manifest as none.
    pub fn read_lenient(run_dir: &Path) -> Option<Self> {
        Self::read(run_dir).ok()
    }

    /// Write `<run_dir>/manifest.json`, pretty-printed.
    pub fn write(&self, run_dir: &Path) -> Result<()> {
        let path = run_dir.join(MANIFEST_FILE);
        fs::write(&path, serde_json::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }

    /// Raise `schema_version` to at least `to`, never lower it: an enricher mints its
    /// own version for the field it adds, after a provider may have written a higher one.
    pub fn raise_schema_version(&mut self, to: u64) {
        self.schema_version = Some(self.schema_version.unwrap_or(0).max(to));
    }

    /// The verify report, when the run recorded one.
    pub fn verify_report(&self) -> Option<&ManifestVerifyReport> {
        match self.verify.as_ref()?.as_ref()? {
            ManifestVerify::Report(r) => Some(r.as_ref()),
            _ => None,
        }
    }

    /// Whether the run's verify command passed: `None` for "not checked" (null or
    /// absent), and for a legacy or unrecognized shape that names no verdict.
    pub fn verify_passed(&self) -> Option<bool> {
        self.verify_report().map(|r| r.passed)
    }

    /// The content hash the run's sandbox had at the end, when one was recorded.
    pub fn final_hash_value(&self) -> Option<&str> {
        self.final_hash.as_ref()?.as_deref()
    }

    /// The content hash of the fixture the run started from, when one was recorded.
    pub fn baseline_hash(&self) -> Option<&str> {
        self.fixture.as_ref()?.baseline_hash.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trips(raw: Value) -> RunManifest {
        let typed: RunManifest = serde_json::from_value(raw.clone()).expect("reads");
        assert_eq!(serde_json::to_value(&typed).unwrap(), raw, "writes back the JSON it read");
        typed
    }

    /// Every spelling the run store holds: the current object form, the older camelCase one
    /// (kept whole in `extras`), a `verify` that is `null`, absent or a bare word, the work
    /// gate's two shapes, a fixture with no source, a refusal list, and a key a newer darkmux
    /// wrote.
    #[test]
    fn every_shape_the_run_store_holds_writes_back_the_json_it_read() {
        round_trips(json!({"schema_version": 7, "manifest_schema_version": "1.0", "run_id": "r-1", "workload": "w",
            "provider": "coding-task", "profile": "p", "profile_description": "d", "duration_ms": 12, "ok": true,
            "verify": null, "session_id": "s", "sandbox": "/x", "final_hash": null,
            "fixture": {"source_path": null, "baseline_hash": null}}));
        let legacy = round_trips(json!({"durationMs": 5, "ok": false, "profile": "p", "profileDescription": "d",
            "provider": "prompt", "runId": "r", "schemaVersion": 3, "sessionId": "s", "workload": "w"}));
        assert_eq!(legacy.duration_ms, None, "a camelCase key is kept, not read as the snake_case field");
        assert_eq!(round_trips(json!({"verify": "pass"})).verify_passed(), None, "a bare word names no verdict");
        assert_eq!(round_trips(json!({"verify": {"passed": true, "details": "ok"}})).verify_passed(), Some(true));
        let gated = round_trips(json!({"verify": {"passed": false, "details": "no work", "work_gate": {
            "baseline_test_count": 14, "tests_total": 14, "tests_passed": 14, "tests_added": 0, "tests_failed": 0,
            "tests_skipped": 0, "tests_todo": 0, "sandbox_changed": false, "coverage_min_pct": 85.5,
            "coverage_pct": null, "command_tampered": null, "command_passed": true}}}));
        assert!(matches!(gated.verify_report().unwrap().work_gate, Some(WorkGate::Evidence(_))));
        let forced = round_trips(json!({"verify": {"passed": false, "details": "x", "work_gate": {"forced_failure_reason": "x"}}}));
        assert!(matches!(forced.verify_report().unwrap().work_gate, Some(WorkGate::Forced { .. })));
        round_trips(json!({"refused_artifacts": [{"file": "f", "reason": "symlink"}], "escalation": "escalation_x",
            "a_later_key": {"nested": [1, 2]}}));
    }

    /// `null` and absent are different statements: an old run that never recorded `verify` must
    /// not read as one the workload declared none for.
    #[test]
    fn a_verify_that_is_absent_stays_absent_and_one_that_is_null_stays_null() {
        assert_eq!(round_trips(json!({"ok": true})).verify, None);
        assert_eq!(round_trips(json!({"ok": true, "verify": null})).verify, Some(None));
        assert_eq!(round_trips(json!({"final_hash": null})).final_hash, Some(None));
    }

    /// A verdict that cannot be read is a failure, never a pass (#2833).
    #[test]
    fn a_verify_with_no_passed_key_reads_as_failed() {
        let m: RunManifest = serde_json::from_value(json!({"verify": {"details": "no passed key at all"}})).unwrap();
        assert_eq!(m.verify_passed(), Some(false));
        let m: RunManifest = serde_json::from_value(json!({"verify": {"passed": "yes", "details": 4}})).unwrap();
        assert_eq!(m.verify_passed(), Some(false));
    }

    #[test]
    fn the_schema_version_is_only_raised() {
        let mut m = RunManifest { schema_version: Some(8), ..RunManifest::default() };
        m.raise_schema_version(4);
        assert_eq!(m.schema_version, Some(8));
        m.raise_schema_version(9);
        assert_eq!(m.schema_version, Some(9));
        let mut none = RunManifest::default();
        none.raise_schema_version(4);
        assert_eq!(none.schema_version, Some(4));
    }

    /// Operator-run evidence, not CI: `DARKMUX_ROUNDTRIP_LAB=~/.darkmux/lab cargo nextest run -p
    /// darkmux-lab --run-ignored only real_run_manifests` reads every `manifest.json` in a real
    /// run store and checks it writes back the JSON it was read from.
    #[test]
    #[ignore = "reads an operator's own run store: set DARKMUX_ROUNDTRIP_LAB"]
    fn real_run_manifests_round_trip() {
        let root = std::env::var("DARKMUX_ROUNDTRIP_LAB").expect("names a lab directory");
        let (mut n, mut bad) = (0, Vec::new());
        for entry in fs::read_dir(&root).unwrap().flatten() {
            let Ok(raw) = fs::read_to_string(entry.path().join(MANIFEST_FILE)) else { continue };
            let want: Value = serde_json::from_str(&raw).unwrap();
            let typed: RunManifest = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", entry.path().display()));
            n += 1;
            if serde_json::to_value(&typed).unwrap() != want {
                bad.push(entry.path().display().to_string());
            }
        }
        println!("manifests {n}, mismatches {}", bad.len());
        assert!(bad.is_empty(), "{bad:#?}");
        assert!(n > 0);
    }
}
