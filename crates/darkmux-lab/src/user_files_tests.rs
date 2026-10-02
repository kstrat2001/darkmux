//! The unknown-key gate over the lab's user files: a workload document or a
//! registered fixture's manifest with an unknown key is refused by the
//! lab-run preflight (and `lab run` itself, before it mints anything),
//! naming the file, the key and the closest valid key.

use super::*;
use darkmux_types::test_isolation::IsolatedState;
use darkmux_types::user_files::{key_issues, no_retired, open_objects, Reach, UserFileKind};
use serde_json::{json, Value};

fn quick_q() -> Value {
    let (_, text) = crate::workloads::load::EMBEDDED_WORKLOADS.iter().find(|(id, _)| *id == "quick-q").unwrap();
    serde_json::from_str(text).unwrap()
}

fn tiny_fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../templates/builtin/lab-fixtures/demo-tiny-py/.fixture.json"
    )))
    .unwrap()
}

fn with_key(mut doc: Value, pointer: &str, key: &str) -> Value {
    doc.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert(key.into(), json!(1));
    doc
}

fn write_workload(state: &IsolatedState, doc: &Value) -> std::path::PathBuf {
    std::fs::create_dir_all(state.join("workloads")).unwrap();
    let path = state.join("workloads/probe.json");
    std::fs::write(&path, doc.to_string()).unwrap();
    path
}

/// Registers one fixture (its manifest `doc`) in the isolated home-tier
/// registry, returning the manifest's path.
fn register_fixture(state: &IsolatedState, doc: &Value) -> std::path::PathBuf {
    let dir = state.join("fixtures/probe");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join(".fixture.json");
    std::fs::write(&manifest, doc.to_string()).unwrap();
    let registry = json!({"fixtures": {"probe": {
        "path": dir, "content_hash": "x", "hashed_at": "2026-01-01T00:00:00Z", "manifest_version": "1.0",
        "satisfies": PROBE_FIXTURE
    }}});
    std::fs::write(state.join("lab-registry.json"), registry.to_string()).unwrap();
    manifest
}

/// The requirement the probe fixture satisfies, which a workload names to
/// bind it.
const PROBE_FIXTURE: &str = "probe-suite@1.0";

fn assert_refused(msg: &str, path: &std::path::Path, key: &str, closest: &str) {
    assert!(msg.contains(&path.display().to_string()), "names the file: {msg}");
    assert!(msg.contains(&format!("unknown key `{key}`: did you mean `")), "names the key and a suggestion: {msg}");
    if !closest.is_empty() {
        assert!(msg.contains(&format!("did you mean `{closest}`?")), "names the closest: {msg}");
    }
}

#[test]
#[serial_test::serial]
fn a_workload_with_an_unknown_key_is_refused_by_the_lab_preflight() {
    let probes = [
        (with_key(quick_q(), "", "zzz_bogus"), "zzz_bogus", ""),
        (with_key(quick_q(), "/workload/verify", "comand"), "workload.verify.comand", "workload.verify.command"),
        (with_key(quick_q(), "/workload", "promt"), "workload.promt", "workload.prompt"),
    ];
    for (doc, key, closest) in probes {
        let state = IsolatedState::new();
        let path = write_workload(&state, &doc);
        let found = problems(UserFileKind::Workload, Reach::Every);
        assert_eq!(found.len(), 1, "{key}: {found:?}");
        assert_refused(&found[0].to_string(), &path, key, closest);
        let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).expect_err(key).to_string();
        assert_refused(&refusal, &path, key, closest);
        for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::FleetSubmission] {
            assert_eq!(preflight_with(scope, None, None), Ok(()), "a workload is consumed only by a lab run ({scope:?})");
        }
    }
}

#[test]
#[serial_test::serial]
fn a_fixture_manifest_with_an_unknown_key_is_refused_by_the_lab_preflight() {
    // A fixture manifest has no nested typed object (`baseline` is free-form
    // JSON), so its probes are an unknown key and two near-misses.
    let probes = [
        (with_key(tiny_fixture(), "", "zzz_bogus"), "zzz_bogus", ""),
        (with_key(tiny_fixture(), "", "verify_comand"), "verify_comand", "verify_command"),
        (with_key(tiny_fixture(), "", "requried_files"), "requried_files", "required_files"),
    ];
    for (doc, key, closest) in probes {
        let state = IsolatedState::new();
        let path = register_fixture(&state, &doc);
        let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).expect_err(key).to_string();
        assert_refused(&refusal, &path, key, closest);
    }
}

#[test]
#[serial_test::serial]
fn a_retired_key_names_its_removal() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["expected"] = json!({"test_count_baseline": 14});
    write_workload(&state, &wl);
    let mut fx = tiny_fixture();
    fx["hash_exclude"] = json!(["__pycache__"]);
    register_fixture(&state, &fx);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("unknown key `workload.expected.test_count_baseline`: removed in #2833"), "{refusal}");
    assert!(refusal.contains("unknown key `hash_exclude`: removed in #610"), "{refusal}");
}

/// `lab run` refuses a workload with an unknown key before it writes
/// anything, under its own label.
#[test]
#[serial_test::serial]
fn lab_run_refuses_an_unknown_key_before_minting() {
    let state = IsolatedState::new();
    write_workload(&state, &with_key(quick_q(), "/workload", "promt"));
    let err = crate::lab::run::lab_run(crate::lab::run::RunOpts {
        workload_id: "quick-q".into(),
        profile_name: None,
        runs: 1,
        config_path: None,
        quiet: true,
        loop_override: None,
        inject_context: None,
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("lab run: refusing to start") && err.contains("unknown key `workload.promt`"), "{err}");
    let runs = state.join("runs");
    assert!(!runs.exists() || std::fs::read_dir(&runs).unwrap().next().is_none(), "no run directory was minted");
}

#[test]
#[serial_test::serial]
fn clean_files_pass() {
    let state = IsolatedState::new();
    write_workload(&state, &quick_q());
    register_fixture(&state, &tiny_fixture());
    assert_eq!(preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)), Ok(()));
}

/// What darkmux ships must never be refused: every embedded workload and
/// the built-in fixture manifest.
#[test]
fn every_shipped_document_has_no_unknown_keys() {
    for (id, text) in crate::workloads::load::EMBEDDED_WORKLOADS {
        let doc: Value = serde_json::from_str(text).unwrap();
        assert_eq!(key_issues::<WorkloadManifest>(&doc, &no_retired), vec![], "builtin workload {id}");
        assert_eq!(doc["schema_version"], UserFileKind::Workload.schema_version(), "builtin workload {id}");
    }
    assert_eq!(key_issues::<FixtureManifest>(&tiny_fixture(), &no_retired), vec![]);
    assert_eq!(tiny_fixture()["schema_version"], UserFileKind::LabFixture.schema_version());
}

#[test]
fn no_lab_file_object_accepts_keys_it_does_not_name() {
    assert_eq!(open_objects::<WorkloadManifest>(), Vec::<String>::new());
    assert_eq!(open_objects::<FixtureManifest>(), Vec::<String>::new());
}

/// Every document under `templates/builtin/` (an operator's on-disk template
/// tier and the starting point for a user copy) passes the gate, and so do
/// the committed `config.example.json` and `profiles.example.json` that
/// `darkmux init` writes.
#[test]
fn every_shipped_template_file_has_no_unknown_keys() {
    use darkmux_crew::mission_config::MissionConfig;
    use darkmux_crew::rules::Rule;
    use darkmux_crew::types::{Role, Skill};
    use darkmux_types::user_files::{check_dir, check_path, FileProblem};
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let t = repo.join("templates/builtin");
    let mut found: Vec<FileProblem> = Vec::new();
    found.extend(check_dir::<Role>(UserFileKind::Role, &t.join("roles"), &no_retired));
    found.extend(check_dir::<Skill>(UserFileKind::Skill, &t.join("skills"), &no_retired));
    found.extend(check_dir::<MissionConfig>(UserFileKind::MissionConfig, &t.join("mission-configs"), &no_retired));
    found.extend(check_dir::<Rule>(UserFileKind::Rule, &t.join("rules"), &no_retired));
    let workloads = crate::workloads::load::documents_in(&t.join("workloads"));
    assert!(workloads.len() >= 7, "{workloads:?}");
    found.extend(workloads.iter().filter_map(|p| check_path::<WorkloadManifest>(UserFileKind::Workload, p, &no_retired)));
    for fixture in std::fs::read_dir(t.join("lab-fixtures")).unwrap().flatten() {
        found.extend(check_path::<FixtureManifest>(UserFileKind::LabFixture, &fixture.path().join(".fixture.json"), &no_retired));
    }
    found.extend(check_path::<darkmux_types::config::DarkmuxConfig>(
        UserFileKind::Config,
        &repo.join("config.example.json"),
        &no_retired,
    ));
    found.extend(check_path::<darkmux_types::ProfileRegistry>(
        UserFileKind::Profiles,
        &repo.join("profiles.example.json"),
        &no_retired,
    ));
    let lines: Vec<String> = found.iter().map(ToString::to_string).collect();
    assert!(lines.is_empty(), "{lines:#?}");
}

/// A mistyped value in a workload is refused by the lab preflight, naming
/// the path, the expected type and what it got.
#[test]
#[serial_test::serial]
fn a_mistyped_workload_value_is_refused() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["verify"] = json!({"must_contain": "four"});
    write_workload(&state, &wl);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("`workload.verify.must_contain` must be a list, got \"four\""), "{refusal}");
}

// ── (review C1) the gate and the loader never disagree ──

/// Every JSON pointer into `v` (the root is `""`).
fn pointers(v: &Value, at: String, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => m.iter().for_each(|(k, c)| pointers(c, format!("{at}/{}", k.replace('~', "~0").replace('/', "~1")), out)),
        Value::Array(a) => a.iter().enumerate().for_each(|(i, c)| pointers(c, format!("{at}/{i}"), out)),
        _ => {}
    }
    out.push(at);
}

/// `base` and its variants: `_comment` and an unknown key inserted into
/// every object, every key removed (a required one included, root level
/// too), and every value replaced by each sample of every JSON type.
fn variants(base: &Value) -> Vec<Value> {
    let samples = [json!("x"), json!(1), json!(-1), json!(1.5), json!(300), json!(70000), json!(true), json!(null), json!([]), json!({}), json!(["x"]), json!({"_comment": "c"})];
    let mut ptrs = Vec::new();
    pointers(base, String::new(), &mut ptrs);
    let mut out = vec![base.clone()];
    for p in &ptrs {
        if base.pointer(p).is_some_and(Value::is_object) {
            for (k, v) in [("_comment", json!("c")), ("zz_probe", json!(1))] {
                let mut d = base.clone();
                d.pointer_mut(p).unwrap().as_object_mut().unwrap().insert(k.into(), v);
                out.push(d);
            }
        }
        if let Some((parent, key)) = p.rsplit_once('/') {
            if base.pointer(parent).is_some_and(Value::is_object) {
                let mut d = base.clone();
                let key = key.replace("~1", "/").replace("~0", "~");
                d.pointer_mut(parent).unwrap().as_object_mut().unwrap().remove(&key);
                out.push(d);
            }
        }
        if !p.is_empty() {
            for s in &samples {
                let mut d = base.clone();
                *d.pointer_mut(p).unwrap() = s.clone();
                out.push(d);
            }
        }
    }
    out
}

/// Every variant the gate passes, the typed load must accept: if the two
/// ever disagree in that direction, a file the gate calls clean is dropped
/// or reset to defaults at load time, silently.
fn gate_agrees_with_loader<T>(name: &str, base: &Value, failures: &mut Vec<String>)
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + 'static,
{
    // The base must be clean: a refused base leaves only the variants that
    // happen to repair it (a removed key), so most of the input tests
    // nothing, silently.
    let base_issues = key_issues::<T>(base, &no_retired);
    if !base_issues.is_empty() {
        failures.push(format!("{name}: the gate refuses the base itself, nothing exercised: {base_issues:?}"));
    }
    let mut passed = 0;
    for doc in variants(base) {
        if key_issues::<T>(&doc, &no_retired).is_empty() {
            passed += 1;
            if let Err(e) = serde_json::from_value::<T>(doc.clone()) {
                failures.push(format!("{name}: the gate passed it, serde refused it ({e}): {doc}"));
            }
        }
    }
    if passed <= 1 {
        failures.push(format!("{name}: the gate passed {passed} variant(s); the base is refused, nothing exercised"));
    }
}

fn json_files(dir: &std::path::Path) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| (p.display().to_string(), serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn whatever_the_gate_passes_the_typed_load_accepts() {
    use darkmux_crew::mission_config::MissionConfig;
    use darkmux_crew::rules::Rule;
    use darkmux_crew::types::{Crew, Role, Skill};
    use darkmux_crew::workspace_spec::WorkspaceSpec;
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let t = repo.join("templates/builtin");
    let read = |p: std::path::PathBuf| -> Value { serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap() };
    let mut failures = Vec::new();
    gate_agrees_with_loader::<darkmux_types::config::DarkmuxConfig>("config.example.json", &read(repo.join("config.example.json")), &mut failures);
    let defaults = serde_json::to_value(darkmux_types::config::DarkmuxConfig::with_defaults()).unwrap();
    gate_agrees_with_loader::<darkmux_types::config::DarkmuxConfig>("with_defaults", &defaults, &mut failures);
    gate_agrees_with_loader::<darkmux_types::ProfileRegistry>("profiles.example.json", &read(repo.join("profiles.example.json")), &mut failures);
    for (n, d) in json_files(&t.join("roles")) { gate_agrees_with_loader::<Role>(&n, &d, &mut failures); }
    for (n, d) in json_files(&t.join("skills")) { gate_agrees_with_loader::<Skill>(&n, &d, &mut failures); }
    for (n, d) in json_files(&t.join("mission-configs")) { gate_agrees_with_loader::<MissionConfig>(&n, &d, &mut failures); }
    for (n, d) in json_files(&t.join("rules")) { gate_agrees_with_loader::<Rule>(&n, &d, &mut failures); }
    for (n, d) in json_files(&t.join("workloads")) { gate_agrees_with_loader::<WorkloadManifest>(&n, &d, &mut failures); }
    gate_agrees_with_loader::<FixtureManifest>("demo-tiny-py", &tiny_fixture(), &mut failures);
    let crew = json!({"id": "c", "description": "d", "members": [{"role_id": "r", "position": "lead"}]});
    gate_agrees_with_loader::<Crew>("crew", &crew, &mut failures);
    let spec = json!({"name": "w", "include": ["**"], "sources": [{"id": "a", "path": "/x"}], "edges": [{"consumer": "a", "library": "a", "package": "p"}]});
    gate_agrees_with_loader::<WorkspaceSpec>("workspace spec", &spec, &mut failures);
    failures.truncate(20);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// (review C3) Every workload and fixture key a released darkmux had and
/// this one does not, from `git log`, is named as retired.
#[test]
#[serial_test::serial]
fn every_historical_workload_and_fixture_key_is_named_as_retired() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["agent"] = json!("code-reviewer");
    write_workload(&state, &wl);
    let mut fx = tiny_fixture();
    fx["hash_include"] = json!(["src"]);
    register_fixture(&state, &fx);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("unknown key `workload.agent`: renamed to `role`"), "{refusal}");
    assert!(refusal.contains("unknown key `hash_include`: removed in #610"), "{refusal}");
}

/// (review C4) One bad registered fixture must not block every lab run:
/// only the fixture the run binds is checked. Doctor still names it, and
/// says no workload being run needs it.
#[test]
#[serial_test::serial]
fn a_bad_fixture_blocks_only_the_run_that_binds_it() {
    let state = IsolatedState::new();
    write_workload(&state, &quick_q());
    register_fixture(&state, &with_key(tiny_fixture(), "", "zzz_bogus"));
    assert_eq!(preflight_with(Scope::LabRun, None, None), Ok(()), "a run binding no fixture is not refused");
    assert_eq!(preflight_with(Scope::LabRun, None, Some("other-suite@1.0")), Ok(()), "nor one binding another");
    let bound = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(bound.contains("unknown key `zzz_bogus`"), "{bound}");
    let every = problems(UserFileKind::LabFixture, Reach::Every);
    assert_eq!(every.len(), 1);
    assert!(every[0].to_string().contains("only a run that binds"), "{}", every[0]);
}

/// (review C4) A workload copy another tier shadows is never loaded.
#[test]
#[serial_test::serial]
fn a_shadowed_workload_does_not_block_a_run() {
    let state = IsolatedState::new();
    let tpl = state.join("tpl");
    std::fs::create_dir_all(tpl.join("workloads")).unwrap();
    unsafe { std::env::set_var("DARKMUX_TEMPLATES_DIR", &tpl) };
    std::fs::write(tpl.join("workloads/quick-q.json"), with_key(quick_q(), "/workload", "promt").to_string()).unwrap();
    write_workload_named(&state, "quick-q", &quick_q());
    let run = preflight_with(Scope::LabRun, None, None);
    let every = problems(UserFileKind::Workload, Reach::Every);
    unsafe { std::env::remove_var("DARKMUX_TEMPLATES_DIR") };
    assert_eq!(run, Ok(()));
    assert_eq!(every.len(), 1);
    assert!(every[0].to_string().contains("shadowed by"), "{}", every[0]);
}

fn write_workload_named(state: &IsolatedState, id: &str, doc: &Value) {
    std::fs::create_dir_all(state.join("workloads")).unwrap();
    std::fs::write(state.join(format!("workloads/{id}.json")), doc.to_string()).unwrap();
}

/// `lab run` checks the fixture its workload binds, before minting.
#[test]
#[serial_test::serial]
fn lab_run_refuses_the_bad_fixture_its_workload_binds() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["id"] = json!("probe-fx");
    wl["workload"]["requires_fixture"] = json!(PROBE_FIXTURE);
    write_workload_named(&state, "probe-fx", &wl);
    register_fixture(&state, &with_key(tiny_fixture(), "", "zzz_bogus"));
    let err = crate::lab::run::lab_run(crate::lab::run::RunOpts {
        workload_id: "probe-fx".into(),
        profile_name: None,
        runs: 1,
        config_path: None,
        quiet: true,
        loop_override: None,
        inject_context: None,
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("lab run: refusing to start") && err.contains("unknown key `zzz_bogus`"), "{err}");
}
