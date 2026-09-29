//! The configs a step kind's readers are tried with: a sample per kind, and
//! every variation of it the gate accepts. The gate/load agreement test in
//! this crate and the reader tests of the crates that own kinds share it, so
//! "whatever the gate accepts, the kind's own reader accepts" is swept the
//! same way everywhere.

use super::ConfigKind;
use serde_json::{json, Value};

/// A config each kind's gate and load both accept, using every key shape the
/// kind reads (numbers as text, a `{{param}}`, an open value).
pub fn sample(kind: ConfigKind) -> Value {
    match kind {
        ConfigKind::DispatchInternal => json!({"role_id": "coder", "message": "m", "timeout_seconds": 3,
            "brief_refs": [{"kind": "finding", "key": "k"}], "json": true, "skip_preflight": "false"}),
        ConfigKind::DispatchSingleShot => json!({"model": "m", "user": "u", "max_tokens": "100", "temperature": 0.5, "endpoint": "hosted"}),
        ConfigKind::DispatchMap => json!({"model": "m", "user_template": "{item}", "collection": [1, {"a": 2}], "retry_on_empty": 1}),
        ConfigKind::ProceduralShell => json!({"command": "true"}),
        ConfigKind::ProceduralNoop => json!({"output": "x"}),
        ConfigKind::ModsGate => json!({"for_key": "k", "test_command": "t"}),
        ConfigKind::RecordsGather => json!({"diff_file": "d", "not_attempted": ["r"]}),
        ConfigKind::DeliverGithubReview => json!({"emit": "-", "head_sha": "s", "findings": [], "mods": [], "diff": "", "scope": {}}),
        ConfigKind::CrawlPlan => json!({"rule": "r", "workspace": "w", "sizing": {"max_sites_per_unit": "{{n}}"}, "no_fetch": "{{f}}"}),
        ConfigKind::PlanSites => json!({"rule": "r", "source": "diff", "diff_file": "d", "github": "o/r", "head_sha": "s"}),
        ConfigKind::CrawlUnit => json!({"plan": "p", "unit": "u", "draws": "2", "timeout_seconds": ""}),
        ConfigKind::MissionCoder => json!({"timeout_seconds": 5, "image": null, "injected_budget_chars": 100}),
        ConfigKind::CrawlSummary | ConfigKind::MissionWorktree | ConfigKind::MissionVerify => json!({}),
    }
}

/// `doc` as a kind sees it: `{{param}}` values are substituted before a kind loads.
pub fn substituted(doc: &Value) -> Value {
    let text = doc.to_string().replace("\"{{n}}\"", "\"5\"").replace("\"{{f}}\"", "\"true\"").replace("\"{{p}}\"", "\"1\"");
    serde_json::from_str(&text).unwrap()
}

/// Keys whose value has a shape the text `1` a reference is substituted with
/// does not have: an open value (a list of records, a scope object, which a
/// launch may substitute whole), a `github` reference (`owner/repo`) and a
/// `rule` (a path component).
const SHAPED_VALUE_KEYS: [&str; 5] = ["findings", "mods", "scope", "github", "rule"];

/// Every config the gate accepts that is a sample of `kind` with one key given
/// each of several values, or removed, with its references substituted as a
/// launch would. Each comes with what was varied, for a failure message.
pub fn gate_accepted(kind: ConfigKind) -> Vec<(String, Value)> {
    let candidates = [
        Some(json!(0)), Some(json!(1)), Some(json!(9)), Some(json!("a")), Some(json!("")), Some(json!("  ")), Some(json!("7")),
        Some(json!("0")), Some(json!("true")), Some(json!("diff")), Some(json!(true)), Some(json!(null)), Some(json!([1])),
        Some(json!({"a": 1})), Some(json!(-1)), Some(json!(1.5)), Some(json!("{{p}}")), Some(json!("n={{p}}")), None,
    ];
    let mut accepted = vec![("the sample".to_string(), substituted(&sample(kind)))];
    for key in kind.keys() {
        for value in &candidates {
            if matches!(value, Some(Value::String(t)) if t.contains("{{")) && SHAPED_VALUE_KEYS.contains(&key.as_str()) {
                continue;
            }
            let mut doc = sample(kind);
            match value {
                Some(v) => doc.as_object_mut().unwrap().insert(key.clone(), v.clone()),
                None => doc.as_object_mut().unwrap().remove(&key),
            };
            if kind.problems(&doc, "config").is_empty() {
                accepted.push((format!("{key}={value:?}"), substituted(&doc)));
            }
        }
    }
    accepted
}
