use super::gate::step_config_issues;
use super::sweep::{self, sample, substituted};
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

#[test]
fn every_kind_has_a_sample_the_gate_and_the_load_accept() {
    for kind in ConfigKind::ALL {
        let doc = sample(kind);
        assert_eq!(rendered(&kind.problems(&doc, "config")), Vec::<String>::new(), "{}", kind.id());
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
    for kind in [ConfigKind::DispatchSummary, ConfigKind::MissionWorktree, ConfigKind::MissionVerify] {
        let issues = kind.issues(&json!({"role_id": "x"}), "config");
        assert_eq!(rendered(&issues).len(), 1, "{}: {:?}", kind.id(), rendered(&issues));
        assert!(kind.issues(&Value::Null, "config").is_empty(), "a null config is an empty one");
    }
}

/// Every key a sample names, removed in turn: a kind that cannot load without
/// the key (or refuses the config by a value rule) is one whose gate refuses
/// the config too. The required keys are DERIVED from what each kind's own
/// reader refuses, not listed here. (A rule can name a related key: without
/// `github`, `plan.sites` names `workspace`.)
#[test]
fn a_config_the_kind_cannot_load_without_a_key_is_refused_naming_that_key() {
    let mut refused_removals = 0;
    for kind in ConfigKind::ALL {
        for key in kind.keys() {
            let mut doc = sample(kind);
            if doc.as_object_mut().unwrap().remove(&key).is_none() || kind.loads(&substituted(&doc)).is_ok() {
                continue;
            }
            let issues = rendered(&kind.problems(&doc, "config"));
            assert!(!issues.is_empty(), "{} without {key} loads no more, and the gate passes it", kind.id());
            refused_removals += 1;
        }
    }
    assert!(refused_removals >= 10, "the derivation found only {refused_removals} required keys");
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
        (ConfigKind::DispatchUnit, "draws", json!("many")),
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
/// (its value rules included) accepts. Every key of every sample takes each of
/// several values, and is removed; a config the gate passes must load.
#[test]
fn whatever_the_step_gate_passes_the_kinds_load_accepts() {
    let mut disagreements = Vec::new();
    for kind in ConfigKind::ALL {
        for (what, config) in sweep::gate_accepted(kind) {
            if let Err(why) = kind.loads(&config) {
                disagreements.push(format!("{} {what}: {why}", kind.id()));
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
    // flow-action-guard:allow — a misspelled step kind id, not an action
    let text = rendered(&step_config_issues(&doc_with_step("dispatch.intrnal", json!({})))).join("\n");
    assert!(text.contains("`phases[0].tasks[0].steps[0].kind`"), "{text}");
    assert!(text.contains("dispatch.internal"), "{text}");
}

fn grown(step_config: Value, grow_config: Value) -> Value {
    json!({"id": "d", "name": "d", "phases": [{"id": "p", "tasks": [{"id": "t",
        "grow": {"from": "x", "items": "i", "id": "{{item.id}}", "config": grow_config},
        "steps": [{"id": "s", "kind": "dispatch.unit", "config": step_config}]}]}]})
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
    assert!(text.contains("`phases[0].tasks[0].role_id`: removed in 5.0"), "{text}");
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

/// One row per rule a kind's own reader enforces that the types alone do not:
/// the config, and the text the refusal must carry.
fn rule_cases() -> Vec<(ConfigKind, Value, &'static str)> {
    let unit = |patch: Value| {
        let mut doc = json!({"plan": "p", "unit": "u"});
        doc.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        doc
    };
    vec![
        (ConfigKind::DispatchUnit, unit(json!({"draws": 0})), "config.draws must be >= 1, got 0"),
        (ConfigKind::DispatchUnit, unit(json!({"draws": 9})), "config.draws is 9, above the cap of 8"),
        (ConfigKind::DispatchUnit, unit(json!({"draws": "9"})), "above the cap of 8"),
        (ConfigKind::DispatchUnit, unit(json!({"timeout_seconds": 0})), "config.timeout_seconds must be >= 1"),
        (ConfigKind::DispatchUnit, unit(json!({"plan": " "})), "config.plan must not be blank"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "diff", "workspace": "w"}), "config.diff_file is required"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "diff", "diff_file": "d"}), "config.workspace is required, or set both config.github and config.head_sha"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "diff", "diff_file": "d", "github": "o/r"}), "config.workspace is required, or set both config.github and config.head_sha"),
        (ConfigKind::PlanSites, json!({"rule": "r"}), "config.workspace is required when `source` is the tree"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "tree", "github": "o/r", "head_sha": "s"}), "config.workspace is required when `source` is the tree"),
        (ConfigKind::PlanSites, json!({"rule": "r", "github": "kstrat2001/darkmux", "head_sha": "abc"}), "config.workspace is required when `source` is the tree"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "diff", "diff_file": "d", "github": "just-a-name", "head_sha": "s"}), "config.github must be `owner/repo` or a GitHub URL"),
        (ConfigKind::PlanSites, json!({"rule": "r", "source": "diff", "diff_file": "d", "github": "a/b/c", "head_sha": "s"}), "config.github must be `owner/repo` or a GitHub URL"),
        (ConfigKind::PlanSites, json!({"rule": "../x", "workspace": "w"}), "config.rule `../x` is not a safe path component"),
        (ConfigKind::CrawlPlan, json!({"rule": "a/b", "workspace": "w"}), "config.rule `a/b` is not a safe path component"),
        (ConfigKind::CrawlPlan, json!({"rule": ".hidden", "workspace": "w"}), "config.rule `.hidden` is not a safe path component"),
        (ConfigKind::DispatchUnit, unit(json!({"rule": "../x"})), "config.rule `../x` is not a safe path component"),
        (ConfigKind::DispatchUnit, unit(json!({"rule": "ok+a/b"})), "config.rule `a/b` is not a safe path component"),
        (ConfigKind::PlanSites, json!({"rule": " ", "workspace": "w"}), "config.rule must not be blank"),
        (ConfigKind::PlanSites, json!({"rule": "r", "workspace": "w", "sizing": {"max_est_tokens_per_unit": 0}}), "config.sizing.max_est_tokens_per_unit must be a positive integer"),
        (ConfigKind::CrawlPlan, json!({"rule": "", "workspace": "w"}), "config.rule must not be blank"),
        (ConfigKind::CrawlPlan, json!({"rule": "r", "workspace": ""}), "config.workspace must not be blank"),
        (ConfigKind::CrawlPlan, json!({"rule": "r", "workspace": "w", "sizing": {"max_sites_per_unit": "0"}}), "config.sizing.max_sites_per_unit must be a positive integer"),
        (ConfigKind::DeliverGithubReview, json!({"findings": []}), "config.mods is required when `findings` is set"),
        (ConfigKind::DeliverGithubReview, json!({"findings": [], "mods": []}), "config.diff is required when `findings` is set"),
        (ConfigKind::DeliverGithubReview, json!({"findings": [1], "mods": [], "diff": ""}), "config.findings is not a list of finding records"),
        (ConfigKind::ModsGate, json!({"for_key": " "}), "config.for_key must not be blank"),
        (ConfigKind::DispatchMap, json!({"model": "m", "user_template": "t", "retry_on_error": 4294967296u64}), "config.retry_on_error (4294967296) exceeds the maximum"),
    ]
}

#[test]
fn a_config_the_kinds_own_reader_refuses_by_value_is_refused_by_the_gate_naming_step_key_and_rule() {
    for (kind, config, expect) in rule_cases() {
        let doc = doc_with_step(kind.id(), config.clone());
        let text = rendered(&step_config_issues(&doc)).join("\n");
        assert!(text.contains(expect), "{} {config}: wanted `{expect}` in\n{text}", kind.id());
        assert!(text.contains(&format!("step `s` (`{}`)", kind.id())), "the step is named: {text}");
        assert!(text.contains("phases[0].tasks[0].steps[0].config."), "the key path is named: {text}");
        assert!(kind.loads(&config).is_err(), "{}: the kind's own load refuses it too", kind.id());
    }
}

#[test]
fn a_placeholder_valued_config_is_left_to_the_launch_check_after_substitution() {
    let doc = doc_with_step("dispatch.unit", json!({"plan": "{{plan}}", "unit": "{{unit}}", "draws": "{{draws}}"}));
    assert_eq!(rendered(&step_config_issues(&doc)), Vec::<String>::new());
}

#[test]
fn a_config_that_is_not_an_object_is_refused_not_read_as_empty() {
    for bad in [json!("oops"), json!(5), json!(["hello"]), json!(true)] {
        for kind in [ConfigKind::ProceduralNoop, ConfigKind::DispatchSummary, ConfigKind::DispatchUnit] {
            let text = rendered(&step_config_issues(&doc_with_step(kind.id(), bad.clone()))).join("\n");
            assert!(text.contains("`phases[0].tasks[0].steps[0].config` must be an object"), "{} {bad}: {text}", kind.id());
            assert!(kind.loads(&bad).is_err(), "{} {bad}: the load refuses it too", kind.id());
            assert!(!kind.problems(&bad, "config").is_empty(), "{} {bad}", kind.id());
        }
    }
    let step = crate::types::Step {
        id: "s".into(),
        task_id: "t".into(),
        gate: None,
        kind: "procedural.noop".into(),
        status: crate::types::NodeStatus::Planned,
        config: json!(["hello"]),
        started_ts: None,
        completed_ts: None,
        output: None,
    };
    let err = load::<NoopConfig>(&step, ConfigKind::ProceduralNoop).map(|c| c.output).unwrap_err();
    assert!(err.to_string().contains("must be an object"), "{err}");
    assert!(ConfigKind::ProceduralNoop.loads(&Value::Null).is_ok(), "null stays an empty config");
}

#[test]
fn a_temperature_reads_its_number_and_its_text() {
    let kind = ConfigKind::DispatchSingleShot;
    for temp in [json!(0.5), json!("0.5"), json!("{{temp}}")] {
        let doc = json!({"model": "m", "temperature": temp});
        assert_eq!(rendered(&kind.problems(&doc, "config")), Vec::<String>::new(), "{temp}");
    }
    let bad = json!({"model": "m", "temperature": "warm"});
    assert!(!kind.problems(&bad, "config").is_empty());
    assert!(kind.loads(&json!({"model": "m", "temperature": "0.5"})).is_ok());
    assert!(kind.problems(&json!({"model": "m", "max_tokens": "n={{n}}"}), "config").iter().any(|i| i.path.ends_with("max_tokens")),
        "a reference embedded in text is not a count");
}

/// The values whose treatment changed when the kinds began reading one typed
/// struct: each is pinned so a later change to it is deliberate.
#[test]
fn values_the_typed_load_now_refuses_or_reads_as_unset() {
    let refused = [
        (ConfigKind::RecordsGather, json!({"not_attempted": [1]})),
        (ConfigKind::DeliverGithubReview, json!({"emit": 5})),
        (ConfigKind::DeliverGithubReview, json!({"attribution": 5})),
        (ConfigKind::DeliverGithubReview, json!({"diff": 5})),
        (ConfigKind::DispatchUnit, json!({"plan": "p", "unit": "u", "rule": 5})),
    ];
    for (kind, config) in refused {
        assert!(kind.loads(&config).is_err(), "{} {config}", kind.id());
        assert!(!kind.problems(&config, "config").is_empty(), "{} {config}: the gate refuses it too", kind.id());
    }
    let unset = [
        (ConfigKind::CrawlPlan, json!({"rule": "r", "workspace": "w", "sizing": {"max_sites_per_unit": null}})),
        (ConfigKind::DispatchUnit, json!({"plan": "p", "unit": "u", "no_progress_turns": null})),
        (ConfigKind::DeliverGithubReview, json!({"findings": null})),
    ];
    for (kind, config) in unset {
        assert_eq!(kind.loads(&config), Ok(()), "{} {config}", kind.id());
        assert!(kind.problems(&config, "config").is_empty(), "{} {config}", kind.id());
    }
}

/// A github reference in either accepted spelling, and a rule id that is one
/// path component or several joined by `+`, are configs the gate accepts.
#[test]
fn the_safe_spellings_of_github_and_rule_pass_the_gate() {
    for github in ["o/r", "https://github.com/o/r", "git@github.com:o/r.git", "o/r/"] {
        let doc = json!({"rule": "r", "source": "diff", "diff_file": "d", "github": github, "head_sha": "s"});
        assert_eq!(rendered(&ConfigKind::PlanSites.problems(&doc, "config")), Vec::<String>::new(), "{github}");
    }
    let unit = json!({"plan": "p", "unit": "u", "rule": "unnamed-predicate+swallowed-error"});
    assert_eq!(rendered(&ConfigKind::DispatchUnit.problems(&unit, "config")), Vec::<String>::new());
    let derived_tree = json!({"rule": "r", "source": "diff", "diff_file": "d", "github": "{{github}}", "head_sha": "{{head_sha}}"});
    assert_eq!(rendered(&ConfigKind::PlanSites.problems(&derived_tree, "config")), Vec::<String>::new());
}

/// A rule id with a reference embedded in it is a value not known yet, like a
/// whole-string reference: the launch check judges it once substituted.
#[test]
fn an_embedded_reference_in_a_rule_is_left_to_the_launch_check() {
    let doc = json!({"plan": "p", "unit": "u", "rule": "pre-{{tag}}"});
    assert_eq!(rendered(&ConfigKind::DispatchUnit.problems(&doc, "config")), Vec::<String>::new());
    let resolved = json!({"plan": "p", "unit": "u", "rule": "pre-../x"});
    assert!(!ConfigKind::DispatchUnit.problems(&resolved, "config").is_empty());
}

/// After a launch substitutes its params, every refusal names the step and its
/// kind, whatever the problem: a missing key, a wrong type, or a broken rule.
#[test]
fn a_resolved_step_refusal_names_the_step_and_kind_for_every_problem() {
    let cases = [
        ("mods.gate", json!({}), "step `s1` (`mods.gate`): missing required key `config.for_key`"),
        ("dispatch.unit", json!({"plan": "p", "unit": "u", "draws": "many"}), "step `s1` (`dispatch.unit`): `config.draws` must be"),
        ("mods.gate", json!({"for_key": " "}), "step `s1` (`mods.gate`): `config.for_key`: config.for_key must not be blank"),
        ("procedural.shell", json!({"command": "true", "comand": 1}), "step `s1` (`procedural.shell`): unknown key `config.comand`"),
    ];
    for (kind, config, expect) in cases {
        let err = gate::check_resolved([("s1", kind, &config)]).unwrap_err().to_string();
        assert!(err.contains(expect), "{kind} {config}: wanted `{expect}` in\n{err}");
    }
}

#[test]
fn ids_are_in_declaration_order_and_round_trip() {
    for (i, kind) in ConfigKind::ALL.into_iter().enumerate() {
        assert_eq!(kind as usize, i, "{kind:?} is out of order in ALL");
        assert_eq!(ConfigKind::from_id(kind.id()), Some(kind), "{kind:?} does not round-trip through its id");
    }
    assert_eq!(ConfigKind::IDS.len(), ConfigKind::ALL.len());
}

/// (#3035) `bucket_group` and `bucket_budget` retired with the per-step
/// shared allowance: a `dispatch.map` config naming either is refused, at the
/// gate and at the kind's own load, with a pointer to the endpoint's rolling
/// `limits.window` (how a whole-run budget is expressed now). Neither is
/// read as an unknown key's near-miss, and neither is silently ignored.
#[test]
fn a_retired_bucket_key_is_refused_with_a_pointer_to_the_endpoint_window() {
    for key in ["bucket_group", "bucket_budget"] {
        let mut config = sample(ConfigKind::DispatchMap);
        config.as_object_mut().unwrap().insert(key.to_string(), json!(if key == "bucket_group" { json!("probe") } else { json!(5000) }));
        let doc = doc_with_step("dispatch.map", config.clone());
        let text = rendered(&step_config_issues(&doc)).join("\n");
        assert!(text.contains(&format!("`phases[0].tasks[0].steps[0].config.{key}`")), "{text}");
        assert!(text.contains("endpoints.<id>.limits.window") && text.contains("tokens_per_dispatch"), "{text}");
        let err = ConfigKind::DispatchMap.loads(&substituted(&config)).unwrap_err();
        assert!(err.contains(key) && err.contains("limits.window"), "{key}: {err}");
    }
}
