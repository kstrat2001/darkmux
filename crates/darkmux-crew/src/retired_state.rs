//! Retired spellings in mission state files.
//!
//! `mission.json` and a task's JSON are operator state, not append-only
//! archives, so each is read in ONE spelling. A file still using a spelling an
//! older darkmux wrote is refused, naming the new one, rather than loaded as
//! if the field were absent: serde ignores an unknown key, so a `sprint_ids`
//! list would otherwise come back as a mission with no phases. `darkmux
//! doctor` reports the same files ([`scan`]), and the loaders refuse them
//! ([`parse_state`]). Only a flow record's `phase_id` keeps a `sprint_id`
//! reader: flow archives are append-only (`darkmux_flow::legacy`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Which state file a document is, and so which retired spellings apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    Mission,
    Phase,
    Task,
    Step,
}

/// A retired top-level key and the key that replaced it.
struct RetiredKey {
    kind: StateKind,
    old: &'static str,
    new: &'static str,
}

const RETIRED_KEYS: &[RetiredKey] = &[
    RetiredKey { kind: StateKind::Mission, old: "sprint_ids", new: "phase_ids" },
    RetiredKey { kind: StateKind::Mission, old: "closed_ts", new: "finalized_ts" },
    RetiredKey { kind: StateKind::Task, old: "sprint_id", new: "phase_id" },
];

/// The pre-`finalized` spelling of a mission's terminal status.
const RETIRED_STATUS: (&str, &str) = ("closed", "finalized");

/// One line per retired spelling in `doc`, each naming the fix. Empty when
/// the document uses only current spellings.
pub fn retired_spellings(kind: StateKind, doc: &Value) -> Vec<String> {
    let mut found: Vec<String> = RETIRED_KEYS
        .iter()
        .filter(|r| r.kind == kind && doc.get(r.old).is_some())
        .map(|r| format!("`{}` was renamed to `{}` in 4.0: rename the key", r.old, r.new))
        .collect();
    let (old, new) = RETIRED_STATUS;
    if kind == StateKind::Mission && doc.get("status").and_then(Value::as_str) == Some(old) {
        found.push(format!("status `{old}` was renamed to `{new}` in 4.0: change the value"));
    }
    found
}

/// Parse a state file, refusing one that uses a retired spelling. `path` only
/// names the file in the refusal.
pub fn parse_state<T: DeserializeOwned>(kind: StateKind, path: &Path, text: &str) -> Result<T> {
    let doc: Value = serde_json::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    let retired = retired_spellings(kind, &doc);
    if !retired.is_empty() {
        bail!("{} uses a retired spelling. {}", path.display(), retired.join("; "));
    }
    serde_json::from_value(doc).with_context(|| format!("parsing {}", path.display()))
}

/// A state file or directory darkmux no longer reads, and the one-line fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateProblem {
    pub path: PathBuf,
    pub fix: String,
}

/// The pre-`phases` directory name under a mission directory.
pub const RETIRED_PHASES_DIR: &str = "sprints";

/// The retired `sprints/` directory under `mission_dir`, when it exists: the
/// phases in it are no longer read.
pub fn retired_phases_dir(mission_dir: &Path) -> Option<StateProblem> {
    let path = mission_dir.join(RETIRED_PHASES_DIR);
    path.is_dir().then(|| StateProblem {
        fix: format!("rename `{RETIRED_PHASES_DIR}/` to `phases/` (renamed in 4.0; the phases in it are not read)"),
        path,
    })
}

fn file_problems(kind: StateKind, path: &Path) -> Option<StateProblem> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let retired = retired_spellings(kind, &doc);
    (!retired.is_empty()).then(|| StateProblem { path: path.to_path_buf(), fix: retired.join("; ") })
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
}

/// Every mission state file (and directory) under `missions_root` that uses a
/// retired spelling, sorted by path.
pub fn scan(missions_root: &Path) -> Vec<StateProblem> {
    let mut found = Vec::new();
    let mut missions: Vec<PathBuf> = std::fs::read_dir(missions_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    missions.sort();
    for dir in missions {
        found.extend(file_problems(StateKind::Mission, &dir.join("mission.json")));
        found.extend(retired_phases_dir(&dir));
        let mut phase_task_dirs: Vec<PathBuf> = std::fs::read_dir(dir.join("tasks"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        phase_task_dirs.sort();
        for tasks in phase_task_dirs {
            found.extend(json_files(&tasks).iter().filter_map(|f| file_problems(StateKind::Task, f)));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mission, MissionStatus, Task};
    use serde_json::json;

    fn mission_doc(extra: Value) -> Value {
        let mut doc = json!({"id": "m", "description": "d", "status": "active", "phase_ids": ["p1"], "created_ts": 1});
        doc.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        doc
    }

    #[test]
    fn a_current_mission_document_has_no_retired_spellings() {
        assert!(retired_spellings(StateKind::Mission, &mission_doc(json!({"finalized_ts": 5}))).is_empty());
    }

    #[test]
    fn each_retired_mission_spelling_is_named_with_its_replacement() {
        let mut doc = mission_doc(json!({"sprint_ids": ["s1"], "closed_ts": 9, "status": "closed"}));
        doc.as_object_mut().unwrap().remove("phase_ids");
        let lines = retired_spellings(StateKind::Mission, &doc);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("`sprint_ids` was renamed to `phase_ids`")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("`closed_ts` was renamed to `finalized_ts`")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("status `closed` was renamed to `finalized`")), "{lines:?}");
    }

    #[test]
    fn a_task_names_its_retired_phase_key_and_a_mission_document_is_not_judged_by_it() {
        let task = json!({"sprint_id": "p1"});
        assert_eq!(retired_spellings(StateKind::Task, &task).len(), 1);
        assert!(retired_spellings(StateKind::Mission, &task).is_empty());
        assert!(retired_spellings(StateKind::Phase, &task).is_empty());
        assert!(retired_spellings(StateKind::Step, &task).is_empty());
    }

    /// The recovery case: the refusal is what stops a retired file from
    /// loading as a mission with no phases, and renaming the key fixes it.
    #[test]
    fn a_retired_mission_file_is_refused_and_the_renamed_one_loads() {
        let path = Path::new("/x/mission.json");
        let mut old = mission_doc(json!({"sprint_ids": ["s1", "s2"]}));
        old.as_object_mut().unwrap().remove("phase_ids");
        let err = parse_state::<Mission>(StateKind::Mission, path, &old.to_string()).unwrap_err().to_string();
        assert!(err.contains("/x/mission.json") && err.contains("`sprint_ids` was renamed to `phase_ids`"), "{err}");

        let fixed = mission_doc(json!({"phase_ids": ["s1", "s2"]}));
        let m: Mission = parse_state(StateKind::Mission, path, &fixed.to_string()).unwrap();
        assert_eq!(m.phase_ids, ["s1", "s2"]);
        assert_eq!(m.status, MissionStatus::Active);
    }

    #[test]
    fn a_retired_task_file_is_refused() {
        let doc = json!({"id": "t", "sprint_id": "p1", "description": "d"});
        let err = parse_state::<Task>(StateKind::Task, Path::new("/x/t.json"), &doc.to_string()).unwrap_err();
        assert!(err.to_string().contains("`sprint_id` was renamed to `phase_id`"), "{err}");
    }

    #[test]
    fn scan_names_the_file_and_the_retired_directory() {
        let root = tempfile::tempdir().unwrap();
        let m = root.path().join("m1");
        std::fs::create_dir_all(m.join("sprints")).unwrap();
        std::fs::create_dir_all(m.join("tasks").join("p1")).unwrap();
        std::fs::write(m.join("mission.json"), mission_doc(json!({"closed_ts": 3})).to_string()).unwrap();
        std::fs::write(m.join("tasks/p1/t.json"), json!({"sprint_id": "p1"}).to_string()).unwrap();
        let clean = root.path().join("m2");
        std::fs::create_dir_all(&clean).unwrap();
        std::fs::write(clean.join("mission.json"), mission_doc(json!({})).to_string()).unwrap();

        let problems = scan(root.path());
        let paths: Vec<_> = problems.iter().map(|p| p.path.strip_prefix(root.path()).unwrap().to_owned()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("m1/mission.json"),
                PathBuf::from("m1/sprints"),
                PathBuf::from("m1/tasks/p1/t.json"),
            ]
        );
        assert!(problems[1].fix.contains("rename `sprints/` to `phases/`"), "{problems:?}");
    }
}
