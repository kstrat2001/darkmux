//! `records.gather` (#2310 P4c-2b) — this mission's finding + mod records,
//! plus a diff and a scope summary, gathered into the typed shape
//! [`deliver_github_review::DeliverConfig`] reads (`DeliverConfig::
//! from_step`, when `Step.config` carries none of `findings`/`mods`/`diff`/
//! `scope` itself, falls back to a [`GatherOutput`] found on the run's
//! artifact bus — see that module's own doc for the wiring).
//!
//! **Mission-agnostic by construction**, same discipline
//! `deliver_github_review`'s own module doc states: this reads the SHARED
//! finding/mod stores (`crate::findings`, `crate::mods`), scoped by mission
//! id the way `finding list --mission`/`mod list --mission` resolve it
//! (`src/finding_cli.rs`'s `list`: `r.mission_id.as_deref() == Some(m)`;
//! `src/mod_cli.rs`'s `list`: `mods::names_mission(m, x)`) — never a crawl
//! or review type. A crawl's own `create-mods` phase could grow a
//! `records.gather` → a different delivery kind exactly the same way.
//!
//! **Tier 1 (#1352).** Reads config, resolves the mission id off the Task's
//! own phase record (the same trick `darkmux-lab`'s
//! `crawl::unit_step::mission_id_for` uses, duplicated here in miniature
//! because this crate has no dependency on that one — see
//! `deliver_github_review`'s own doc on why `darkmux-crew` never depends
//! on `darkmux-lab`), computes a scope summary from what is on disk, and
//! writes one typed envelope. No model dispatch, no per-mission control
//! flow of its own.
//!
//! **Scope summary, honestly bounded.** `rules_run` and `hunks_covered`
//! come from every `plan/<rule>.json` this mission wrote under
//! `<missions_dir>/<mission-id>/plan/` — read as loosely-typed JSON
//! (`serde_json::Value`), never through `darkmux-lab`'s own `Plan` struct,
//! for the same crate-boundary reason. `hunks_total` comes from parsing
//! the diff itself with the shared `crate::diff::parse_diff` (the same
//! parser `deliver_github_review` uses). `refused` counts mods whose gate
//! is known to have failed (`gate_passed == Some(false)`) — the one
//! "refused" signal this crate can see without a `darkmux-lab` dependency;
//! a `crawl.unit`'s own `findings_rejected` count (a runtime-boundary
//! rejection, before a finding is ever stored) is invisible here by the
//! same crate-boundary rule, and is not counted.

use crate::findings::{self, FindingRecord};
use crate::mods;
use crate::step_kinds::deliver_github_review::{DeliverScope, GatedMod};
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{Port, StepKind, StepOutcome};
use crate::types::{Step, Task};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const RECORDS_GATHER_KIND: &str = "records.gather";

/// Content id both the step's own `provides()` port and the envelope's
/// `kind` use — same one-name-for-both-roles convention `crawl.summary`
/// establishes (`CRAWL_SUMMARY_KIND == CRAWL_SUMMARY_OUTPUT_KIND`).
pub const RECORDS_GATHER_OUTPUT_KIND: &str = "records.gather";

pub const GATHER_OUTPUT_SCHEMA_VERSION: &str = "1.0";

/// What [`RecordsGatherStepKind`] produces — everything
/// `deliver_github_review::render_github_review` needs, gathered from this
/// mission's own stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatherOutput {
    pub schema_version: String,
    pub findings: Vec<FindingRecord>,
    pub mods: Vec<GatedMod>,
    pub diff: String,
    pub scope: DeliverScope,
}

pub struct RecordsGatherStepKind;

impl StepKind for RecordsGatherStepKind {
    fn id(&self) -> &'static str {
        RECORDS_GATHER_KIND
    }

    fn display_name(&self) -> &'static str {
        "Gather records"
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(RECORDS_GATHER_OUTPUT_KIND)];
        &PORTS
    }

    /// (#1979) No model work, no dispatch session — same opt-out
    /// `deliver.github_review`/`procedural.shell`/`procedural.noop` use.
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let mission_id = mission_id_for(task)?;

        let diff = match step.config.get("diff_file").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => std::fs::read_to_string(p)
                .with_context(|| format!("step `{}`: `{RECORDS_GATHER_KIND}` reading config.diff_file {p}", step.id))?,
            _ => String::new(),
        };
        let not_attempted: Vec<String> = step
            .config
            .get("not_attempted")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let findings: Vec<FindingRecord> = findings::load_all_at(&findings::findings_dir())
            .context("loading the finding store")?
            .into_iter()
            .filter(|f| f.mission_id.as_deref() == Some(mission_id.as_str()))
            .collect();

        let mods: Vec<GatedMod> = mods::load_all_at(&mods::mods_dir())
            .context("loading the mod store")?
            .into_iter()
            .filter(|m| mods::names_mission(m, &mission_id))
            .map(|record| {
                let gate_passed = record.gate.as_ref().map(|g| g.passed);
                GatedMod { record, gate_passed }
            })
            .collect();

        let refused = mods.iter().filter(|m| m.gate_passed == Some(false)).count();
        let (rules_run, hunks_covered) = plan_totals(&mission_id);
        let hunks_total: usize =
            crate::diff::parse_diff(&diff).iter().map(|(_, hunks)| hunks.len()).sum();

        let scope = DeliverScope { rules_run, hunks_covered, hunks_total, refused, not_attempted };
        let out = GatherOutput {
            schema_version: GATHER_OUTPUT_SCHEMA_VERSION.to_string(),
            findings,
            mods,
            diff,
            scope,
        };
        let wrapped = crate::step_output::Output::wrap(
            RECORDS_GATHER_OUTPUT_KIND,
            out,
            crate::step_output::Producer::of(&mission_id, &task.id, &step.id),
        );
        Ok(StepOutcome { output: wrapped.to_output_string()?, flow_records: Vec::new() })
    }
}

/// The mission id a Task's own phase record names — the same lookup
/// `darkmux-lab`'s `crawl::unit_step::mission_id_for` performs, duplicated
/// here because `darkmux-crew` cannot depend on `darkmux-lab` (this
/// module's own doc).
fn mission_id_for(task: &Task) -> Result<String> {
    let phases = crate::loader::load_phases().context("loading phase records to locate the run")?;
    phases
        .iter()
        .find(|p| p.id == task.phase_id)
        .map(|p| p.mission_id.clone())
        .ok_or_else(|| {
            anyhow!(
                "records.gather: task `{}` names phase `{}`, which has no record — the run cannot be located",
                task.id,
                task.phase_id
            )
        })
}

/// `(rules_run, hunks_covered)` from every `plan/<rule>.json` this mission
/// wrote — the rule id from the filename stem (a plan task that was pruned
/// or never minted writes no file, so the file set alone answers "which
/// plan tasks minted"), `hunks_covered` from that plan's own recorded
/// `totals.units` (read as loose JSON — see this module's own doc on why
/// not the typed `Plan`). A directory that doesn't exist (no plan phase
/// ran) yields `(vec![], 0)`, not an error — a descriptive summary, same
/// tolerance `crawl::unit_step::plan_totals` extends to a missing/
/// unreadable plan file.
fn plan_totals(mission_id: &str) -> (Vec<String>, usize) {
    let plan_dir = crate::loader::missions_dir().join(mission_id).join("plan");
    let mut rules: Vec<String> = Vec::new();
    let mut hunks_covered = 0usize;
    let Ok(entries) = std::fs::read_dir(&plan_dir) else {
        return (rules, hunks_covered);
    };
    let mut paths: Vec<std::path::PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Ok(raw) = std::fs::read_to_string(path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        rules.push(stem.to_string());
        if let Some(units) = value.pointer("/body/totals/units").and_then(|v| v.as_u64()) {
            hunks_covered += units as usize;
        }
    }
    (rules, hunks_covered)
}

/// Register `records.gather` — the same opt-in shape
/// `deliver_github_review::register_deliver_kind` uses: no caller
/// registers this by default; a mission config that wires both a gather
/// and a deliver step (`src/mission_launch.rs::all_step_kinds`) does.
pub fn register_records_gather_kind(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(RecordsGatherStepKind)).context("registering records.gather")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::{self, Proposer, Scope};
    use crate::mods::{ForFinding, ModContext, ModRecord};
    use crate::types::{NodeStatus, Phase, PhaseStatus};
    use serde_json::json;
    use tempfile::TempDir;

    /// Scopes `DARKMUX_HOME` for one test and restores the prior value —
    /// same pattern `crawl::unit_step_tests::HomeGuard` uses, duplicated
    /// here because this crate has no dependency on that one.
    struct HomeGuard(Option<String>);
    impl HomeGuard {
        fn set(p: &std::path::Path) -> Self {
            let prior = std::env::var("DARKMUX_HOME").ok();
            std::env::set_var("DARKMUX_HOME", p);
            Self(prior)
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    const MISSION: &str = "review-v2-2310";
    const PHASE: &str = "review-v2-2310-deliver";

    fn save_phase() {
        crate::lifecycle::save_phase(&Phase {
            id: PHASE.into(),
            mission_id: MISSION.into(),
            description: String::new(),
            display_name: None,
            status: PhaseStatus::Running,
            created_ts: 1,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: vec!["deliver".into()],
        })
        .unwrap();
    }

    fn task() -> Task {
        Task {
            id: "deliver".into(),
            phase_id: PHASE.into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["records-gather-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
            run_on: crate::types::default_run_on(),
        }
    }

    fn step(config: serde_json::Value) -> Step {
        Step {
            id: "records-gather-step".into(),
            task_id: "deliver".into(),
            kind: RECORDS_GATHER_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn a_finding(dispatch: &str, seq: u64, mission: Option<&str>) -> FindingRecord {
        findings::build_record(
            dispatch,
            seq,
            "2026-09-05T00:00:00Z".to_string(),
            "create_finding",
            Proposer { handle: "reviewer".into(), model: "test".into(), machine_id: None },
            Scope { mission_id: mission.map(str::to_string), phase_id: None, step_id: None },
            None,
            json!({ "file": "src/a.ts", "line": 2, "pattern": "p", "evidence": "e", "why": "w" }),
        )
    }

    fn a_mod(key: &str, for_key: &str, mission: Option<&str>) -> ModRecord {
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
                    mission_id: mission.map(str::to_string),
                    context: None,
                    emitted: None,
                    missing: false,
                }],
            },
            warnings: Vec::new(),
            mission_id: mission.map(str::to_string),
            phase_id: None,
            step_id: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn gathers_only_this_missions_findings_and_mods() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();

        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-b", 1, Some("other-mission"))).unwrap();
        crate::mods::materialize(&crate::mods::mods_dir(), &a_mod("mod-1", "sess-a/1", Some(MISSION))).unwrap();
        crate::mods::materialize(&crate::mods::mods_dir(), &a_mod("mod-2", "sess-b/1", Some("other-mission")))
            .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.findings.len(), 1, "{:?}", wrapped.body.findings);
        assert_eq!(wrapped.body.findings[0].key, "sess-a/1");
        assert_eq!(wrapped.body.mods.len(), 1, "{:?}", wrapped.body.mods);
        assert_eq!(wrapped.body.mods[0].record.key, "mod-1");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn reads_the_diff_file_and_counts_its_hunks_as_hunks_total() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let diff_path = tmp.path().join("d.diff");
        std::fs::write(
            &diff_path,
            "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,1 +1,2 @@\n foo\n+bar\n\
             diff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1,1 +1,2 @@\n baz\n+qux\n",
        )
        .unwrap();

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.diff, std::fs::read_to_string(&diff_path).unwrap());
        assert_eq!(wrapped.body.scope.hunks_total, 2, "{:?}", wrapped.body.scope);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn refused_counts_gate_failed_mods_only() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();

        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 2, Some(MISSION))).unwrap();
        let root = crate::mods::mods_dir();
        crate::mods::materialize(&root, &a_mod("mod-pass", "sess-a/1", Some(MISSION))).unwrap();
        crate::mods::materialize(&root, &a_mod("mod-fail", "sess-a/2", Some(MISSION))).unwrap();
        crate::mods::record_gate(
            &root,
            "mod-pass",
            Some(crate::mods::GateOutcome { passed: true, command: "true".into(), exit_code: Some(0) }),
            None,
        )
        .unwrap();
        crate::mods::record_gate(
            &root,
            "mod-fail",
            Some(crate::mods::GateOutcome { passed: false, command: "false".into(), exit_code: Some(1) }),
            None,
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.scope.refused, 1, "{:?}", wrapped.body.scope);
        let passed_gate = wrapped.body.mods.iter().find(|m| m.record.key == "mod-pass").unwrap();
        assert_eq!(passed_gate.gate_passed, Some(true));
        let failed_gate = wrapped.body.mods.iter().find(|m| m.record.key == "mod-fail").unwrap();
        assert_eq!(failed_gate.gate_passed, Some(false));
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn rules_run_and_hunks_covered_come_from_this_missions_plan_files() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(
            plan_dir.join("existing-solution.json"),
            serde_json::to_string(&json!({
                "kind": "crawl.plan",
                "body": { "totals": { "units": 3 } }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            plan_dir.join("union-vs-enum.json"),
            serde_json::to_string(&json!({
                "kind": "crawl.plan",
                "body": { "totals": { "units": 2 } }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.scope.rules_run, vec!["existing-solution".to_string(), "union-vs-enum".to_string()]);
        assert_eq!(wrapped.body.scope.hunks_covered, 5);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_task_naming_an_unrecorded_phase_is_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        // No `save_phase()` call — the phase record does not exist.
        let err = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains(PHASE), "{err}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn deliver_github_review_reads_a_records_gather_step_as_its_own_task_predecessor() {
        // (#2310 P4c-2b) The wiring this packet adds: `records.gather` and
        // `deliver.github_review` as two steps of ONE task, the SAME
        // same-task-predecessor `input` entry `scheduler::gather_inputs`
        // already threads to every multi-step task — no `step.config`
        // literal `findings`/`mods`/`diff`/`scope` at all.
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();

        let gather_out =
            RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let mut input = BTreeMap::new();
        input.insert("records-gather-step".to_string(), gather_out.output);

        let deliver_step = Step {
            id: "deliver-step".into(),
            task_id: "deliver".into(),
            kind: super::super::deliver_github_review::DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config: json!({ "emit": "-" }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let outcome =
            super::super::deliver_github_review::DeliverGithubReviewStepKind.run(&deliver_step, &task(), &input).unwrap();
        assert_eq!(outcome.output, "-");
    }
}
