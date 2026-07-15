//! Mission configs — missions as DATA (#1284 Packet 1).
//!
//! A mission config is a named JSON document declaring a mission's graph
//! SHAPE: phases (ordered), each phase's tasks, each task's steps, each
//! step naming a REGISTERED step-kind id (`step_kinds::StepKind::id()`)
//! plus a kind-specific `config` object. A later packet (#1284 Packet 3)
//! interprets a resolved config into an executable `Task`/`Step` graph —
//! the same shape `build_review_graph` (`darkmux-lab`'s `lab::review`) and
//! `default_phase_graph` (the `darkmux` binary's `mission_run.rs`) build BY
//! HAND today, one Rust function per mission type. This packet builds the
//! schema, the loader, the built-in transcriptions of those two graphs, and
//! the `darkmux doctor` surface — it does NOT wire configs into execution
//! (Packet 3) and does NOT touch `mission propose` (Packet 4).
//!
//! **Lenient-on-read (contract 7, `CLAUDE.md` "Cross-system contracts"):**
//! every struct here carries `#[serde(flatten)] extras` overflow, optional
//! fields stay `Option`, and an unrecognized field or a newer
//! `schema_version` never fails PARSING. Semantic validation is the
//! SEPARATE [`MissionConfig::validate`] pass — never invoked on the hot
//! load path ([`load`] never calls it).
//!
//! **Naming note.** This `MissionConfig` is a MISSION GRAPH document
//! (phases/tasks/steps) — a completely different concept from the OPERATOR
//! `config.json` `mission{}` block (drift-detection knobs like
//! `stale_active_days`), which was `darkmux_types::config::MissionConfig`
//! until the #1284 review round renamed it `MissionBoardConfig` precisely
//! so THIS type could own the bare name as the arc's headline concept
//! (Rust-only rename; the serde field stays `mission`, so operator
//! config.json files are untouched).

pub mod load;

pub use load::{load, list_ids, LoadedMissionConfig, MissionConfigSource};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Current schema version for mission config documents. Plain semver
/// applied to the DATA SHAPE — mirrors `RULES_SCHEMA_VERSION`
/// (`darkmux-eureka`), `FLOW_SCHEMA_VERSION` (`darkmux-flow`),
/// `CONFIG_SCHEMA_VERSION` (`darkmux-types::config`). Starts at "1.0"
/// (#1284 Packet 1). Bump discipline (see `CLAUDE.md`'s "Versioning" —
/// same rule, different data shape): additive field/section → minor;
/// rename/retype/new-required-field → major.
pub const MISSION_CONFIG_SCHEMA: &str = "1.0";

/// One mission config document — the whole graph SHAPE, as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Schema version this document was authored against. `#[serde(default)]`
    /// so a document that omits it still parses — treated as compatible
    /// (no schema-version finding) by [`MissionConfig::validate`], since
    /// absence isn't drift, it's just an unlabeled document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Runtime-only values the LAUNCHER (Packet 3+) must supply before this
    /// config can become an executable graph — a diff, a worktree path, a
    /// case id, resolved crew staffing. Declared here so the document is
    /// self-describing about what it needs from its caller, WITHOUT those
    /// genuinely per-launch values living inside the static document
    /// itself. See [`MissionInput`]'s doc.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<MissionInput>,
    /// Ordered — a phase's position in this list IS its ordering (mirrors
    /// `crew::types::Mission::phase_ids`'s #1341 strictly-linear-phase
    /// doctrine: no `depends_on` at the phase level, "the phase before this
    /// one" is purely positional).
    #[serde(default)]
    pub phases: Vec<PhaseConfig>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer document read by an older binary).
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// A declared runtime-only input a mission config's LAUNCHER must supply
/// (Packet 3+) — a value that is genuinely per-launch (a diff, a worktree
/// path, a case id) and therefore doesn't belong IN the static document.
/// Purely a documentation contract at this packet — nothing consumes it
/// yet; Packet 3 decides how a named input maps onto the composed
/// `Task`/`Step` fields it feeds (workdir, role override, step config
/// substitution, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the launcher must supply this input to proceed. Absent
    /// (`None`) is treated as `true` (required) by convention — an
    /// undeclared optionality on a runtime-only input is the more
    /// conservative default (better to demand a value than silently run
    /// with one missing). Not enforced by [`MissionConfig::validate`] at
    /// this packet (no launcher exists yet to check against) — recorded
    /// here as the intended semantic for Packet 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One phase, as data. `id` is a SUFFIX — the launcher composes the real
/// `Phase.id` (e.g. `<mission-id>-<suffix>`), matching
/// `build_review_graph`'s caller-supplied `investigate_phase_id`-style
/// convention. **A phase with zero tasks is valid by design** — it
/// expresses a manual/freeform phase (operator-driven transitions, no
/// automated Task/Step graph underneath): a duration-container phase
/// ("wait for the trip") or a blog-post phase ("draft by hand, no
/// dispatch"). `validate()` does not require `tasks` to be non-empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One task, as data — mirrors `crew::types::Task`'s shape (the ASSIGNABLE
/// unit: role/profile/workdir/image fixed for the task's whole duration,
/// `depends_on` the only cross-task dependency/concurrency declaration).
/// `id` and `depends_on` entries are DOCUMENT-WIDE (not phase-scoped) —
/// `depends_on` may name a task in an EARLIER phase, exactly as
/// `build_review_graph`'s `report` phase's `synthesis` task depends on the
/// `investigate` phase's `dedup` task. `profile_name`/`workdir`/`image` are
/// deliberately NOT fields here (unlike the real `Task`) — those are
/// genuinely per-launch values (a worktree path, an image override) that
/// belong in [`MissionConfig::inputs`], not the static document; see the
/// packet report for how the two built-in configs use `role_id` as an
/// overridable default and push workdir/image to `inputs` entirely.
///
/// **Placeholder-prefix rule (task AND step ids).** When the Rust builder a
/// config transcribes composes its `Task`/`Step` ids from a caller-supplied
/// phase id (`default_phase_graph`'s `format!("{phase_id}-worktree")` /
/// `-coder` / `-verify`, steps appending `-step`), the config writes those
/// ids with the owning [`PhaseConfig::id`] as a LITERAL prefix — that
/// literal prefix stands in for the real phase id: at launch (#1284
/// Packet 3), the launcher substitutes its composed phase id for the
/// phase-config id wherever it prefixes a task/step id, so the persisted
/// ids match what the Rust builder produces today byte for byte (task ids
/// surface in `mission status`, the viewer, and lifecycle records — a
/// silent id-scheme change at cutover is exactly what this rule prevents).
/// A config whose builder uses FIXED ids (`build_review_graph`'s
/// `review-bundle-task` etc.) writes them verbatim — no substitution. Each
/// built-in config names which convention it uses in its own top-level
/// `description`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Default crew role this task dispatches, e.g. `"coder"`. Not
    /// necessarily final — Packet 3's launcher may let a `MissionInput`
    /// override it per-launch (the built-in `coder-phase` config declares
    /// exactly that intent — see its `inputs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// Ordered — step at index `i` depends on the step at index `i - 1`
    /// (mirrors `crew::types::Step`'s #1341 "no depends_on field" —
    /// purely positional intra-task ordering).
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One step, as data. `kind` names a REGISTERED `step_kinds::StepKind` id —
/// Tier 1 generic (e.g. `"dispatch.internal"`), Tier 2 pattern, or a Tier 3
/// mission-bespoke id (e.g. `"review.bundle"`, `"mission.worktree"`, #1352).
/// `config` is kind-specific and opaque to this schema — mirrors
/// `crew::types::Step.config`'s own flat `serde_json::Value` bag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepConfig {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// Severity of a [`ValidationFinding`]. `Error` blocks a config from being
/// USABLE (Packet 3's launcher would refuse it); `Warning` is
/// operator-actionable but non-blocking — notably, an unrecognized step
/// kind is ALWAYS `Warning`, never `Error` (see `validate`'s doc: a Tier 3
/// kind that registers at composition time is invisible to a
/// document-level check, so "unknown" doesn't mean "wrong").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingSeverity {
    Error,
    Warning,
}

/// One actionable validation finding — a JSON-pointer-ish `path` into the
/// document plus a human-readable `message`. `Display` renders the
/// `[level] path: message` line both `darkmux doctor` and a future CLI can
/// print verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationFinding {
    pub severity: FindingSeverity,
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            FindingSeverity::Error => "error",
            FindingSeverity::Warning => "warning",
        };
        write!(f, "[{level}] {}: {}", self.path, self.message)
    }
}

impl MissionConfig {
    /// Semantic validation — SEPARATE from parsing (contract 7): a
    /// lenient-on-read document always PARSES regardless of content; this
    /// is what a caller (`darkmux doctor`, Packet 3's launcher) runs to
    /// decide whether the document is actually USABLE. [`load`] never
    /// calls this — semantic validation never runs on the hot load path.
    ///
    /// `known_step_kinds` is the caller's registered-kind universe (e.g.
    /// `StepKindRegistry::with_builtins().ids()`). A step whose `kind`
    /// isn't in it produces a `Warning`, NEVER an `Error`: Tier 3 kinds
    /// (#1352) register into their OWN per-mission registry at
    /// COMPOSITION time (`build_review_graph`, `default_phase_graph`),
    /// which no document-level check can see — an "unknown" kind here may
    /// simply be a Tier 3 id this call site's registry doesn't carry, not
    /// a real mistake. Pass `&[]` to skip the kind-reference check
    /// entirely (an empty universe is treated as "unverifiable", not
    /// "everything is wrong" — no kind warnings are emitted).
    pub fn validate(&self, known_step_kinds: &[&str]) -> Vec<ValidationFinding> {
        let mut findings = Vec::new();

        if self.id.trim().is_empty() {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "id".to_string(),
                message: "mission config id is empty".to_string(),
            });
        }
        if self.name.trim().is_empty() {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "name".to_string(),
                message: "mission config name is empty".to_string(),
            });
        }

        if let Some(v) = &self.schema_version {
            match parse_major(v) {
                Some(major) if major != current_major() => {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Warning,
                        path: "schema_version".to_string(),
                        message: format!(
                            "document schema_version \"{v}\" (major {major}) differs from this \
                             binary's MISSION_CONFIG_SCHEMA \"{MISSION_CONFIG_SCHEMA}\" (major \
                             {}) — a major-version mismatch may mean this binary can't fully \
                             interpret the document, or vice versa",
                            current_major()
                        ),
                    });
                }
                None => {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Warning,
                        path: "schema_version".to_string(),
                        message: format!("schema_version \"{v}\" is not a MAJOR.MINOR string"),
                    });
                }
                _ => {}
            }
        }

        // Ids are DOCUMENT-WIDE, not phase/task-scoped — `Task.depends_on`
        // may cross phases, and every `Step`/`Task` lands in one flat map
        // once composed into a real graph, so a same-named collision
        // anywhere in the document is a real problem.
        let mut seen_phase_ids: BTreeSet<&str> = BTreeSet::new();
        let mut seen_task_ids: BTreeSet<&str> = BTreeSet::new();
        let mut seen_step_ids: BTreeSet<&str> = BTreeSet::new();
        let mut all_task_ids: BTreeSet<&str> = BTreeSet::new();

        for phase in &self.phases {
            for task in &phase.tasks {
                all_task_ids.insert(task.id.as_str());
            }
        }

        for (pi, phase) in self.phases.iter().enumerate() {
            let phase_path = format!("phases[{pi}]");
            if phase.id.trim().is_empty() {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("{phase_path}.id"),
                    message: "phase id is empty".to_string(),
                });
            } else if !seen_phase_ids.insert(phase.id.as_str()) {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("{phase_path}.id"),
                    message: format!("duplicate phase id \"{}\"", phase.id),
                });
            }

            for (ti, task) in phase.tasks.iter().enumerate() {
                let task_path = format!("{phase_path}.tasks[{ti}]");
                if task.id.trim().is_empty() {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.id"),
                        message: "task id is empty".to_string(),
                    });
                } else if !seen_task_ids.insert(task.id.as_str()) {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.id"),
                        message: format!("duplicate task id \"{}\"", task.id),
                    });
                }

                for dep in &task.depends_on {
                    if dep == &task.id {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{task_path}.depends_on"),
                            message: format!("task \"{}\" depends_on itself", task.id),
                        });
                    } else if !all_task_ids.contains(dep.as_str()) {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{task_path}.depends_on"),
                            message: format!(
                                "task \"{}\" depends_on unknown task id \"{dep}\"",
                                task.id
                            ),
                        });
                    }
                }

                for (si, step) in task.steps.iter().enumerate() {
                    let step_path = format!("{task_path}.steps[{si}]");
                    if step.id.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.id"),
                            message: "step id is empty".to_string(),
                        });
                    } else if !seen_step_ids.insert(step.id.as_str()) {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.id"),
                            message: format!("duplicate step id \"{}\"", step.id),
                        });
                    }
                    if step.kind.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.kind"),
                            message: "step kind is empty".to_string(),
                        });
                    } else if !known_step_kinds.is_empty()
                        && !known_step_kinds.contains(&step.kind.as_str())
                    {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Warning,
                            path: format!("{step_path}.kind"),
                            message: format!(
                                "step \"{}\" references unknown step kind \"{}\"",
                                step.id, step.kind
                            ),
                        });
                    }
                }
            }
        }

        findings
    }

    /// True when [`validate`](Self::validate) reports zero `Error`
    /// findings (`Warning`s don't block usability — e.g. an unrecognized
    /// Tier 3 step kind is expected, not a failure).
    pub fn is_valid(&self, known_step_kinds: &[&str]) -> bool {
        !self
            .validate(known_step_kinds)
            .iter()
            .any(|f| f.severity == FindingSeverity::Error)
    }
}

fn current_major() -> u32 {
    parse_major(MISSION_CONFIG_SCHEMA).expect("MISSION_CONFIG_SCHEMA is a valid MAJOR.MINOR constant")
}

fn parse_major(v: &str) -> Option<u32> {
    v.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_json() -> &'static str {
        r#"{"id":"x","name":"X"}"#
    }

    #[test]
    fn minimal_document_parses_and_validates_clean() {
        let cfg: MissionConfig = serde_json::from_str(minimal_json()).unwrap();
        assert_eq!(cfg.id, "x");
        assert_eq!(cfg.name, "X");
        assert!(cfg.phases.is_empty());
        assert!(cfg.inputs.is_empty());
        assert!(cfg.validate(&[]).is_empty());
    }

    #[test]
    fn missing_id_or_name_fails_to_parse() {
        // `id`/`name` are required (non-Option) fields, matching
        // `WorkloadSpec.id`'s precedent (`workloads::types::
        // workload_manifest_rejects_missing_id`) — a document that omits
        // the core identity fields fails to PARSE, not just to validate.
        let missing_id = r#"{"name":"X"}"#;
        assert!(serde_json::from_str::<MissionConfig>(missing_id).is_err());
        let missing_name = r#"{"id":"x"}"#;
        assert!(serde_json::from_str::<MissionConfig>(missing_name).is_err());
    }

    #[test]
    fn empty_id_is_a_validate_time_error_not_a_parse_error() {
        let json = r#"{"id":"","name":"X"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == FindingSeverity::Error && f.path == "id"),
            "expected an id-empty error, got {findings:?}"
        );
    }

    #[test]
    fn unknown_top_level_field_is_tolerated_lenient_on_read() {
        let json = r#"{"id":"x","name":"X","totallyNewField":{"nested":true}}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.extras.get("totallyNewField"),
            Some(&serde_json::json!({"nested": true}))
        );
    }

    #[test]
    fn newer_schema_version_parses_and_warns_not_errors() {
        let json = r#"{"id":"x","name":"X","schema_version":"99.0"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(findings.iter().all(|f| f.severity == FindingSeverity::Warning));
        assert!(findings.iter().any(|f| f.path == "schema_version"));
    }

    #[test]
    fn unparseable_schema_version_warns() {
        let json = r#"{"id":"x","name":"X","schema_version":"not-a-version"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == FindingSeverity::Warning && f.path == "schema_version")
        );
    }

    #[test]
    fn absent_schema_version_is_not_drift() {
        let cfg: MissionConfig = serde_json::from_str(minimal_json()).unwrap();
        assert!(cfg.schema_version.is_none());
        assert!(cfg.validate(&[]).is_empty());
    }

    fn step(id: &str, kind: &str) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            kind: kind.to_string(),
            config: serde_json::Value::Null,
            extras: BTreeMap::new(),
        }
    }

    fn task(id: &str, depends_on: &[&str], steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: id.to_string(),
            description: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            steps,
            extras: BTreeMap::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig {
            id: id.to_string(),
            description: None,
            tasks,
            extras: BTreeMap::new(),
        }
    }

    fn doc(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "m".to_string(),
            name: "M".to_string(),
            description: None,
            schema_version: Some(MISSION_CONFIG_SCHEMA.to_string()),
            inputs: Vec::new(),
            phases,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn phase_with_zero_tasks_is_valid() {
        let cfg = doc(vec![phase("wait", vec![])]);
        assert!(cfg.is_valid(&[]));
    }

    #[test]
    fn dangling_depends_on_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &["nonexistent"], vec![step("s1", "dispatch.internal")])],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Error
                && f.message.contains("nonexistent")),
            "expected a dangling depends_on error, got {findings:?}"
        );
    }

    #[test]
    fn cross_phase_depends_on_resolves_cleanly() {
        // A later phase's task depending on an earlier phase's task is
        // exactly `build_review_graph`'s synthesis→dedup shape — must NOT
        // be flagged as dangling.
        let cfg = doc(vec![
            phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])]),
            phase("p2", vec![task("t2", &["t1"], vec![step("s2", "dispatch.internal")])]),
        ]);
        assert!(cfg.is_valid(&["dispatch.internal"]));
    }

    #[test]
    fn self_referential_depends_on_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &["t1"], vec![step("s1", "dispatch.internal")])],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("depends_on itself")));
    }

    #[test]
    fn duplicate_task_id_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![
                task("dup", &[], vec![step("s1", "dispatch.internal")]),
                task("dup", &[], vec![step("s2", "dispatch.internal")]),
            ],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("duplicate task id")));
    }

    #[test]
    fn duplicate_phase_id_is_caught() {
        let cfg = doc(vec![phase("dup", vec![]), phase("dup", vec![])]);
        let findings = cfg.validate(&[]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("duplicate phase id")));
    }

    #[test]
    fn unknown_step_kind_is_a_warning_not_an_error() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![step("s1", "review.bundle")])],
        )]);
        // Only Tier 1 kinds known to this call site — "review.bundle" is a
        // real Tier 3 kind, but this check can't see it.
        let findings = cfg.validate(&["dispatch.internal", "procedural.shell"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Warning && f.message.contains("review.bundle")));
        assert!(!findings.iter().any(|f| f.severity == FindingSeverity::Error));
        assert!(cfg.is_valid(&["dispatch.internal", "procedural.shell"]));
    }

    #[test]
    fn empty_known_kinds_skips_the_kind_check_entirely() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![step("s1", "anything.at.all")])],
        )]);
        assert!(cfg.validate(&[]).is_empty());
    }

    #[test]
    fn empty_step_kind_is_an_error() {
        let cfg = doc(vec![phase("p1", vec![task("t1", &[], vec![step("s1", "")])])]);
        let findings = cfg.validate(&[]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.path.ends_with(".kind")));
    }

    #[test]
    fn finding_display_renders_level_path_message() {
        let f = ValidationFinding {
            severity: FindingSeverity::Warning,
            path: "phases[0].id".to_string(),
            message: "test message".to_string(),
        };
        assert_eq!(f.to_string(), "[warning] phases[0].id: test message");
    }

    // ─── Built-in config golden-shape tests (#1284 Packet 1) ──────────
    // Mirrors `workloads::types::tests::
    // medium_coding_embedded_manifest_has_expected_shape`'s style: no
    // separate golden-file mechanism exists in this codebase — "golden"
    // means the expected shape is locked in code. These parse the EMBEDDED
    // constants directly (`load::find_embedded`), NOT through `load::load`'s
    // user → on-disk → embedded chain (#1284 review round 1): they are
    // goldens for the embedded documents, and going through the chain both
    // tested the wrong thing and was unisolated — it raced the loader's
    // `#[serial]` tests that write user-tier stubs (serial_test does not
    // block non-serial tests), and would deterministically read a real
    // operator override at `~/.darkmux/mission-configs/<id>.json`. Chain
    // resolution of the embedded tier keeps its own `#[serial]` test in
    // `load::tests::embedded_resolves_with_no_user_or_on_disk_copy`.

    fn embedded_config(id: &str) -> MissionConfig {
        let raw = load::find_embedded(id)
            .unwrap_or_else(|| panic!("`{id}` must be in EMBEDDED_MISSION_CONFIGS"));
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("embedded `{id}` must parse: {e}"))
    }

    fn known_kinds() -> Vec<String> {
        crate::step_kinds::StepKindRegistry::with_builtins().ids()
    }

    fn known_kinds_refs(known: &[String]) -> Vec<&str> {
        known.iter().map(String::as_str).collect()
    }

    #[test]
    fn review_builtin_has_the_expected_graph_shape() {
        let cfg = &embedded_config("review");
        assert_eq!(cfg.id, "review");
        assert_eq!(cfg.schema_version.as_deref(), Some(MISSION_CONFIG_SCHEMA));
        assert!(!cfg.inputs.is_empty(), "review declares its runtime-only inputs");
        assert!(
            cfg.inputs.iter().any(|i| i.name == "crew_staffing"),
            "the dynamic-probe-count limitation must be named as a declared input"
        );

        let phase_ids: Vec<&str> = cfg.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(phase_ids, vec!["investigate", "adjudicate", "report"]);

        let investigate = &cfg.phases[0];
        let investigate_task_ids: Vec<&str> =
            investigate.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            investigate_task_ids,
            vec!["review-bundle-task", "review-probe-template-task", "review-dedup-task"]
        );

        let adjudicate = &cfg.phases[1];
        assert_eq!(adjudicate.tasks.len(), 1);
        assert_eq!(adjudicate.tasks[0].id, "review-judge-task");
        assert_eq!(adjudicate.tasks[0].depends_on, vec!["review-dedup-task"]);
        assert_eq!(adjudicate.tasks[0].steps[0].kind, "review.judge");
        assert_eq!(
            adjudicate.tasks[0].steps[0].config,
            serde_json::json!({"concurrency": 1})
        );

        let report = &cfg.phases[2];
        let report_task_ids: Vec<&str> = report.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(report_task_ids, vec!["review-verify-task", "review-synthesis-task"]);
        // synthesis depends on BOTH dedup and verify — the cross-phase edge
        // `build_review_graph` wires (see that function's doc).
        assert_eq!(
            report.tasks[1].depends_on,
            vec!["review-dedup-task", "review-verify-task"]
        );

        // The probe-template task documents the dynamic-expansion limitation
        // via extras (no schema field for it yet — see the config's own
        // "notes"/"expands_per_staffed_seat" keys).
        let probe_template = &investigate.tasks[1];
        assert_eq!(
            probe_template.extras.get("expands_per_staffed_seat"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(probe_template.extras.contains_key("notes"));
    }

    #[test]
    fn coder_phase_builtin_has_the_expected_graph_shape() {
        let cfg = &embedded_config("coder-phase");
        assert_eq!(cfg.id, "coder-phase");
        assert_eq!(cfg.schema_version.as_deref(), Some(MISSION_CONFIG_SCHEMA));
        assert!(
            cfg.inputs.iter().any(|i| i.name == "workdir"),
            "workdir must be declared as a runtime-only input, not a TaskConfig field"
        );

        assert_eq!(cfg.phases.len(), 1);
        let phase = &cfg.phases[0];
        assert_eq!(phase.id, "build");

        // Task ids match `default_phase_graph`'s exact construction
        // (`{phase_id}-worktree` / `-coder` / `-verify` — NO `-task`
        // suffix), with the literal `build-` prefix standing in for the
        // launcher-composed phase id per the placeholder-prefix rule (see
        // `TaskConfig`'s doc). #1284 review round 1 caught the original
        // `-task`-suffixed divergence — persisted Task.ids surface in
        // `mission status`, the viewer, and lifecycle records, so the
        // config must not silently change them at Packet 3 cutover.
        let task_ids: Vec<&str> = phase.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(task_ids, vec!["build-worktree", "build-coder", "build-verify"]);

        assert!(phase.tasks[0].depends_on.is_empty());
        assert_eq!(phase.tasks[1].depends_on, vec!["build-worktree"]);
        assert_eq!(phase.tasks[2].depends_on, vec!["build-coder"]);

        // Step ids likewise match the Rust scheme: `{phase_id}-<name>-step`.
        assert_eq!(phase.tasks[0].steps[0].id, "build-worktree-step");
        assert_eq!(phase.tasks[1].steps[0].id, "build-coder-step");
        assert_eq!(phase.tasks[2].steps[0].id, "build-verify-step");

        assert_eq!(phase.tasks[0].steps[0].kind, "mission.worktree");
        assert_eq!(phase.tasks[1].steps[0].kind, "mission.coder");
        assert_eq!(phase.tasks[2].steps[0].kind, "mission.verify");

        assert_eq!(phase.tasks[1].role_id.as_deref(), Some("coder"));
        assert_eq!(phase.tasks[2].role_id.as_deref(), Some("code-reviewer"));

        // The coder task's description matches the Rust builder's dynamic
        // form with the default role substituted (`dispatch `{role}` into
        // the worktree`) — the `role` input overrides both at launch.
        assert_eq!(
            phase.tasks[1].description.as_deref(),
            Some("dispatch `coder` into the worktree")
        );
    }

    #[test]
    fn both_builtins_validate_with_zero_error_findings() {
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let findings = cfg.validate(&known_refs);
            let errors: Vec<&ValidationFinding> = findings
                .iter()
                .filter(|f| f.severity == FindingSeverity::Error)
                .collect();
            assert!(
                errors.is_empty(),
                "`{id}` must validate with zero Error findings, got: {errors:?}"
            );
            assert!(
                cfg.is_valid(&known_refs),
                "`{id}` must report is_valid() true (Warnings don't block usability)"
            );
        }
    }

    #[test]
    fn both_builtins_reference_only_tier_3_kinds_unknown_to_tier_1() {
        // Every real step kind in both built-ins is Tier 3 (`review.*` /
        // `mission.*`) — none collide with the four Tier 1 builtins, so
        // validating against ONLY `with_builtins()`'s ids should warn on
        // every step (the doctor-visible "can't see Tier 3" case this
        // packet's doctor check documents), never silently pass them as
        // "known".
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let findings = cfg.validate(&known_refs);
            let warnings: Vec<&ValidationFinding> = findings
                .iter()
                .filter(|f| f.severity == FindingSeverity::Warning && f.path.ends_with(".kind"))
                .collect();
            assert!(
                !warnings.is_empty(),
                "`{id}` should warn on its Tier 3 kinds against a Tier-1-only known set"
            );
        }
    }

    #[test]
    fn both_builtins_round_trip_through_json() {
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let json = serde_json::to_string(&cfg).unwrap();
            let back: MissionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(cfg, back, "`{id}` must round-trip through JSON unchanged");
        }
    }

    #[test]
    fn document_round_trips_through_json() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task(
                "t1",
                &[],
                vec![StepConfig {
                    id: "s1".to_string(),
                    kind: "dispatch.internal".to_string(),
                    config: serde_json::json!({"role": "coder"}),
                    extras: BTreeMap::new(),
                }],
            )],
        )]);
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }
}
