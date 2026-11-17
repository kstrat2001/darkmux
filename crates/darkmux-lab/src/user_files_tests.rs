//! The unknown-key gate over the lab's user files: a workload document or a
//! registered fixture's manifest with an unknown key is refused by the
//! lab-run preflight (and `lab run` itself, before it mints anything),
//! naming the file, the key and the closest valid key.

use super::*;
use darkmux_types::test_isolation::IsolatedState;
use darkmux_types::user_files::{key_issues, no_retired, open_objects, text_key_issues, with_duplicate_key, Issue, Reach, Repeat, UserFileKind};
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
        (with_key(quick_q(), "/workload", "replyRom"), "workload.replyRom", "workload.replyRoom"),
        (with_key(quick_q(), "/workload/verify", "expct"), "workload.verify.expct", "workload.verify.expect"),
        (with_key(quick_q(), "/workload/verify", "grde"), "workload.verify.grde", "workload.verify.grade"),
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

/// A key written twice in a workload, top level or nested, is refused by
/// the lab preflight and doctor, naming its path, and by no other scope. A
/// `serde_json::Value` keeps only the last copy, so the gate never saw it,
/// while the workload's typed load refuses the whole file over it.
#[test]
#[serial_test::serial]
fn a_workload_with_a_duplicate_key_is_refused_by_the_lab_preflight() {
    for pointer in ["", "/workload", "/workload/verify"] {
        let (text, key) = with_duplicate_key(&quick_q(), pointer);
        assert!(serde_json::from_str::<crate::workloads::types::WorkloadManifest>(&text).is_err(), "precondition: {key} fails the typed load");
        let state = IsolatedState::new();
        let path = write_workload(&state, &quick_q());
        std::fs::write(&path, &text).unwrap();
        let found = problems(UserFileKind::Workload, Reach::Every);
        assert_eq!(found.len(), 1, "{key}: {found:?}");
        let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).expect_err(&key).to_string();
        for msg in [found[0].to_string(), refusal] {
            assert!(msg.contains(&path.display().to_string()) && msg.contains(&format!("duplicate key `{key}`")), "{msg}");
        }
        for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::FleetSubmission] {
            assert_eq!(preflight_with(scope, None, None), Ok(()), "a workload is consumed only by a lab run ({scope:?})");
        }
    }
}

/// A fixture manifest the run binds is refused the same way.
#[test]
#[serial_test::serial]
fn a_fixture_manifest_with_a_duplicate_key_is_refused_by_the_lab_preflight() {
    let (text, key) = with_duplicate_key(&tiny_fixture(), "");
    let state = IsolatedState::new();
    let path = register_fixture(&state, &tiny_fixture());
    std::fs::write(&path, &text).unwrap();
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).expect_err(&key).to_string();
    assert!(refusal.contains(&path.display().to_string()) && refusal.contains(&format!("duplicate key `{key}`")), "{refusal}");
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
    assert!(workloads.len() >= 6, "{workloads:?}");
    found.extend(workloads.iter().filter_map(|p| check_path::<WorkloadManifest>(UserFileKind::Workload, p, &no_retired)));
    for fixture in std::fs::read_dir(t.join("lab-fixtures")).unwrap().flatten() {
        found.extend(check_path::<FixtureManifest>(UserFileKind::LabFixture, &fixture.path().join(".fixture.json"), &no_retired));
        // (#3363) The roles an eval carries are role manifests too.
        found.extend(check_dir::<Role>(UserFileKind::Role, &fixture.path().join("roles"), &no_retired));
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

/// (#3363) An eval owns its roles, darkmux's own included: a built-in
/// workload bound to a built-in fixture runs on the role that fixture's
/// schema says it carries, so every such pair must load (`lab run
/// demo-quickstart` would otherwise be refused). A built-in workload bound to
/// a fixture darkmux does not ship can only be checked for the legacy case:
/// its role must be built in, so a fixture at schema 1.1 with no roles runs.
#[test]
fn every_builtin_workload_bound_to_a_builtin_fixture_finds_its_role_there() {
    let t = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/builtin");
    let read = |p: &std::path::Path| -> serde_json::Value { serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap() };
    let fixtures: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(t.join("lab-fixtures"))
        .unwrap()
        .flatten()
        .map(|e| (read(&e.path().join(".fixture.json"))["satisfies"].as_str().unwrap().to_string(), e.path()))
        .collect();
    let (mut checked, mut user_fixture) = (0, Vec::new());
    for path in crate::workloads::load::documents_in(&t.join("workloads")) {
        let doc = read(&path);
        let Some(requires) = doc["workload"]["requires_fixture"].as_str() else { continue };
        let id = doc["workload"]["id"].as_str().unwrap().to_string();
        let role = doc["workload"]["role"].as_str().unwrap_or_else(|| panic!("{path:?} names no role"));
        let Some((_, dir)) = fixtures.iter().find(|(satisfies, _)| satisfies == requires) else {
            darkmux_crew::dispatch::check_lab_role(&darkmux_crew::dispatch::LabInputs::Builtin, role)
                .unwrap_or_else(|e| panic!("{id} binds {requires}, a fixture darkmux does not ship: {e:#}"));
            user_fixture.push(id);
            continue;
        };
        let layout = crate::lab::fixture::FixtureManifest::load_from_dir(dir).unwrap().role_layout();
        darkmux_crew::dispatch::LabInputs::for_fixture(dir, &layout, role).unwrap_or_else(|e| panic!("{path:?}: {e:#}"));
        checked += 1;
    }
    assert!(checked >= 1, "demo-quickstart binds demo-tiny-py");
    assert_eq!(user_fixture, ["crawl-error-discard"], "crawl-error-discard binds rust-crawl-scope@1.0, a user fixture");
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
    assert!(refusal.contains("`workload.verify.must_contain` must be a list, got a string"), "{refusal}");
}

/// (`verify.expect`, `replyRoom`) The answer a workload expects is data, so
/// its own keys are never gated; an unregistered `replyRoom` is refused.
#[test]
#[serial_test::serial]
fn a_structured_output_workload_passes_and_a_bad_reply_room_is_refused() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["verify"] = json!({"expect": {"business_name": {"anyOf": ["A", "A LLC"]}, "zzz_any_field": [1, {"k": "v"}]}});
    wl["workload"]["replyRoom"] = json!("context");
    write_workload(&state, &wl);
    assert_eq!(preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)), Ok(()));
    wl["workload"]["replyRoom"] = json!("whole");
    write_workload(&state, &wl);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("workload.replyRoom"), "{refusal}");
}

/// (#3353) A grader's keys are checked like every other key, nested ones
/// included, and an unregistered report format is refused.
#[test]
#[serial_test::serial]
fn a_grader_with_a_mistyped_key_or_an_unknown_report_is_refused() {
    let state = IsolatedState::new();
    let mut wl = quick_q();
    wl["workload"]["provider"] = json!("coding-task");
    let suite = json!({"asks": ["a"], "command": "sh t.sh ${REPORT}", "report": "junit"});
    wl["workload"]["verify"] = json!({"grade": {"suite": suite}});
    write_workload(&state, &wl);
    assert_eq!(preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)), Ok(()));
    wl["workload"]["verify"]["grade"]["suite"]["comand"] = json!("x");
    write_workload(&state, &wl);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("workload.verify.grade.suite.comand"), "{refusal}");
    wl["workload"]["verify"] = json!({"grade": {"suite": {"asks": ["a"], "command": "x", "report": "tap"}}});
    write_workload(&state, &wl);
    let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).unwrap_err().to_string();
    assert!(refusal.contains("workload.verify.grade.suite.report"), "{refusal}");
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
    gate_agrees_with_loader_on_text::<T>(name, base, failures);
}

/// How many repeats the text guard saw named as failing the load, and as
/// keeping the last copy, so a guard that exercised only one kind fails.
static REPEATS_SEEN: [std::sync::atomic::AtomicUsize; 2] = [std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicUsize::new(0)];

/// The same promise for what a `Value` cannot hold: `base` with a key
/// written twice in each of its objects, read the way a typed loader reads
/// a file. What the gate says a repeat costs must be what serde does: a
/// repeat named as failing the load fails it, and one named as keeping the
/// last copy (or not refused at all, a note) loads.
fn gate_agrees_with_loader_on_text<T>(name: &str, base: &Value, failures: &mut Vec<String>)
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + 'static,
{
    let mut ptrs = Vec::new();
    pointers(base, String::new(), &mut ptrs);
    let objects: Vec<String> = ptrs.into_iter().filter(|p| base.pointer(p).and_then(Value::as_object).is_some_and(|o| !o.is_empty())).collect();
    if objects.is_empty() {
        failures.push(format!("{name}: no object to write a key twice in, nothing exercised"));
    }
    for p in objects {
        let (text, key) = with_duplicate_key(base, &p);
        let doc: Value = serde_json::from_str(&text).unwrap();
        let issues = text_key_issues::<T>(&text, &doc, &no_retired, true);
        let repeat = issues.iter().find_map(|k| match k.issue {
            Issue::Duplicate(r) => Some(r),
            _ => None,
        });
        let loaded = serde_json::from_str::<T>(&text);
        match (repeat, &loaded) {
            (Some(Repeat::Field), Ok(_)) => failures.push(format!("{name}: `{key}` written twice is named as failing the load, but it loads")),
            (Some(Repeat::Entry), Err(e)) => failures.push(format!("{name}: `{key}` written twice is named as keeping the last copy, but serde refused it ({e})")),
            (None, Err(e)) if issues.is_empty() => failures.push(format!("{name}: the gate passed `{key}` written twice, serde refused it ({e})")),
            _ => {}
        }
        if let Some(r) = repeat {
            REPEATS_SEEN[usize::from(r == Repeat::Entry)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
    let [fields, entries] = REPEATS_SEEN.each_ref().map(|c| c.load(std::sync::atomic::Ordering::Relaxed));
    assert!(fields > 0 && entries > 0, "the text guard saw {fields} repeated field(s) and {entries} repeated entr(ies); both must be exercised");
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

/// (#3396 review) A fixture `schema_version` darkmux cannot parse is refused
/// by every entry point that consumes the manifest: the lab-run preflight and
/// `darkmux doctor` (both through the gate), and `lab fixture register` and
/// `lab doctor` (both through the manifest's load). Each names the file or
/// the value.
#[test]
#[serial_test::serial]
fn an_unparseable_fixture_schema_version_is_refused_by_every_consuming_entry_point() {
    for bad in [" 1.2", "1.2 ", "v1.2", "1.2-rc1"] {
        let state = IsolatedState::new();
        let mut doc = tiny_fixture();
        doc["schema_version"] = json!(bad);
        let path = register_fixture(&state, &doc);
        let refusal = preflight_with(Scope::LabRun, None, Some(PROBE_FIXTURE)).expect_err(bad).to_string();
        assert!(refusal.contains(&path.display().to_string()) && refusal.contains(&format!("`{bad}`")), "{bad:?}: {refusal}");
        let every = problems(UserFileKind::LabFixture, Reach::Every);
        assert_eq!(every.len(), 1, "{bad:?}: doctor reports it");
        assert!(every[0].to_string().contains(&format!("`{bad}`")), "{}", every[0]);
        let dir = path.parent().unwrap();
        let err = crate::lab::registry::LabRegistry::default().register(dir, None, false).map(|_| ()).unwrap_err();
        assert!(format!("{err:#}").contains(&format!("`{bad}`")), "register: {err:#}");
        let doctor = crate::lab::fixture::FixtureManifest::load_from_dir(dir).unwrap_err();
        assert!(format!("{doctor:#}").contains(&format!("`{bad}`")), "lab doctor's load: {doctor:#}");
    }
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
