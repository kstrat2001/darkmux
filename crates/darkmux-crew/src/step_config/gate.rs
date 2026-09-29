//! The unknown-key gate over mission-config step configs: for every step of
//! every task, the step's `config` is checked against the struct its `kind`
//! selects (`ConfigKind`). It runs beside the document walk in the mission
//! config's preflight (`crate::user_files`), which sees `config` as an open
//! value.
//!
//! A task's `grow.config` is merged into every step of each grown copy, so a
//! step is checked as its own `config` overlaid with the grow keys ITS kind
//! names. A grow key no step of the task names is refused at
//! `grow.config.<key>`; a grow key's problems are reported there too.

use super::ConfigKind;
use darkmux_types::user_files::{closest, key_issues, Issue, KeyIssue, COMMENT_KEY};
use serde_json::{Map, Value};

/// Why a `mission.verify` task must not name a role.
const VERIFY_ROLE_REMOVED: &str = "removed in 4.0, it had no effect: `mission.verify` always dispatches the \
     `code-reviewer` role, whatever the task names (#2953). Delete it";

/// Every problem in the step configs of the mission-config document `doc`.
pub fn step_config_issues(doc: &Value) -> Vec<KeyIssue> {
    let mut out = Vec::new();
    for (path, task) in tasks(doc) {
        task_issues(&path, task, &mut out);
    }
    out
}

/// Every `(path, task)` of the document, in document order.
pub(super) fn tasks(doc: &Value) -> Vec<(String, &Value)> {
    let phases = doc.get("phases").and_then(Value::as_array).into_iter().flatten().enumerate();
    phases
        .flat_map(|(p, phase)| {
            let tasks = phase.get("tasks").and_then(Value::as_array).into_iter().flatten().enumerate();
            tasks.map(move |(t, task)| (format!("phases[{p}].tasks[{t}]"), task))
        })
        .collect()
}

/// One task's step configs, its grow keys, and its role.
fn task_issues(path: &str, task: &Value, out: &mut Vec<KeyIssue>) {
    let grow = task.pointer("/grow/config").and_then(Value::as_object);
    let mut kinds = Vec::new();
    let mut every_kind_known = true;
    let steps = task.get("steps").and_then(Value::as_array).into_iter().flatten().enumerate();
    for (i, step) in steps {
        let step_path = format!("{path}.steps[{i}]");
        match step_kind(step, &step_path, out) {
            Some(kind) => {
                kinds.push(kind);
                out.extend(step_issues(&step_path, step, kind, grow, path));
            }
            None => every_kind_known = false,
        }
    }
    if let (Some(grow), true) = (grow, every_kind_known) {
        out.extend(unnamed_grow_keys(path, grow, &kinds));
    }
    if kinds.contains(&ConfigKind::MissionVerify) && task.get("role_id").is_some() {
        out.push(KeyIssue { path: format!("{path}.role_id"), issue: Issue::Removed(VERIFY_ROLE_REMOVED.to_string()) });
    }
}

/// The kind a step names, or the issue that it names none darkmux ships. A
/// step with no `kind` string is the document walk's to report.
fn step_kind(step: &Value, step_path: &str, out: &mut Vec<KeyIssue>) -> Option<ConfigKind> {
    let kind = step.get("kind")?;
    match kind.as_str().and_then(ConfigKind::from_id) {
        Some(known) => Some(known),
        None => {
            out.extend(key_issues::<ConfigKind>(kind, &|_| None).into_iter().map(|mut i| {
                i.path = format!("{step_path}.kind");
                i
            }));
            None
        }
    }
}

/// One step's issues: its `config` overlaid with the grow keys its kind
/// names, checked against the kind's struct.
fn step_issues(step_path: &str, step: &Value, kind: ConfigKind, grow: Option<&Map<String, Value>>, task_path: &str) -> Vec<KeyIssue> {
    let config_path = format!("{step_path}.config");
    let named = kind.keys();
    let from_grow: Vec<&String> = grow.into_iter().flat_map(|g| g.keys()).filter(|k| named.contains(k)).collect();
    let mut overlay = step.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
    for key in &from_grow {
        overlay.insert((*key).clone(), grow.and_then(|g| g.get(*key)).cloned().unwrap_or(Value::Null));
    }
    let grow_path = format!("{task_path}.grow.config");
    kind.issues(&Value::Object(overlay), &config_path)
        .into_iter()
        .map(|issue| relocate(issue, &config_path, &grow_path, &from_grow))
        .collect()
}

/// An issue about a key that came from `grow.config` is reported there, not
/// under the step whose overlay it was found in.
fn relocate(mut issue: KeyIssue, config_path: &str, grow_path: &str, from_grow: &[&String]) -> KeyIssue {
    let under = |key: &String| {
        let at = format!("{config_path}.{key}");
        issue.path.strip_prefix(&at).filter(|rest| rest.is_empty() || rest.starts_with(['.', '['])).map(str::to_string)
    };
    if let Some((key, rest)) = from_grow.iter().find_map(|k| under(k).map(|rest| (k, rest))) {
        issue.path = format!("{grow_path}.{key}{rest}");
    }
    issue
}

/// The grow keys no step of the task reads: each with the closest key any of
/// the task's kinds names.
fn unnamed_grow_keys(path: &str, grow: &Map<String, Value>, kinds: &[ConfigKind]) -> Vec<KeyIssue> {
    let mut valid: Vec<String> = kinds.iter().flat_map(|k| k.keys()).collect();
    valid.sort();
    valid.dedup();
    grow.keys()
        .filter(|k| k.as_str() != COMMENT_KEY && !valid.contains(k))
        .map(|key| KeyIssue {
            path: format!("{path}.grow.config.{key}"),
            issue: Issue::Unknown {
                closest: closest(key, valid.iter().map(String::as_str)).map(|c| format!("{path}.grow.config.{c}")),
                valid: valid.clone(),
            },
        })
        .collect()
}
