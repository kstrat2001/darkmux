//! The unknown-key gate over the lab's user files: a workload document or a
//! registered fixture's manifest with an unknown key is refused by the
//! lab-run preflight (and `lab run` itself, before it mints anything),
//! naming the file, the key and the closest valid key.

use super::*;
use darkmux_types::test_isolation::IsolatedState;
use darkmux_types::user_files::{no_retired, open_objects, key_issues};
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
        "path": dir, "content_hash": "x", "hashed_at": "2026-01-01T00:00:00Z", "manifest_version": "1.0"
    }}});
    std::fs::write(state.join("lab-registry.json"), registry.to_string()).unwrap();
    manifest
}

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
        let found = problems(UserFileKind::Workload);
        assert_eq!(found.len(), 1, "{key}: {found:?}");
        assert_refused(&found[0].to_string(), &path, key, closest);
        let refusal = preflight_with(Scope::LabRun, None).expect_err(key).to_string();
        assert_refused(&refusal, &path, key, closest);
        for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::FleetSubmission] {
            assert_eq!(preflight_with(scope, None), Ok(()), "a workload is consumed only by a lab run ({scope:?})");
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
        let refusal = preflight_with(Scope::LabRun, None).expect_err(key).to_string();
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
    let refusal = preflight_with(Scope::LabRun, None).unwrap_err().to_string();
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
    assert_eq!(preflight_with(Scope::LabRun, None), Ok(()));
}

/// What darkmux ships must never be refused: every embedded workload and
/// the built-in fixture manifest.
#[test]
fn every_shipped_document_has_no_unknown_keys() {
    for (id, text) in crate::workloads::load::EMBEDDED_WORKLOADS {
        let doc: Value = serde_json::from_str(text).unwrap();
        assert_eq!(key_issues::<WorkloadManifest>(&doc, &no_retired), vec![], "builtin workload {id}");
    }
    assert_eq!(key_issues::<FixtureManifest>(&tiny_fixture(), &no_retired), vec![]);
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
    let refusal = preflight_with(Scope::LabRun, None).unwrap_err().to_string();
    assert!(refusal.contains("`workload.verify.must_contain` must be a list, got \"four\""), "{refusal}");
}
