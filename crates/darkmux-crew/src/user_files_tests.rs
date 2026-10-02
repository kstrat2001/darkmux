//! The unknown-key gate over the crew-owned user files: for each kind, an
//! unknown top-level key, an unknown nested key and a near-miss typo are
//! refused by every preflight that consumes the kind, naming the closest
//! valid key; the shipped documents pass; loading still works.

use super::*;
use darkmux_types::test_isolation::IsolatedState;
use darkmux_types::user_files::{key_issues, no_retired, open_objects, Problem, Reach};
use serde_json::{json, Value};

fn embedded(table: &[(&str, &str)], id: &str) -> Value {
    let (_, text) = table.iter().find(|(i, _)| *i == id).unwrap_or_else(|| panic!("no builtin {id}"));
    serde_json::from_str(text).unwrap()
}

fn with_key(mut doc: Value, pointer: &str, key: &str) -> Value {
    doc.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert(key.into(), json!(1));
    doc
}

/// One file kind: where its user files live under the isolated root, a
/// clean document, and three probes of `(document, key path, closest)`.
struct KindCase {
    kind: UserFileKind,
    subdir: &'static str,
    clean: Value,
    probes: Vec<(Value, &'static str, &'static str)>,
}

fn cases() -> Vec<KindCase> {
    let role = embedded(crate::loader::BUILTIN_ROLES, "code-reviewer");
    let skill = embedded(crate::loader::BUILTIN_SKILLS, "coding");
    let crew = json!({"id": "c", "description": "d", "members": [{"role_id": "coder", "position": "lead"}]});
    let mission = embedded(crate::mission_config::load::EMBEDDED_MISSION_CONFIGS, "machine-status");
    let rule = embedded(crate::rules::EMBEDDED_RULES, "existing-solution");
    vec![
        KindCase {
            kind: UserFileKind::Role,
            subdir: "roles",
            probes: vec![
                (with_key(role.clone(), "", "zzz_bogus"), "zzz_bogus", ""),
                (with_key(role.clone(), "/tool_palette", "alow"), "tool_palette.alow", "tool_palette.allow"),
                (with_key(role.clone(), "", "skils"), "skils", "skills"),
            ],
            clean: role,
        },
        KindCase {
            kind: UserFileKind::Skill,
            subdir: "skills",
            probes: vec![
                (with_key(skill.clone(), "", "zzz_bogus"), "zzz_bogus", ""),
                (with_key(skill.clone(), "/keywords/0", "wieght"), "keywords[0].wieght", "keywords[0].weight"),
                (with_key(skill.clone(), "", "descripton"), "descripton", "description"),
            ],
            clean: skill,
        },
        KindCase {
            kind: UserFileKind::Crew,
            subdir: "crews",
            probes: vec![
                (with_key(crew.clone(), "", "zzz_bogus"), "zzz_bogus", ""),
                (with_key(crew.clone(), "/members/0", "rol_id"), "members[0].rol_id", "members[0].role_id"),
                (with_key(crew.clone(), "", "member"), "member", "members"),
            ],
            clean: crew,
        },
        KindCase {
            kind: UserFileKind::MissionConfig,
            subdir: "mission-configs",
            probes: vec![
                (with_key(mission.clone(), "", "zzz_bogus"), "zzz_bogus", ""),
                (with_key(mission.clone(), "/phases/0/tasks/0", "stpes"), "phases[0].tasks[0].stpes", "phases[0].tasks[0].steps"),
                (with_key(mission.clone(), "", "phase"), "phase", "phases"),
                (
                    with_key(mission.clone(), "/phases/0/tasks/0/steps/0/config", "comand"),
                    "phases[0].tasks[0].steps[0].config.comand",
                    "phases[0].tasks[0].steps[0].config.command",
                ),
            ],
            clean: mission,
        },
        KindCase {
            kind: UserFileKind::Rule,
            subdir: "rules",
            probes: vec![
                (with_key(rule.clone(), "", "zzz_bogus"), "zzz_bogus", ""),
                (with_key(rule.clone(), "/search", "notee"), "search.notee", "search.note"),
                (with_key(rule.clone(), "", "titel"), "titel", "title"),
            ],
            clean: rule,
        },
    ]
}

fn write(state: &IsolatedState, subdir: &str, doc: &Value) -> std::path::PathBuf {
    let dir = state.join(subdir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("probe.json");
    std::fs::write(&path, serde_json::to_string_pretty(doc).unwrap()).unwrap();
    path
}

#[test]
#[serial_test::serial]
fn each_kind_refuses_an_unknown_key_at_every_consuming_preflight_naming_the_closest() {
    for case in cases() {
        for (doc, key, closest) in &case.probes {
            let state = IsolatedState::new();
            let path = write(&state, case.subdir, doc);
            let found = problems(case.kind, Reach::Every);
            assert_eq!(found.len(), 1, "{:?} {key}: {found:?}", case.kind);
            let msg = found[0].to_string();
            assert!(msg.contains(&path.display().to_string()), "names the file: {msg}");
            assert!(msg.contains(&format!("unknown key `{key}`")), "names the key: {msg}");
            if closest.is_empty() {
                assert!(msg.contains(": did you mean `"), "names a suggestion even for an unrelated key: {msg}");
            } else {
                assert!(msg.contains(&format!("did you mean `{closest}`?")), "names the closest: {msg}");
            }
            for scope in Scope::ALL {
                let refused = preflight(scope).err().map(|r| r.to_string());
                if case.kind.scopes().contains(&scope) {
                    let r = refused.unwrap_or_else(|| panic!("{:?} must be refused at {scope:?}", case.kind));
                    assert!(r.contains(&format!("unknown key `{key}`")), "{r}");
                } else {
                    assert_eq!(refused, None, "{:?} is not consumed at {scope:?}", case.kind);
                }
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn clean_documents_pass_and_nothing_is_refused() {
    let state = IsolatedState::new();
    for case in cases() {
        write(&state, case.subdir, &case.clean);
        assert_eq!(problems(case.kind, Reach::Every), vec![], "{:?}", case.kind);
    }
    for scope in Scope::ALL {
        assert_eq!(preflight(scope), Ok(()), "{scope:?}");
    }
}

/// Loading never crashes on an unknown key: the loaders still read the file.
#[test]
#[serial_test::serial]
fn a_file_with_an_unknown_key_still_loads() {
    let state = IsolatedState::new();
    let mut role = embedded(crate::loader::BUILTIN_ROLES, "code-reviewer");
    role["id"] = json!("probe");
    write(&state, "roles", &with_key(role, "", "skils"));
    let roles = crate::loader::load_roles().unwrap();
    assert!(roles.iter().any(|r| r.id == "probe"), "the role still loads");
    let mission = with_key(embedded(crate::mission_config::load::EMBEDDED_MISSION_CONFIGS, "machine-status"), "", "phase");
    std::fs::create_dir_all(state.join("mission-configs")).unwrap();
    std::fs::write(state.join("mission-configs/probe-mc.json"), mission.to_string()).unwrap();
    assert!(crate::mission_config::load::load("probe-mc").is_ok());
}

#[test]
#[serial_test::serial]
fn a_syntax_error_is_reported_not_skipped() {
    let state = IsolatedState::new();
    std::fs::create_dir_all(state.join("roles")).unwrap();
    std::fs::write(state.join("roles/broken.json"), "{\"id\": ").unwrap();
    let found = problems(UserFileKind::Role, Reach::Every);
    assert!(matches!(found.as_slice(), [p] if matches!(p.problem, Problem::NotJson(_))), "{found:?}");
    assert!(preflight(Scope::Dispatch).is_err());
}

/// What darkmux ships must never be refused: every embedded role, skill,
/// mission config and rule, as an operator's copy of it would be read.
#[test]
fn every_shipped_document_has_no_unknown_keys() {
    fn check<T: schemars::JsonSchema + 'static>(table: &[(&str, &str)]) {
        for (id, text) in table {
            let doc: Value = serde_json::from_str(text).unwrap();
            assert_eq!(key_issues::<T>(&doc, &no_retired), vec![], "builtin {id}");
        }
    }
    check::<Role>(crate::loader::BUILTIN_ROLES);
    check::<Skill>(crate::loader::BUILTIN_SKILLS);
    check::<MissionConfig>(crate::mission_config::load::EMBEDDED_MISSION_CONFIGS);
    check::<Rule>(crate::rules::EMBEDDED_RULES);
}

#[test]
fn no_crew_file_object_accepts_keys_it_does_not_name() {
    assert_eq!(open_objects::<Role>(), Vec::<String>::new());
    assert_eq!(open_objects::<Skill>(), Vec::<String>::new());
    assert_eq!(open_objects::<Crew>(), Vec::<String>::new());
    assert_eq!(open_objects::<MissionConfig>(), Vec::<String>::new());
    assert_eq!(open_objects::<Rule>(), Vec::<String>::new());
    assert_eq!(open_objects::<crate::workspace_spec::WorkspaceSpec>(), Vec::<String>::new());
}

/// A mission config's retired keys name what replaced them rather than a
/// near-miss guess.
#[test]
#[serial_test::serial]
fn a_retired_mission_config_key_names_its_replacement() {
    let state = IsolatedState::new();
    let mut mission = embedded(crate::mission_config::load::EMBEDDED_MISSION_CONFIGS, "machine-status");
    mission["gh_verb"] = json!("pr-merge");
    mission["phases"][0]["tasks"][0]["expand"] = json!({"over": "items"});
    write(&state, "mission-configs", &mission);
    let msg = problems(UserFileKind::MissionConfig, Reach::Every).iter().map(ToString::to_string).collect::<String>();
    assert!(msg.contains("unknown key `gh_verb`: RENAMED to `cmd` in schema 3.0"), "{msg}");
    assert!(msg.contains("unknown key `phases[0].tasks[0].expand`: REMOVED in schema 2.0"), "{msg}");
}

/// A user role with one mistyped value fails its typed load, and the loader
/// skips it, so the BUILTIN role of the same id silently runs in its place.
/// The gate refuses it at every consuming preflight, naming the path, the
/// expected type and what it got.
#[test]
#[serial_test::serial]
fn a_mistyped_value_is_refused_where_the_loader_would_fall_back_to_the_builtin() {
    let state = IsolatedState::new();
    let mut role = embedded(crate::loader::BUILTIN_ROLES, "code-reviewer");
    role["skills"] = json!("code-reviewing");
    write(&state, "roles", &role);
    let loaded = crate::loader::load_roles().unwrap();
    let reviewer = loaded.iter().find(|r| r.id == "code-reviewer").unwrap();
    assert!(!reviewer.skills.is_empty() && reviewer.prompt_path.is_none(), "precondition: the builtin stood in");
    for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun] {
        let refusal = preflight(scope).expect_err("refused").to_string();
        assert!(refusal.contains("`skills` must be a list, got \"code-reviewing\""), "{scope:?}: {refusal}");
    }
}

/// A user rule is a partial override: leaving out a required key is how it
/// keeps the embedded value, so the gate does not report it.
#[test]
#[serial_test::serial]
fn a_partial_rule_override_is_not_missing_keys() {
    let state = IsolatedState::new();
    write(&state, "rules", &json!({"id": "existing-solution", "window": 40}));
    assert_eq!(problems(UserFileKind::Rule, Reach::Every), vec![]);
}

/// (review C3) Every role key a released darkmux had and this one does not,
/// from `git log`, is named as retired.
#[test]
#[serial_test::serial]
fn every_historical_role_key_is_named_as_retired() {
    let state = IsolatedState::new();
    let mut role = embedded(crate::loader::BUILTIN_ROLES, "code-reviewer");
    role["capabilities"] = json!(["code-reviewing"]);
    role["tier"] = json!("large");
    role["escalation_posture"] = json!("pause");
    write(&state, "roles", &role);
    let msg = problems(UserFileKind::Role, Reach::Every).iter().map(ToString::to_string).collect::<String>();
    assert!(msg.contains("unknown key `capabilities`: renamed to `skills`"), "{msg}");
    assert!(msg.contains("unknown key `tier`: removed in #605"), "{msg}");
    assert!(msg.contains("unknown key `escalation_posture`: removed in 4.0, it had no effect"), "{msg}");
}

/// (review C4) A mission-config copy another tier shadows is never loaded,
/// so the launch preflight does not refuse over it; the effective copy is
/// what counts. Doctor still names the shadowed copy, and says so.
#[test]
#[serial_test::serial]
fn a_shadowed_mission_config_does_not_block_a_launch() {
    let state = IsolatedState::new();
    let tpl = state.join("tpl");
    std::fs::create_dir_all(tpl.join("mission-configs")).unwrap();
    unsafe { std::env::set_var("DARKMUX_TEMPLATES_DIR", &tpl) };
    let good = embedded(crate::mission_config::load::EMBEDDED_MISSION_CONFIGS, "machine-status");
    let bad = with_key(good.clone(), "", "phase");
    std::fs::write(tpl.join("mission-configs/machine-status.json"), bad.to_string()).unwrap();
    // The user tier holds a clean copy of the same id: it wins, the template never loads.
    write_named(&state, "mission-configs", "machine-status", &good);
    let launch = preflight(Scope::MissionLaunch);
    let every = problems(UserFileKind::MissionConfig, Reach::Every);
    // The shadowing copy removed: the bad one is the effective copy now.
    std::fs::remove_file(state.join("mission-configs/machine-status.json")).unwrap();
    let unshadowed = preflight(Scope::MissionLaunch).err().map(|r| r.to_string());
    unsafe { std::env::remove_var("DARKMUX_TEMPLATES_DIR") };
    assert_eq!(launch, Ok(()), "a shadowed copy must not block the launch");
    assert_eq!(every.len(), 1, "{every:?}");
    let msg = every[0].to_string();
    assert!(msg.contains("shadowed by") && msg.contains("never loaded"), "doctor says why it does not refuse: {msg}");
    assert!(unshadowed.is_some_and(|r| r.contains("unknown key `phase`")), "the effective copy is still refused");
}

fn write_named(state: &IsolatedState, subdir: &str, id: &str, doc: &Value) {
    let dir = state.join(subdir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), doc.to_string()).unwrap();
}

/// A hook rule in a retired action spelling stops nothing (#3036): a
/// dispatch-scope preflight passes with the stale rule present
/// (`HOOK_RULE_NO_SCOPE`), and the sink loads it as a rule that matches no
/// action (it warns at load and `darkmux doctor` names it).
#[test]
#[serial_test::serial]
fn a_stale_hook_rule_stops_neither_the_sink_nor_the_dispatch() {
    let state = IsolatedState::new();
    // flow-action-guard:allow-start — an old spelling is this test's input
    let config = json!({"hooks": {"enabled": true, "rules": [
        {"match": {"action": "dispatch complete"}, "http": "http://127.0.0.1:8790/events"}
    ]}});
    // flow-action-guard:allow-end
    let _config = darkmux_types::config_access::set_config_for_test(serde_json::from_value(config).unwrap());
    let rules = darkmux_types::config_access::hooks_rules();
    assert_eq!(rules.len(), 1, "the fixture must reach the resolver");
    for scope in Scope::ALL {
        assert_eq!(preflight(scope), Ok(()), "{scope:?}: a stale hook rule must not stop work");
    }
    let sink = darkmux_flow::hooks::HookSink::new(&rules, state.join("outbox"), std::sync::Arc::new(darkmux_flow::LocalFileSink));
    assert!(sink.is_ok(), "the sink loads: the rule just matches nothing");
}
