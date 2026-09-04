//! (#2299) `enabled: false` pruning — applied to a [`MissionConfig`] BEFORE a
//! run is minted, so a disabled item never exists in the run at all.
//!
//! The operator's decision (2026-09-04): **no gray state.** A run's graph is
//! exactly what will execute. Ten rules in a crawl config are ten plan tasks,
//! and a nightly that wants six of them disables four; the run shows six.
//! Provenance is the resolved-config snapshot every run keeps (it carries the
//! flags verbatim) plus the [`PruneReport`] written beside it as
//! `graph-report.json`, so `mission status` can say "12 steps in the config,
//! 4 minted" without drawing anything dead. There is deliberately no CLI
//! override: edit the JSON and run.
//!
//! Four rules, applied in this order:
//!
//! 0. **`not_selected`** (#2301) — a task the LAUNCHER deselected for this
//!    run. `enabled: false` is the document's standing decision; this is
//!    the operator's per-launch one (`--param rules=` on a crawl picks
//!    which rules are planned at all). Both mean the same thing to
//!    everything downstream — the task never exists in the run — so they
//!    share one mechanism and differ only in the `reason` the report
//!    records, which is what tells an operator whether to edit the JSON or
//!    change the invocation.
//! 1. **`disabled`** — a phase, task or step with `enabled: false` is pruned,
//!    and everything under it is pruned with it (`parent_pruned`).
//! 2. **`all_steps_pruned` / `all_tasks_pruned`** — a task whose steps were
//!    all pruned is pruned; a phase whose tasks were all pruned is pruned. A
//!    phase or task that never HAD children (a freeform phase) is untouched:
//!    it was never going to run a step, so nothing was taken from it.
//! 3. **`all_dependencies_pruned`** — a task whose `depends_on` names only
//!    pruned tasks is pruned too, to a fixpoint. A task with at least one
//!    live dependency stays and simply sees fewer inputs; pruned ids are
//!    dropped from the survivors' `depends_on` and `reads` so the interpreter
//!    never meets a dangling reference.

use super::{MissionConfig, PhaseConfig, TaskConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One pruned item and why. `kind` is `phase` | `task` | `step`; `reason` is
/// one of the rule names in the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    pub id: String,
    pub kind: String,
    pub reason: String,
}

/// What the mint saw and what it kept. Written to the run as
/// `graph-report.json` and stamped on the `mission start` record's payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    pub phases_in_config: usize,
    pub phases_minted: usize,
    pub tasks_in_config: usize,
    pub tasks_minted: usize,
    pub steps_in_config: usize,
    pub steps_minted: usize,
    #[serde(default)]
    pub pruned: Vec<Pruned>,
    /// (#2300) One entry per growth event, appended AFTER the mint — a
    /// `grow` template's copies are minted at a phase boundary, long after
    /// this report is first written, so the launcher loads, appends and
    /// saves. Empty on every run whose config declares no `grow`, and on
    /// every pre-#2300 report (serde default), which is why it never
    /// changes the pruning numbers above: growth ADDS tasks the config
    /// never counted, pruning REMOVES ones it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grown: Vec<super::grow::Grown>,
}

impl PruneReport {
    /// (#2300) The one-line human summary of what a run GREW, or `None`
    /// when nothing did. Separate from [`summary_line`](Self::summary_line)
    /// on purpose: that one counts what the CONFIG declared against what
    /// was minted from it, and a grown task was never in the config's
    /// count at all.
    pub fn grown_line(&self) -> Option<String> {
        if self.grown.is_empty() {
            return None;
        }
        Some(
            self.grown
                .iter()
                .map(|g| format!("grew {} task(s) from `{}`", g.minted.len(), g.from))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// True when the config asked for something to be left out.
    pub fn pruned_anything(&self) -> bool {
        !self.pruned.is_empty()
    }

    /// The one-line human summary every surface prints the same way. Every
    /// number is a STEP count, so the units never mix: the parenthetical is
    /// the steps the config left out, whatever rule removed each one.
    pub fn summary_line(&self) -> String {
        format!(
            "{} of {} steps minted ({} left out by config)",
            self.steps_minted,
            self.steps_in_config,
            self.steps_in_config.saturating_sub(self.steps_minted)
        )
    }
}

/// Apply the rules with nothing deselected — `enabled: false` only.
pub fn prune_disabled(config: &MissionConfig) -> (MissionConfig, PruneReport) {
    prune_with_selection(config, &|_| true)
}

/// (#2301) Apply the rules, with `selected` deciding which tasks this
/// LAUNCH wants. A task `selected` returns `false` for is pruned with
/// reason `not_selected`, exactly as if the document had disabled it.
///
/// Returns the pruned config (the one to mint from) and the report. A
/// config with nothing disabled and nothing deselected comes back equal to
/// its input with an empty `pruned` list.
pub fn prune_with_selection(
    config: &MissionConfig,
    selected: &dyn Fn(&TaskConfig) -> bool,
) -> (MissionConfig, PruneReport) {
    let mut report = PruneReport {
        phases_in_config: config.phases.len(),
        tasks_in_config: config.phases.iter().map(|p| p.tasks.len()).sum(),
        steps_in_config: config.phases.iter().flat_map(|p| p.tasks.iter()).map(|t| t.steps.len()).sum(),
        ..PruneReport::default()
    };
    let mut pruned_tasks: BTreeSet<String> = BTreeSet::new();
    let mut out_phases: Vec<PhaseConfig> = Vec::new();

    // Rules 1 and 2.
    for phase in &config.phases {
        if !phase.is_enabled() {
            report.pruned.push(Pruned { id: phase.id.clone(), kind: "phase".into(), reason: "disabled".into() });
            for t in &phase.tasks {
                prune_task_with_children(t, "parent_pruned", &mut report, &mut pruned_tasks);
            }
            continue;
        }
        let mut kept_tasks: Vec<TaskConfig> = Vec::new();
        for task in &phase.tasks {
            if !task.is_enabled() {
                prune_task_with_children(task, "disabled", &mut report, &mut pruned_tasks);
                continue;
            }
            if !selected(task) {
                prune_task_with_children(task, "not_selected", &mut report, &mut pruned_tasks);
                continue;
            }
            let mut kept = task.clone();
            kept.steps.retain(|s| {
                let keep = s.is_enabled();
                if !keep {
                    report.pruned.push(Pruned { id: s.id.clone(), kind: "step".into(), reason: "disabled".into() });
                }
                keep
            });
            if !task.steps.is_empty() && kept.steps.is_empty() {
                report.pruned.push(Pruned {
                    id: task.id.clone(),
                    kind: "task".into(),
                    reason: "all_steps_pruned".into(),
                });
                pruned_tasks.insert(task.id.clone());
                continue;
            }
            kept_tasks.push(kept);
        }
        // A phase emptied here is pruned by the SAME retain that handles a
        // phase emptied by rule 3 below — one place, one reason string.
        let mut kept_phase = phase.clone();
        kept_phase.tasks = kept_tasks;
        out_phases.push(kept_phase);
    }

    // Rule 3, to a fixpoint: a task whose every dependency is gone goes too.
    loop {
        let mut newly: Vec<String> = Vec::new();
        for phase in &out_phases {
            for task in &phase.tasks {
                if !task.depends_on.is_empty() && task.depends_on.iter().all(|d| pruned_tasks.contains(d)) {
                    newly.push(task.id.clone());
                }
            }
        }
        if newly.is_empty() {
            break;
        }
        for id in newly {
            // The task's steps go with it, reported like every other child.
            let task = out_phases.iter().flat_map(|p| p.tasks.iter()).find(|t| t.id == id).cloned();
            if let Some(task) = task {
                prune_task_with_children(&task, "all_dependencies_pruned", &mut report, &mut pruned_tasks);
            }
        }
        for phase in &mut out_phases {
            phase.tasks.retain(|t| !pruned_tasks.contains(&t.id));
        }
    }
    // Rule 2 for phases: a phase that HAD tasks and has none left is pruned,
    // whether rule 1/2 or rule 3 emptied it.
    out_phases.retain(|phase| {
        let had_tasks = config.phases.iter().any(|p| p.id == phase.id && !p.tasks.is_empty());
        if had_tasks && phase.tasks.is_empty() {
            report.pruned.push(Pruned {
                id: phase.id.clone(),
                kind: "phase".into(),
                reason: "all_tasks_pruned".into(),
            });
            return false;
        }
        true
    });
    // Survivors never reference a pruned task.
    for phase in &mut out_phases {
        for task in &mut phase.tasks {
            task.depends_on.retain(|d| !pruned_tasks.contains(d));
            task.reads.retain(|d| !pruned_tasks.contains(d));
        }
    }

    report.phases_minted = out_phases.len();
    report.tasks_minted = out_phases.iter().map(|p| p.tasks.len()).sum();
    report.steps_minted = out_phases.iter().flat_map(|p| p.tasks.iter()).map(|t| t.steps.len()).sum();
    let mut pruned_config = config.clone();
    pruned_config.phases = out_phases;
    (pruned_config, report)
}

fn prune_task_with_children(
    task: &TaskConfig,
    reason: &str,
    report: &mut PruneReport,
    pruned_tasks: &mut BTreeSet<String>,
) {
    report.pruned.push(Pruned { id: task.id.clone(), kind: "task".into(), reason: reason.into() });
    pruned_tasks.insert(task.id.clone());
    for s in &task.steps {
        report.pruned.push(Pruned { id: s.id.clone(), kind: "step".into(), reason: "parent_pruned".into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_config::{interpret, StepConfig};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, enabled: Option<bool>) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            kind: "procedural.noop".to_string(),
            config: serde_json::Value::Null,
            gate: None,
            enabled,
            extras: Map::new(),
        }
    }

    fn task(id: &str, depends_on: &[&str], enabled: Option<bool>, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: Vec::new(),
            role_id: None,
            steps,
            enabled,
            grow: None,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, enabled: Option<bool>, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, enabled, extras: Map::new() }
    }

    fn doc(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "m".to_string(),
            name: "M".to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            panel: None,
            cmd: None,
            outcome_from: None,
            extras: Map::new(),
        }
    }

    fn reasons(report: &PruneReport) -> Vec<(String, String)> {
        report.pruned.iter().map(|p| (p.id.clone(), p.reason.clone())).collect()
    }

    #[test]
    fn a_config_with_no_enabled_fields_mints_exactly_as_before() {
        let cfg = doc(vec![phase("p", None, vec![task("t1", &[], None, vec![step("s1", None), step("s2", None)])])]);
        let (pruned, report) = prune_disabled(&cfg);
        assert_eq!(pruned, cfg, "nothing disabled → the config is untouched");
        assert!(!report.pruned_anything());
        assert_eq!((report.steps_in_config, report.steps_minted), (2, 2));
    }

    #[test]
    fn a_disabled_step_is_pruned_and_the_report_names_it() {
        // 4 steps, 1 disabled → 3 minted.
        let cfg = doc(vec![phase(
            "p",
            None,
            vec![
                task("t1", &[], None, vec![step("s1", None), step("s2", Some(false))]),
                task("t2", &[], None, vec![step("s3", None), step("s4", Some(true))]),
            ],
        )]);
        let (pruned, report) = prune_disabled(&cfg);
        let minted: Vec<&str> = pruned.phases[0].tasks.iter().flat_map(|t| t.steps.iter()).map(|s| s.id.as_str()).collect();
        assert_eq!(minted, vec!["s1", "s3", "s4"]);
        assert_eq!((report.steps_in_config, report.steps_minted), (4, 3));
        assert_eq!(reasons(&report), vec![("s2".to_string(), "disabled".to_string())]);
        assert_eq!(report.summary_line(), "3 of 4 steps minted (1 left out by config)");
    }

    #[test]
    fn a_phase_whose_work_is_all_disabled_is_absent_from_the_graph() {
        let cfg = doc(vec![
            phase("plan", None, vec![task("t1", &[], Some(false), vec![step("s1", None)])]),
            phase("crawl", None, vec![task("t2", &[], None, vec![step("s2", None)])]),
        ]);
        let (pruned, report) = prune_disabled(&cfg);
        let phases: Vec<&str> = pruned.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(phases, vec!["crawl"], "an emptied phase is pruned, not left as a gray shell");
        assert_eq!((report.phases_in_config, report.phases_minted), (2, 1));
        assert!(reasons(&report).contains(&("plan".to_string(), "all_tasks_pruned".to_string())));
        assert!(reasons(&report).contains(&("t1".to_string(), "disabled".to_string())));
        assert!(reasons(&report).contains(&("s1".to_string(), "parent_pruned".to_string())));
    }

    #[test]
    fn a_task_depending_only_on_pruned_tasks_is_pruned_but_one_live_dependency_keeps_it() {
        let cfg = doc(vec![phase(
            "p",
            None,
            vec![
                task("a", &[], Some(false), vec![step("sa", None)]),
                task("b", &[], None, vec![step("sb", None)]),
                task("only-a", &["a"], None, vec![step("s-only-a", None)]),
                task("a-or-b", &["a", "b"], None, vec![step("s-a-or-b", None)]),
                task("chained", &["only-a"], None, vec![step("s-chained", None)]),
            ],
        )]);
        let (pruned, report) = prune_disabled(&cfg);
        let kept: Vec<&str> = pruned.phases[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept, vec!["b", "a-or-b"]);
        assert!(reasons(&report).contains(&("only-a".to_string(), "all_dependencies_pruned".to_string())));
        assert!(
            reasons(&report).contains(&("chained".to_string(), "all_dependencies_pruned".to_string())),
            "pruning runs to a fixpoint: {:?}",
            reasons(&report)
        );
        assert!(
            reasons(&report).contains(&("s-only-a".to_string(), "parent_pruned".to_string()))
                && reasons(&report).contains(&("s-chained".to_string(), "parent_pruned".to_string())),
            "a rule-3 casualty's steps are reported like every other child: {:?}",
            reasons(&report)
        );
        let pruned_steps = report.pruned.iter().filter(|p| p.kind == "step").count();
        assert_eq!(pruned_steps, report.steps_in_config - report.steps_minted, "every pruned step is listed");
        let a_or_b = &pruned.phases[0].tasks[1];
        assert_eq!(a_or_b.depends_on, vec!["b".to_string()], "the pruned id is dropped from the survivor's list");
        // The interpreter must accept the pruned document as-is.
        interpret::interpret(&pruned, &interpret::LaunchParams::default())
            .expect("no dangling depends_on survives pruning");
    }

    #[test]
    fn a_freeform_phase_with_no_tasks_is_never_pruned() {
        let cfg = doc(vec![phase("hand", None, vec![])]);
        let (pruned, report) = prune_disabled(&cfg);
        assert_eq!(pruned.phases.len(), 1);
        assert!(!report.pruned_anything());
    }

    #[test]
    fn a_disabled_phase_prunes_everything_under_it() {
        let cfg = doc(vec![phase("p", Some(false), vec![task("t", &[], None, vec![step("s", None)])])]);
        let (pruned, report) = prune_disabled(&cfg);
        assert!(pruned.phases.is_empty());
        assert_eq!(
            reasons(&report),
            vec![
                ("p".to_string(), "disabled".to_string()),
                ("t".to_string(), "parent_pruned".to_string()),
                ("s".to_string(), "parent_pruned".to_string()),
            ]
        );
    }

    #[test]
    fn enabled_round_trips_through_json_and_absent_means_enabled() {
        let json = r#"{"id":"m","name":"M","phases":[{"id":"p","tasks":[
            {"id":"t","steps":[{"id":"s","kind":"procedural.noop","enabled":false},{"id":"u","kind":"procedural.noop"}]}]}]}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let steps = &cfg.phases[0].tasks[0].steps;
        assert_eq!(steps[0].enabled, Some(false));
        assert!(!steps[0].is_enabled());
        assert_eq!(steps[1].enabled, None);
        assert!(steps[1].is_enabled(), "absent means enabled — the field is the gate, never its presence");
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["phases"][0]["tasks"][0]["steps"][0]["enabled"], serde_json::json!(false));
        assert!(back["phases"][0]["tasks"][0]["steps"][1].get("enabled").is_none(), "absent stays absent");
    }

    /// (#2301) A per-launch deselection is the same mechanism as
    /// `enabled: false`, with its own reason — and it cascades to the
    /// dependents exactly the same way.
    #[test]
    fn a_deselected_task_is_pruned_as_not_selected_and_takes_its_dependents() {
        let mut ruled = step("plan-a-step", None);
        ruled.config = serde_json::json!({ "rule": "a" });
        let mut ruled_b = step("plan-b-step", None);
        ruled_b.config = serde_json::json!({ "rule": "b" });
        let d = doc(vec![
            phase(
                "plan",
                None,
                vec![
                    task("plan-a", &[], None, vec![ruled]),
                    task("plan-b", &[], None, vec![ruled_b]),
                ],
            ),
            phase(
                "run",
                None,
                vec![
                    task("unit-a", &["plan-a"], None, vec![step("unit-a-step", None)]),
                    task("unit-b", &["plan-b"], None, vec![step("unit-b-step", None)]),
                ],
            ),
        ]);
        let wanted = |t: &TaskConfig| {
            t.steps
                .iter()
                .find_map(|s| s.config.get("rule").and_then(|v| v.as_str()))
                .is_none_or(|r| r == "a")
        };
        let (pruned, report) = prune_with_selection(&d, &wanted);

        let live: Vec<&str> =
            pruned.phases.iter().flat_map(|p| p.tasks.iter()).map(|t| t.id.as_str()).collect();
        assert_eq!(live, vec!["plan-a", "unit-a"], "only the selected rule's track survives");
        let reason = |id: &str| {
            report.pruned.iter().find(|p| p.id == id).map(|p| p.reason.clone()).unwrap_or_default()
        };
        assert_eq!(reason("plan-b"), "not_selected", "the deselected task names WHY");
        assert_eq!(reason("unit-b"), "all_dependencies_pruned", "its dependent goes with it");
        assert_eq!(report.tasks_minted, 2);
    }

    /// Nothing deselected is byte-identical to `prune_disabled`.
    #[test]
    fn selecting_everything_prunes_exactly_what_enabled_false_alone_would() {
        let d = doc(vec![phase(
            "p",
            None,
            vec![
                task("t1", &[], None, vec![step("s1", None)]),
                task("t2", &[], Some(false), vec![step("s2", None)]),
            ],
        )]);
        assert_eq!(prune_with_selection(&d, &|_| true), prune_disabled(&d));
    }
}
