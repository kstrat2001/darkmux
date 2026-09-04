//! `mods.gate` (#2310 P4c-2b) — the create-mods confirmation gate.
//! DESIGN.md "the changed files name the test targets, which is what makes
//! confirmation cheap enough to do per finding": run `config.test_command`
//! (when the review declared one) against `config.workdir`, and record the
//! outcome onto every stored mod naming `config.for_key`
//! ([`mods::record_gate`]).
//!
//! **Mission-agnostic by construction**, same discipline every other
//! `step_kinds::` Tier 1 kind in this crate follows: this reads/writes the
//! SHARED mod store (`crate::mods`) keyed by a finding key handed in
//! through config, never a review or crawl type.
//!
//! **Tier 1 (#1352).** A fixed procedure — run one command, or don't; write
//! one fact onto zero or more already-stored records — config-driven, no
//! caller-supplied strategy, no per-mission control flow. Physically its
//! own file (not folded into `builtins.rs`, already ~4200 lines) for the
//! same monolith-avoidance reason `deliver_github_review.rs` states for
//! itself.
//!
//! **No test_command configured is not an error.** `review-v2.json`'s
//! `test_command` input is optional; when the launch never set it, EVERY
//! mod this task's finding names is recorded `gate_skipped_reason: "no
//! test_command configured"` rather than left silently ungated forever —
//! `deliver_github_review`'s own render already tells "confirmed" from
//! "never gated" apart (`GatedMod::gate_passed: None`), so an honest skip
//! reads correctly downstream with no special-casing there.
//!
//! **A nonzero exit is DATA, not a step failure.** Unlike
//! `procedural.shell` (which bails the step on a nonzero exit), this kind
//! never fails the STEP over the command's own exit code — `passed: false`
//! is exactly as valid an outcome as `passed: true`; only a genuine
//! infrastructure problem (an unreadable mod store, a malformed config)
//! fails the step itself.

use crate::mods::{self, GateOutcome};
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{StepKind, StepOutcome};
use crate::types::{Step, Task};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const MODS_GATE_KIND: &str = "mods.gate";

/// What one `mods.gate` step reports as its own output — a small summary,
/// never the gated mods themselves (a reader wanting those reads the mod
/// store, the same discipline `crawl.summary` uses for findings).
#[derive(Debug, Clone, Serialize)]
struct GateSummary {
    for_key: String,
    mods_seen: usize,
    mods_gated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_reason: Option<String>,
}

pub struct ModsGateStepKind;

impl StepKind for ModsGateStepKind {
    fn id(&self) -> &'static str {
        MODS_GATE_KIND
    }

    fn display_name(&self) -> &'static str {
        "Gate"
    }

    /// (#1979) No model work, no dispatch session — same opt-out
    /// `deliver.github_review`/`procedural.shell` use.
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    fn run(&self, step: &Step, _task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let for_key = step
            .config
            .get("for_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("step `{}`: `{MODS_GATE_KIND}` requires config.for_key", step.id))?;
        let test_command =
            step.config.get("test_command").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());
        let workdir = step.config.get("workdir").and_then(|v| v.as_str());

        let root = mods::mods_dir();
        let all = mods::load_all_at(&root).context("loading the mod store")?;
        let targets = mods::mods_for(&all, for_key);

        let skipped_reason = if test_command.is_none() { Some("no test_command configured".to_string()) } else { None };
        let mut mods_gated = 0usize;
        for m in &targets {
            // (#2310 P4c-2b self-QA mutation kill — proven vacuous, so
            // stated precisely) Already gated (a re-run of this step, or
            // two units racing on the same finding key): this check is a
            // PERFORMANCE optimization that skips the `test_command`
            // process spawn entirely, never the CORRECTNESS guard — that
            // one is `mods::record_gate`'s own already-gated check, which
            // still protects a stored mod's gate from being overwritten
            // even with this line deleted (mutation-killed: removing it
            // left every existing test green, because `record_gate` is
            // the authoritative guard). Kept because re-spawning
            // `test_command` on every re-run of a large create-mods phase
            // is real, avoidable cost — not because correctness depends
            // on it.
            if m.gate.is_some() || m.gate_skipped_reason.is_some() {
                continue;
            }
            let outcome = test_command.map(|cmd| run_gate(cmd, workdir));
            let res = mods::record_gate(&root, &m.key, outcome, skipped_reason.as_deref())
                .with_context(|| format!("step `{}`: recording the gate for mod `{}`", step.id, m.key))?;
            if res == mods::Materialized::Created {
                mods_gated += 1;
            }
        }

        let summary =
            GateSummary { for_key: for_key.to_string(), mods_seen: targets.len(), mods_gated, skipped_reason };
        Ok(StepOutcome {
            output: serde_json::to_string(&summary).context("serializing the gate summary")?,
            flow_records: Vec::new(),
        })
    }
}

/// Run `command` in `workdir` (or the process's own cwd when unset) via
/// `sh -c`. A command that could not even be spawned (missing
/// interpreter, unreadable `workdir`) is `passed: false` with no
/// `exit_code` — a real fact ("this gate did not confirm"), never
/// propagated as a step `Err`; see this module's own doc.
fn run_gate(command: &str, workdir: Option<&str>) -> GateOutcome {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(w) = workdir {
        cmd.current_dir(w);
    }
    match cmd.output() {
        Ok(out) => GateOutcome { passed: out.status.success(), command: command.to_string(), exit_code: out.status.code() },
        Err(_) => GateOutcome { passed: false, command: command.to_string(), exit_code: None },
    }
}

/// Register `mods.gate` — same opt-in shape
/// `deliver_github_review::register_deliver_kind`/`records_gather::
/// register_records_gather_kind` use.
pub fn register_mods_gate_kind(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(ModsGateStepKind)).context("registering mods.gate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::{ForFinding, ModContext, ModRecord};
    use crate::types::{NodeStatus, Task};
    use serde_json::json;
    use tempfile::TempDir;

    /// Scopes `DARKMUX_MODS_DIR` for one test and restores the prior value
    /// on drop — including on panic/assert failure (unlike a manual
    /// `remove_var` at the end of a test body, which a failed assertion
    /// skips), the same RAII discipline `records_gather::tests::HomeGuard`
    /// uses for `DARKMUX_HOME`.
    struct ModsDirGuard(Option<String>);
    impl ModsDirGuard {
        fn set(p: &std::path::Path) -> Self {
            let prior = std::env::var("DARKMUX_MODS_DIR").ok();
            std::env::set_var("DARKMUX_MODS_DIR", p);
            Self(prior)
        }
    }
    impl Drop for ModsDirGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_MODS_DIR", v),
                None => std::env::remove_var("DARKMUX_MODS_DIR"),
            }
        }
    }

    fn a_mod(key: &str, for_key: &str) -> ModRecord {
        ModRecord {
            key: key.to_string(),
            ts: "2026-09-05T00:00:01Z".to_string(),
            by: "coder".to_string(),
            r#for: vec![for_key.to_string()],
            kit: Some("kit text".to_string()),
            kit_looks_json: false,
            kit_kind: None,
            attachments: Vec::new(),
            context: ModContext {
                findings: vec![ForFinding {
                    key: for_key.to_string(),
                    mission_id: None,
                    context: None,
                    emitted: None,
                    missing: false,
                }],
            },
            warnings: Vec::new(),
            mission_id: None,
            phase_id: None,
            step_id: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    fn step(config: serde_json::Value) -> Step {
        Step {
            id: "gate-step".into(),
            task_id: "create-mod-1".into(),
            kind: MODS_GATE_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn task() -> Task {
        Task {
            id: "create-mod-1".into(),
            phase_id: "p".into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["create-mod-step".into(), "gate-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
            run_on: crate::types::default_run_on(),
        }
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn no_test_command_configured_records_a_skip_reason_on_every_matching_mod() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-1", "sess-a/1")).unwrap();

        let outcome = ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/1" })), &task(), &BTreeMap::new())
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_seen"], 1);
        assert_eq!(summary["skipped_reason"], "no test_command configured");

        let rec = mods::load_at(tmp.path(), "mod-1").unwrap().unwrap();
        assert!(rec.gate.is_none());
        assert_eq!(rec.gate_skipped_reason.as_deref(), Some("no test_command configured"));
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_passing_command_records_gate_passed_true() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-2", "sess-a/2")).unwrap();

        ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/2", "test_command": "true" })), &task(), &BTreeMap::new())
            .unwrap();

        let rec = mods::load_at(tmp.path(), "mod-2").unwrap().unwrap();
        assert!(rec.gate.as_ref().unwrap().passed, "{:?}", rec.gate);
        assert_eq!(rec.gate.as_ref().unwrap().command, "true");
        assert!(rec.gate_skipped_reason.is_none());
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_failing_command_records_gate_passed_false_not_a_step_error() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-3", "sess-a/3")).unwrap();

        let result = ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/3", "test_command": "false" })), &task(), &BTreeMap::new());
        assert!(result.is_ok(), "a failing gate command is DATA, never a step failure: {result:?}");

        let rec = mods::load_at(tmp.path(), "mod-3").unwrap().unwrap();
        assert!(!rec.gate.as_ref().unwrap().passed);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_mod_already_gated_is_left_untouched_on_a_second_pass() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-4", "sess-a/4")).unwrap();
        mods::record_gate(
            tmp.path(),
            "mod-4",
            Some(GateOutcome { passed: true, command: "original".into(), exit_code: Some(0) }),
            None,
        )
        .unwrap();

        let outcome = ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/4", "test_command": "false" })), &task(), &BTreeMap::new())
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_gated"], 0, "already gated — the second pass changes nothing");

        let rec = mods::load_at(tmp.path(), "mod-4").unwrap().unwrap();
        assert_eq!(rec.gate.as_ref().unwrap().command, "original", "the original gate result survives untouched");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn no_matching_mods_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        let outcome =
            ModsGateStepKind.run(&step(json!({ "for_key": "sess-a/nope" })), &task(), &BTreeMap::new()).unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_seen"], 0);
        assert_eq!(summary["mods_gated"], 0);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_missing_for_key_is_refused_by_name() {
        let err = ModsGateStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("for_key"), "{err}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn the_kind_registers_via_its_own_dedicated_function() {
        let registry = StepKindRegistry::new();
        register_mods_gate_kind(&registry).unwrap();
        assert!(registry.ids().iter().any(|id| id == MODS_GATE_KIND));
    }
}
