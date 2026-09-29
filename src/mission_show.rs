//! `darkmux mission show <id>`: one mission's config, its phases, tasks and
//! steps, its runs, tokens, and a viewer link. `mission status` stays the
//! board; this is the one-mission read.
//!
//! **One derivation, two surfaces.** [`build`] assembles a [`MissionShow`]
//! and [`render_text`] prints it. `darkmux mission show <id>`, the editor
//! panel's `/mission show <id>` and `--json` all read that one value. It
//! reuses what the daemon already derives instead of re-deriving it:
//!
//! - the phase/task/step graph, with each finished step's tokens, turns and
//!   model, is `darkmux_serve::mission_graph::build_mission_graph`, the
//!   function behind `GET /mission/:id/graph.json`;
//! - the runs and their tokens are the `darkmux_serve::Run` rows
//!   `darkmux run list` and `GET /runs` read, filtered to this mission's id;
//! - the config is the registry entry the mission was launched from
//!   (`Mission.spec.config_id`), through `mission_config::load`.
//!
//! READ-ONLY, like `mission status`.

use anyhow::{bail, Result};
use darkmux_serve::mission_graph::{GraphNode, MissionGraph, NodeKind, StepRow};
use darkmux_serve::{Run, RunKind};
use darkmux_types::config_access;
use serde::Serialize;

use crate::crew;
use crate::crew::types::MissionStatus;

/// The one wire spelling of a serde-lowercase enum (`active`, `running`,
/// `phase`), for the text view. Derived from serde so the text and `--json`
/// cannot drift onto two hand-kept lists.
fn wire_word<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// The config a mission was launched from, as it resolves in the registry
/// today.
#[derive(Debug, Clone, Serialize)]
pub struct ShownConfig {
    pub id: String,
    pub name: String,
    /// The registry tier it resolves from now (`user`, `on-disk`, `embedded`).
    pub source: String,
    /// The first sentence of the config's `description`.
    pub summary: String,
    /// The inputs the config declares, with whether each is required.
    pub inputs: Vec<ShownInput>,
}

/// One declared input of a [`ShownConfig`].
#[derive(Debug, Clone, Serialize)]
pub struct ShownInput {
    pub name: String,
    pub required: bool,
}

/// One mission, everything `mission show` reports. The `--json` shape of
/// `darkmux mission show <id>`: a semver-bound contract.
#[derive(Debug, Clone, Serialize)]
pub struct MissionShow {
    pub id: String,
    /// `active`, `finalized` or `aborted`; absent when this machine holds no
    /// record of the mission (a run observed from a peer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MissionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `None` for a mission with no resolvable config (a `dispatch <role>`
    /// crew-of-one, a hand-authored mission, or a config since deleted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ShownConfig>,
    /// The `GET /mission/:id/graph.json` value: phase and task nodes with
    /// their step rows. `None` when this machine holds no record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<MissionGraph>,
    /// The `run list` rows whose id is this mission's.
    pub runs: Vec<Run>,
    /// ALL tokens of the mission's run (the `run list` TOKENS column); absent
    /// when nothing measured any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// The viewer's page for this mission.
    pub link: String,
}

/// The config a mission was launched from, when it names a loadable one.
fn shown_config(mission: &crew::types::Mission) -> Option<ShownConfig> {
    let config_id = mission.spec.as_ref()?.config_id.as_str();
    let loaded = crew::mission_config::load(config_id).ok()?;
    Some(ShownConfig {
        id: config_id.to_string(),
        name: loaded.config.name.clone(),
        source: loaded.source.label().to_string(),
        summary: crate::acp_panel::config_summary(&loaded.config),
        inputs: loaded
            .config
            .inputs
            .iter()
            .map(|i| ShownInput { name: i.name.clone(), required: i.is_required_of_operator() })
            .collect(),
    })
}

/// The run rows for mission `id`, from the same union `darkmux run list`
/// prints.
fn runs_of(id: &str) -> Vec<Run> {
    let flows_dir = config_access::flows_dir();
    let lab_dir = config_access::lab_dir();
    let fleet = darkmux_serve::fleet_records_for_runs();
    let built = darkmux_serve::build_runs_with_usage(&flows_dir, Some(&lab_dir), &fleet.records, None);
    built.runs.into_iter().filter(|r| r.id == id).collect()
}

/// Assemble the show value for mission `id`. `Err` when neither this
/// machine's mission records nor the run union know the id.
pub fn build(id: &str) -> Result<MissionShow> {
    let mission = crew::loader::load_missions()?.into_iter().find(|m| m.id == id);
    let graph = darkmux_serve::mission_graph::build_mission_graph(id, &config_access::flows_dir())?;
    let runs = runs_of(id);
    if mission.is_none() && runs.is_empty() {
        bail!("no mission `{id}`: `darkmux mission status` lists the missions on this machine");
    }
    let tokens = runs.iter().find(|r| r.kind != RunKind::Lab).and_then(|r| r.tokens);
    let link = crate::mission_status::mission_url(&crate::mission_status::board_link_base(), id);
    Ok(MissionShow {
        id: id.to_string(),
        status: graph.as_ref().map(|g| g.mission_status),
        description: mission.as_ref().map(|m| m.description.trim().to_string()).filter(|d| !d.is_empty()),
        config: mission.as_ref().and_then(shown_config),
        graph,
        runs,
        tokens,
        link,
    })
}

/// `darkmux mission show <id>`.
pub fn run(id: &str, json: bool) -> Result<i32> {
    let show = build(id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&show)?);
    } else {
        println!("{}", render_text(&show));
    }
    Ok(0)
}

fn config_lines(config: &ShownConfig) -> Vec<String> {
    let mut lines = vec![format!("Config: {} ({}, {} tier)", config.id, config.name, config.source)];
    lines.push(format!("  {}", config.summary));
    if !config.inputs.is_empty() {
        let inputs: Vec<String> = config
            .inputs
            .iter()
            .map(|i| if i.required { format!("{} (required)", i.name) } else { i.name.clone() })
            .collect();
        lines.push(format!("  inputs: {}", inputs.join(", ")));
    }
    lines
}

fn step_line(step: &StepRow) -> String {
    let mut line = format!("      {} [{}] {}", step.id, step.kind, wire_word(&step.status));
    if let Some(tokens) = step.tokens_final {
        line.push_str(&format!(" · {} tokens", crate::run_list::grouped(tokens)));
    }
    if let Some(turns) = step.turns_final {
        line.push_str(&format!(" · {turns} turns"));
    }
    if let Some(model) = &step.model {
        line.push_str(&format!(" · {model}"));
    }
    line
}

fn task_lines(task: &GraphNode) -> Vec<String> {
    let mut lines = vec![format!("    {} {}", task.label, wire_word(&task.status))];
    lines.extend(task.steps.iter().flatten().map(step_line));
    lines
}

fn graph_lines(graph: &MissionGraph) -> Vec<String> {
    let mut lines = vec!["Phases:".to_string()];
    for phase in graph.nodes.iter().filter(|n| n.kind == NodeKind::Phase) {
        let note = phase.status_note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
        lines.push(format!("  {} {}{note}", phase.label, wire_word(&phase.status)));
        for task in graph.nodes.iter().filter(|n| n.parent_id.as_deref() == Some(phase.id.as_str())) {
            lines.extend(task_lines(task));
        }
    }
    if let Some(note) = &graph.note {
        lines.push(format!("  note: {note}"));
    }
    lines
}

fn run_line(run: &Run) -> String {
    let tokens = run.tokens.map(|t| format!(" · {} tokens", crate::run_list::grouped(t))).unwrap_or_default();
    format!("  {} {}{tokens}", crate::run_list::kind_label(run.kind), crate::run_list::status_label(run.status))
}

/// Render `show` as plain text, no color: the terminal and the editor panel
/// print the same lines.
pub fn render_text(show: &MissionShow) -> String {
    let status = show.status.map(|s| format!(" ({})", wire_word(&s))).unwrap_or_default();
    let mut lines = vec![format!("Mission {}{status}", show.id)];
    if let Some(description) = &show.description {
        lines.push(crate::mission_status::cap_note(crate::mission_status::first_sentence(description), 160));
    }
    if let Some(config) = &show.config {
        lines.extend(config_lines(config));
    }
    if let Some(graph) = &show.graph {
        lines.extend(graph_lines(graph));
    }
    lines.push("Runs:".to_string());
    lines.extend(show.runs.iter().map(run_line));
    if let Some(tokens) = show.tokens {
        lines.push(format!("Tokens: {}", crate::run_list::grouped(tokens)));
    }
    lines.push(format!("Viewer: {}", show.link));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew::types::{Mission, MissionSpec, MissionStatus, Phase, PhaseStatus};

    /// Points `DARKMUX_HOME` and the flow dir at a tempdir for one test and
    /// restores both on drop, so a panic in an assertion cannot leave the
    /// process reading a deleted directory. Callers are `#[serial_test::serial]`.
    struct Isolated {
        _dir: tempfile::TempDir,
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl Isolated {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let vars = [("DARKMUX_HOME", dir.path().to_path_buf()), ("DARKMUX_FLOWS_DIR", dir.path().join("flows"))];
            let prev = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
            for (k, v) in &vars {
                // SAFETY: every caller is #[serial_test::serial].
                unsafe { std::env::set_var(k, v) };
            }
            Isolated { _dir: dir, prev }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            for (k, v) in &self.prev {
                // SAFETY: every caller is #[serial_test::serial].
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    fn save_mission(id: &str, config_id: Option<&str>) {
        let phase_id = format!("{id}-p1");
        let mission = Mission {
            id: id.to_string(),
            description: "Shows the machine. A second sentence.".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec![phase_id.clone()],
            created_ts: 1,
            started_ts: Some(1),
            finalized_ts: None,
            source_input: None,
            ticket: None,
            spec: config_id.map(|c| MissionSpec {
                config_id: c.to_string(),
                origin: None,
                inputs_fingerprint: "f".to_string(),
            }),
            machine: None,
        };
        crew::lifecycle::save_mission(&mission).unwrap();
        crew::lifecycle::save_phase(&Phase {
            id: phase_id,
            mission_id: id.to_string(),
            description: "d".to_string(),
            display_name: Some("The phase".to_string()),
            status: PhaseStatus::Running,
            created_ts: 1,
            started_ts: Some(1),
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        })
        .unwrap();
    }

    /// `build` reads one mission's record, graph, config and run row from
    /// disk, and the text view prints the same facts.
    #[test]
    #[serial_test::serial]
    fn build_assembles_the_config_graph_and_run_of_one_mission() {
        let _iso = Isolated::new();
        save_mission("show-m1", Some("machine-status"));

        let show = build("show-m1").unwrap();
        assert_eq!(show.id, "show-m1");
        assert_eq!(show.status, Some(MissionStatus::Active));
        let json = serde_json::to_value(&show).unwrap();
        assert_eq!(json["status"], "active", "--json keeps the lowercase wire word");
        let config = show.config.as_ref().expect("the spec names a loadable config");
        assert_eq!((config.id.as_str(), config.name.as_str()), ("machine-status", "Machine status"));
        assert!(config.summary.to_ascii_lowercase().contains("loaded"), "{}", config.summary);
        let graph = show.graph.as_ref().expect("a local mission has a graph");
        assert!(graph.nodes.iter().any(|n| n.kind == NodeKind::Phase && n.label == "The phase"), "{:?}", graph.nodes);
        assert!(show.runs.iter().any(|r| r.id == "show-m1"), "the run row for this mission: {:?}", show.runs);
        assert!(show.link.ends_with("/#mission=show-m1"), "{}", show.link);

        let text = render_text(&show);
        for needle in ["Mission show-m1 (active)", "Config: machine-status (Machine status", "The phase running", "Runs:", "Viewer: "] {
            assert!(text.contains(needle), "`{needle}` missing from:\n{text}");
        }
    }

    /// A mission with no resolvable config still shows: `config` is absent,
    /// never an error.
    #[test]
    #[serial_test::serial]
    fn a_mission_without_a_loadable_config_shows_without_one() {
        let _iso = Isolated::new();
        save_mission("show-m2", Some("dispatch"));
        let show = build("show-m2").unwrap();
        assert!(show.config.is_none());
        assert!(!render_text(&show).contains("Config:"));
    }

    /// The inverse of the two above: an id nothing knows is refused, naming
    /// the board that lists what exists.
    #[test]
    #[serial_test::serial]
    fn an_unknown_mission_is_refused_naming_the_board() {
        let _iso = Isolated::new();
        let err = build("no-such-mission").unwrap_err().to_string();
        assert!(err.contains("no mission `no-such-mission`") && err.contains("mission status"), "{err}");
    }

    /// `mission_id` is filled by the launcher, so `mission show` must not
    /// call it required; the genuinely required inputs stay marked.
    #[test]
    #[serial_test::serial]
    fn a_launcher_supplied_input_is_not_listed_as_required() {
        let _iso = Isolated::new();
        save_mission("show-m4", Some("coder-phase"));
        let show = build("show-m4").unwrap();
        let inputs = &show.config.as_ref().unwrap().inputs;
        let required = |name: &str| inputs.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("no `{name}`: {inputs:?}")).required;
        assert!(!required("mission_id"), "launch fills mission_id itself");
        assert!(required("workdir"), "workdir is asked of the operator");
        let text = render_text(&show);
        assert!(!text.contains("mission_id (required)") && text.contains("workdir (required)"), "{text}");
    }

    /// The `--json` contract: the named keys a consumer reads.
    #[test]
    #[serial_test::serial]
    fn the_json_shape_carries_the_contract_keys() {
        let _iso = Isolated::new();
        save_mission("show-m3", Some("machine-status"));
        let v = serde_json::to_value(build("show-m3").unwrap()).unwrap();
        for key in ["id", "status", "description", "config", "graph", "runs", "link"] {
            assert!(v.get(key).is_some(), "missing `{key}` in {v}");
        }
        assert_eq!(v["graph"]["mission_id"], "show-m3");
        assert_eq!(v["config"]["id"], "machine-status");
        assert!(v["config"]["inputs"].is_array());
    }
}
