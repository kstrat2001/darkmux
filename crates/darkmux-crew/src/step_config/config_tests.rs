use super::gate::step_config_issues;
use super::*;
use serde_json::{json, Value};

fn shipped() -> Vec<(String, Value)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/builtin/mission-configs");
    let mut docs: Vec<(String, Value)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()))
        .collect();
    docs.sort_by(|a, b| a.0.cmp(&b.0));
    docs
}

fn rendered(issues: &[darkmux_types::user_files::KeyIssue]) -> Vec<String> {
    issues.iter().map(|i| i.to_string()).collect()
}

#[test]
fn every_shipped_mission_config_passes_the_step_gate() {
    let docs = shipped();
    assert!(docs.len() >= 4, "swept {} shipped configs", docs.len());
    for (name, doc) in docs {
        let issues = step_config_issues(&doc);
        assert!(issues.is_empty(), "{name}: {:#?}", rendered(&issues));
    }
}

/// A config each kind's gate and load both accept, using every key shape the
/// kind reads (numbers as text, a `{{param}}`, an open value).
fn sample(kind: ConfigKind) -> Value {
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

/// The keys a kind's config cannot omit.
fn required(kind: ConfigKind) -> &'static [&'static str] {
    match kind {
        ConfigKind::DispatchSingleShot => &["model"],
        ConfigKind::DispatchMap => &["model", "user_template"],
        ConfigKind::ProceduralShell => &["command"],
        ConfigKind::ModsGate => &["for_key"],
        ConfigKind::CrawlPlan => &["rule", "workspace"],
        ConfigKind::PlanSites => &["rule"],
        ConfigKind::CrawlUnit => &["plan", "unit"],
        _ => &[],
    }
}

/// `doc` as a kind sees it: `{{param}}` values are substituted before a kind loads.
fn substituted(doc: &Value) -> Value {
    let text = doc.to_string().replace("\"{{n}}\"", "\"5\"").replace("\"{{f}}\"", "\"true\"");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn every_kind_has_a_sample_the_gate_and_the_load_accept() {
    for kind in ConfigKind::ALL {
        let doc = sample(kind);
        assert_eq!(rendered(&kind.issues(&doc, "config")), Vec::<String>::new(), "{}", kind.id());
        assert_eq!(kind.loads(&substituted(&doc)), Ok(()), "{}", kind.id());
    }
}

#[test]
fn a_misspelled_key_is_refused_naming_the_file_path_and_the_closest_key() {
    for kind in ConfigKind::ALL {
        let Some(key) = kind.keys().into_iter().find(|k| k.len() > 3) else { continue };
        let mut doc = sample(kind);
        doc.as_object_mut().unwrap().insert(format!("{key}x"), json!(1));
        let issues = kind.issues(&doc, "steps[0].config");
        let hit = issues.iter().find(|i| i.path == format!("steps[0].config.{key}x")).unwrap_or_else(|| panic!("{}: {:?}", kind.id(), rendered(&issues)));
        let text = hit.to_string();
        assert!(text.contains(&format!("did you mean `steps[0].config.{key}`")) || text.contains("did you mean"), "{}: {text}", kind.id());
    }
}

#[test]
fn a_kind_that_reads_no_config_refuses_every_key() {
    for kind in [ConfigKind::CrawlSummary, ConfigKind::MissionWorktree, ConfigKind::MissionVerify] {
        let issues = kind.issues(&json!({"role_id": "x"}), "config");
        assert_eq!(rendered(&issues).len(), 1, "{}: {:?}", kind.id(), rendered(&issues));
        assert!(kind.issues(&Value::Null, "config").is_empty(), "a null config is an empty one");
    }
}

#[test]
fn a_missing_required_key_is_refused() {
    for kind in ConfigKind::ALL {
        for key in required(kind) {
            let mut doc = sample(kind);
            doc.as_object_mut().unwrap().remove(*key);
            let issues = rendered(&kind.issues(&doc, "config"));
            assert!(issues.iter().any(|i| i.contains(&format!("missing required key `config.{key}`"))), "{} {key}: {issues:?}", kind.id());
        }
    }
}

#[test]
fn a_wrong_type_is_refused_before_anything_runs() {
    let cases = [
        (ConfigKind::DispatchInternal, "timeout_seconds", json!("soon")),
        (ConfigKind::DispatchInternal, "json", json!("maybe")),
        (ConfigKind::DispatchInternal, "brief_refs", json!([{"kind": "note", "key": "k"}])),
        (ConfigKind::DispatchSingleShot, "max_tokens", json!(-3)),
        (ConfigKind::DispatchMap, "collection", json!("not a list")),
        (ConfigKind::ProceduralShell, "command", json!(["true"])),
        (ConfigKind::PlanSites, "source", json!("both")),
        (ConfigKind::CrawlPlan, "no_fetch", json!("sometimes")),
        (ConfigKind::CrawlUnit, "draws", json!("many")),
        (ConfigKind::MissionCoder, "injected_budget_chars", json!(true)),
    ];
    for (kind, key, bad) in cases {
        let mut doc = sample(kind);
        doc.as_object_mut().unwrap().insert(key.to_string(), bad.clone());
        let issues = rendered(&kind.issues(&doc, "config"));
        assert!(issues.iter().any(|i| i.contains(&format!("config.{key}"))), "{} {key}={bad}: {issues:?}", kind.id());
        assert!(kind.loads(&doc).is_err(), "{} {key}={bad}: the load must refuse it too", kind.id());
    }
}

/// The promise's second half: whatever the gate accepts, the kind's own load
/// accepts. Every key of every sample takes each of several values; a
/// value the gate passes must load.
#[test]
fn whatever_the_step_gate_passes_the_kinds_load_accepts() {
    let candidates = [json!(1), json!("a"), json!("7"), json!("true"), json!(true), json!(null), json!([1]), json!({"a": 1}), json!(-1), json!(1.5)];
    let mut disagreements = Vec::new();
    for kind in ConfigKind::ALL {
        for key in kind.keys() {
            for value in &candidates {
                let mut doc = sample(kind);
                doc.as_object_mut().unwrap().insert(key.clone(), value.clone());
                if kind.issues(&doc, "config").is_empty() && kind.loads(&substituted(&doc)).is_err() {
                    disagreements.push(format!("{} {key}={value}: {:?}", kind.id(), kind.loads(&substituted(&doc))));
                }
            }
        }
    }
    assert!(disagreements.is_empty(), "{disagreements:#?}");
}

fn doc_with_step(kind: &str, config: Value) -> Value {
    json!({"id": "d", "name": "d", "phases": [{"id": "p", "tasks": [{"id": "t", "steps": [{"id": "s", "kind": kind, "config": config}]}]}]})
}

#[test]
fn a_typo_inside_a_step_config_is_refused_at_its_full_path_with_the_closest_key() {
    let doc = doc_with_step("procedural.shell", json!({"comand": "true"}));
    let text = rendered(&step_config_issues(&doc)).join("\n");
    assert!(text.contains("unknown key `phases[0].tasks[0].steps[0].config.comand`"), "{text}");
    assert!(text.contains("did you mean `phases[0].tasks[0].steps[0].config.command`?"), "{text}");
    assert!(text.contains("missing required key `phases[0].tasks[0].steps[0].config.command`"), "{text}");
}

#[test]
fn an_unknown_step_kind_is_refused_naming_the_kinds_darkmux_ships() {
    let text = rendered(&step_config_issues(&doc_with_step("dispatch.intrnal", json!({})))).join("\n");
    assert!(text.contains("`phases[0].tasks[0].steps[0].kind`"), "{text}");
    assert!(text.contains("dispatch.internal"), "{text}");
}

fn grown(step_config: Value, grow_config: Value) -> Value {
    json!({"id": "d", "name": "d", "phases": [{"id": "p", "tasks": [{"id": "t",
        "grow": {"from": "x", "items": "i", "id": "{{item.id}}", "config": grow_config},
        "steps": [{"id": "s", "kind": "crawl.unit", "config": step_config}]}]}]})
}

#[test]
fn a_grown_step_is_checked_with_its_grow_keys() {
    let ok = grown(json!({}), json!({"plan": "{{from.output}}", "unit": "{{item.id}}", "draws": "{{draws}}"}));
    assert_eq!(rendered(&step_config_issues(&ok)), Vec::<String>::new());
    let unnamed = grown(json!({}), json!({"plan": "p", "unit": "u", "drawz": 2}));
    let text = rendered(&step_config_issues(&unnamed)).join("\n");
    assert!(text.contains("unknown key `phases[0].tasks[0].grow.config.drawz`"), "{text}");
    assert!(text.contains("did you mean `phases[0].tasks[0].grow.config.draws`"), "{text}");
    let bad = grown(json!({}), json!({"plan": "p", "unit": "u", "draws": "many"}));
    let text = rendered(&step_config_issues(&bad)).join("\n");
    assert!(text.contains("`phases[0].tasks[0].grow.config.draws` must be"), "a grow key's own problem is reported at the grow key: {text}");
    let missing = grown(json!({}), json!({"plan": "p"}));
    assert!(rendered(&step_config_issues(&missing)).join("\n").contains("missing required key"), "grow does not supply `unit`");
}

#[test]
fn a_grow_key_a_sibling_step_reads_is_not_refused_on_this_step() {
    let doc = json!({"id": "d", "name": "d", "phases": [{"id": "p", "tasks": [{"id": "t",
        "grow": {"from": "x", "items": "i", "id": "{{item.id}}", "config": {"command": "true", "for_key": "k"}},
        "steps": [{"id": "a", "kind": "procedural.shell", "config": {}}, {"id": "b", "kind": "mods.gate", "config": {}}]}]}]});
    assert_eq!(rendered(&step_config_issues(&doc)), Vec::<String>::new());
}

#[test]
fn a_verify_task_naming_a_role_is_refused() {
    let doc = json!({"id": "d", "name": "d", "phases": [{"id": "p", "tasks": [
        {"id": "t", "role_id": "code-reviewer", "steps": [{"id": "s", "kind": "mission.verify"}]},
        {"id": "u", "role_id": "coder", "steps": [{"id": "c", "kind": "mission.coder"}]}]}]});
    let text = rendered(&step_config_issues(&doc)).join("\n");
    assert!(text.contains("`phases[0].tasks[0].role_id`: removed in 4.0"), "{text}");
    assert!(!text.contains("tasks[1]"), "the coder task's role is honored: {text}");
}

/// `value` with every whole-string `{{param}}` replaced by the text `1`,
/// which reads as a string, a count and a flag, as a launch's substitution
/// yields for a `--param` value.
fn with_params_filled(value: &Value) -> Value {
    match value {
        Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => json!("1"),
        Value::Array(items) => Value::Array(items.iter().map(with_params_filled).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), with_params_filled(v))).collect()),
        other => other.clone(),
    }
}

/// The gate's promise on the shipped documents: every step config the gate
/// passes (grow keys included) is one its kind loads.
#[test]
fn every_shipped_step_config_loads_once_its_params_are_substituted() {
    let mut checked = 0;
    for (name, doc) in shipped() {
        for (path, task) in gate::tasks(&doc) {
            let grow = task.pointer("/grow/config").and_then(Value::as_object);
            for step in task.get("steps").and_then(Value::as_array).into_iter().flatten() {
                let kind = ConfigKind::from_id(step["kind"].as_str().unwrap()).unwrap();
                let mut config = step.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
                let named = kind.keys();
                for (key, value) in grow.into_iter().flatten().filter(|(k, _)| named.contains(k)) {
                    config.insert(key.clone(), value.clone());
                }
                let loaded = kind.loads(&with_params_filled(&Value::Object(config)));
                assert_eq!(loaded, Ok(()), "{name} {path} step {} ({})", step["id"], kind.id());
                checked += 1;
            }
        }
    }
    assert!(checked > 30, "checked {checked} shipped steps");
}
