//! Launch inputs a config cannot consume are refused before anything is
//! minted, naming the closest valid value (contracts 7 and 9 in
//! `docs/agents/CONTRACTS.md`): an undeclared `--param` key, an unknown
//! `role=` override on a coder-phase graph (#3634), and a non-string value
//! for an input the launcher reads as text (#3635).
//!
//! Only dry runs launch here: a real launch is never run by a test in this
//! file. The real launch reaches the same checks because they run before the
//! `--dry-run` short-circuit in [`launch`].

use super::*;
use darkmux_types::test_isolation::IsolatedState;

fn shipped(file: &str) -> MissionConfig {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("templates/builtin/mission-configs")
            .join(file),
    )
    .expect("the config is shipped");
    serde_json::from_str(&raw).expect("parses")
}

fn strings(params: &[&str]) -> Vec<String> {
    params.iter().map(|p| p.to_string()).collect()
}

/// An isolated darkmux home with no reachable model server, so nothing a
/// dry run does can touch the operator's state or a real LM Studio.
struct DryRunEnv {
    _state: IsolatedState,
    stub: tempfile::TempDir,
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl DryRunEnv {
    fn new() -> Self {
        let state = IsolatedState::new();
        let stub = tempfile::TempDir::new().unwrap();
        let lms = stub.path().join("lms");
        std::fs::write(&lms, "#!/bin/sh\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&lms, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = Self {
            _state: state,
            stub,
            prev: Vec::new(),
        };
        env.set("DARKMUX_LMS_BIN", lms.into_os_string());
        env.set("DARKMUX_LMSTUDIO_URL", "http://127.0.0.1:9".into());
        env
    }

    fn set(&mut self, var: &'static str, value: std::ffi::OsString) {
        self.prev.push((var, std::env::var_os(var)));
        // SAFETY: every caller holds #[serial_test::serial].
        unsafe { std::env::set_var(var, value) };
    }

    fn minted_nothing(&self) -> bool {
        let dir = crew::loader::missions_dir();
        std::fs::read_dir(&dir).map(|rd| rd.count() == 0).unwrap_or(true)
    }
}

impl Drop for DryRunEnv {
    fn drop(&mut self) {
        for (var, prev) in self.prev.drain(..).rev() {
            // SAFETY: as in `set`.
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
        let _ = &self.stub;
    }
}

fn coder_phase_dry_run(extra: &[&str]) -> Result<i32> {
    let mut params = strings(&[
        "workdir=/tmp/darkmux-dry-run-only",
        "branch=feature/x",
        "base=main",
        "dry_run=true",
    ]);
    params.extend(strings(extra));
    launch("coder-phase", None, &params, None)
}

// ── #3634: the coder-phase `role=` override ─────────────────────────────────

#[test]
#[serial_test::serial]
fn an_unknown_role_override_is_refused_before_a_dry_run_naming_the_closest_role() {
    let env = DryRunEnv::new();
    let err = coder_phase_dry_run(&["role=codr"])
        .expect_err("an unknown role is refused")
        .to_string();
    assert!(err.contains("`role=codr` names no role"), "{err}");
    assert!(err.contains("the closest is `coder`"), "{err}");
    assert!(err.contains("darkmux role list"), "{err}");
    assert!(env.minted_nothing());
}

#[test]
#[serial_test::serial]
fn a_role_override_far_from_every_role_is_refused_too() {
    let _env = DryRunEnv::new();
    let err = coder_phase_dry_run(&["role=no-such-role"])
        .expect_err("refused")
        .to_string();
    assert!(err.contains("`role=no-such-role` names no role"), "{err}");
}

#[test]
#[serial_test::serial]
fn a_valid_role_override_is_what_the_dry_run_graph_shows() {
    let _env = DryRunEnv::new();
    let _ = take_last_dry_run_report();
    assert_eq!(
        coder_phase_dry_run(&["role=test-designer"]).expect("a known role launches"),
        0
    );
    let report = take_last_dry_run_report().expect("a dry run prints its plan");
    assert!(report.contains("role=test-designer"), "{report}");
    assert!(
        !report.contains("role=coder"),
        "the overridden default must not show: {report}"
    );
}

#[test]
#[serial_test::serial]
fn without_an_override_the_dry_run_graph_shows_the_documents_role() {
    let _env = DryRunEnv::new();
    let _ = take_last_dry_run_report();
    assert_eq!(coder_phase_dry_run(&[]).expect("launches"), 0);
    let report = take_last_dry_run_report().expect("a dry run prints its plan");
    assert!(report.contains("role=coder"), "{report}");
}

#[test]
fn the_role_check_names_the_closest_known_role_and_passes_a_known_one() {
    let config = shipped("coder-phase.json");
    let known = vec!["coder".to_string(), "test-designer".to_string()];
    let with_role = |r: &str| BTreeMap::from([("role".to_string(), serde_json::json!(r))]);
    assert!(refuse_unknown_role_override(&config, &with_role("coder"), &known).is_ok());
    assert!(
        refuse_unknown_role_override(&config, &BTreeMap::new(), &known).is_ok(),
        "no override, nothing to check"
    );
    let err = refuse_unknown_role_override(&config, &with_role("test-desinger"), &known)
        .unwrap_err()
        .to_string();
    assert!(err.contains("the closest is `test-designer`"), "{err}");
    let review = shipped("review.json");
    assert!(
        refuse_unknown_role_override(&review, &with_role("anything"), &known).is_ok(),
        "only a coder-phase graph reads `role`"
    );
}

// ── #3635: a `rules` input that is not a string ─────────────────────────────

#[test]
fn a_rules_input_that_is_not_a_string_is_refused_naming_the_accepted_shape() {
    let config = shipped("review.json");
    let dir = tempfile::TempDir::new().unwrap();
    let input = dir.path().join("in.json");
    std::fs::write(&input, r#"{"rules":["no-such"],"diff_file":"d.diff"}"#).unwrap();
    let err = resolve_inputs(&config, Some(&input), &[])
        .expect_err("an array is refused")
        .to_string();
    assert!(err.contains("input `rules`"), "{err}");
    assert!(err.contains("a JSON array"), "names the type it got: {err}");
    assert!(err.contains("comma-separated"), "names the accepted shape: {err}");
    assert!(!err.contains("no-such"), "never quotes the value: {err}");
}

#[test]
fn every_input_the_launcher_reads_as_text_refuses_a_non_string() {
    let config = shipped("coder-phase.json");
    for name in LAUNCHER_STRING_INPUTS {
        let collected = BTreeMap::from([(name.to_string(), serde_json::json!(7))]);
        let err = refuse_non_string_inputs(&config, &collected)
            .expect_err(name)
            .to_string();
        assert!(
            err.contains(&format!("input `{name}`")) && err.contains("a JSON number"),
            "{err}"
        );
    }
    let fine = BTreeMap::from([("rules".to_string(), serde_json::json!("a,b"))]);
    assert!(refuse_non_string_inputs(&config, &fine).is_ok());
}

// ── an undeclared `--param` key ─────────────────────────────────────────────

#[test]
fn an_undeclared_param_is_refused_naming_the_closest_declared_input() {
    let config = shipped("review.json");
    let err = resolve_inputs(&config, None, &strings(&["diff_file=d.diff", "rulez=test-gap"]))
        .expect_err("an undeclared key is refused")
        .to_string();
    assert!(
        err.contains("input `rulez` is not declared by config \"review\""),
        "{err}"
    );
    assert!(err.contains("the closest is `rules`"), "{err}");
    assert!(err.contains("Declared inputs:"), "{err}");
}

#[test]
fn an_undeclared_param_on_a_config_with_no_inputs_says_it_takes_none() {
    let config: MissionConfig =
        serde_json::from_value(serde_json::json!({ "id": "bare", "name": "bare", "phases": [] })).unwrap();
    let err = resolve_inputs(&config, None, &strings(&["zzz=1"]))
        .expect_err("refused")
        .to_string();
    assert!(err.contains("input `zzz` is not declared"), "{err}");
    assert!(err.contains("declares no inputs"), "{err}");
}

#[test]
fn the_launchers_own_flags_are_never_refused_as_undeclared() {
    let config = shipped("coder-phase.json");
    let params = strings(&["workdir=w", "branch=b", "base=main", "dry_run=true", "force=true"]);
    assert!(resolve_inputs(&config, None, &params).is_ok());
}

#[test]
fn args_is_accepted_only_by_a_config_that_reads_it() {
    let takes: MissionConfig = serde_json::from_value(serde_json::json!({
        "id": "panel", "name": "panel",
        "phases": [{ "id": "p", "tasks": [{ "id": "t", "reads": ["__panel_args__"],
            "steps": [{ "id": "s", "kind": "procedural.noop" }] }] }]
    }))
    .unwrap();
    assert!(resolve_inputs(&takes, None, &strings(&["args=42"])).is_ok());
    let bare: MissionConfig =
        serde_json::from_value(serde_json::json!({ "id": "bare", "name": "bare", "phases": [] })).unwrap();
    let err = resolve_inputs(&bare, None, &strings(&["args=42"]))
        .expect_err("refused")
        .to_string();
    assert!(err.contains("input `args` is not declared"), "{err}");
    let mut audited = bare.clone();
    audited.cmd = Some("pr-merge".to_string());
    assert!(
        resolve_inputs(&audited, None, &strings(&["args=42"])).is_ok(),
        "a `cmd` config's audit reads `args`"
    );
}

#[test]
#[serial_test::serial]
fn an_undeclared_param_refuses_a_dry_run_and_mints_nothing() {
    let env = DryRunEnv::new();
    let err = coder_phase_dry_run(&["rolez=coder"]).expect_err("refused").to_string();
    assert!(
        err.contains("input `rolez` is not declared") && err.contains("the closest is `role`"),
        "{err}"
    );
    assert!(env.minted_nothing());
}

// ── #3634, related: an unknown `mod_seat_profile` names the closest ─────────

#[test]
#[serial_test::serial]
fn an_unknown_mod_seat_profile_names_the_closest_defined_profile() {
    let env = DryRunEnv::new();
    let dir = tempfile::TempDir::new().unwrap();
    let reg = dir.path().join("profiles.json");
    std::fs::write(&reg, r#"{"profiles":{"deep":{"models":[{"id":"m","n_ctx":1}]}}}"#).unwrap();
    let diff = dir.path().join("d.diff");
    std::fs::write(&diff, "").unwrap();
    // `review` declares no `profiles` input, so the registry comes from the
    // variable dispatch reads too (`IsolatedState` restores it).
    // SAFETY: serial.
    unsafe { std::env::set_var("DARKMUX_PROFILES", &reg) };
    let params = vec![
        format!("diff_file={}", diff.display()),
        "mod_seat_profile=deeep".to_string(),
        "dry_run=true".to_string(),
    ];
    let err = launch("review", None, &params, None).expect_err("refused").to_string();
    assert!(err.contains("names no profile in the registry"), "{err}");
    assert!(err.contains("the closest is `deep`"), "{err}");
    assert!(env.minted_nothing());
}

/// The coder-phase gate's re-run command names only inputs `review`
/// declares (it named `worktree`, which a launch refuses).
#[test]
fn the_gates_review_rerun_command_names_only_declared_inputs() {
    let review = shipped("review.json");
    let line = review_rerun_command(std::path::Path::new("/wt/x"));
    let keys: Vec<&str> =
        line.split("--param ").skip(1).filter_map(|p| p.split_once('=').map(|(k, _)| k)).collect();
    assert!(!keys.is_empty(), "{line}");
    for key in keys {
        assert!(review.inputs.iter().any(|i| i.name == key), "`{key}` is not a `review` input: {line}");
    }
    assert!(line.contains("/wt/x"), "{line}");
}

/// Every JSON type a refusal can name, so a refusal never says the wrong one.
#[test]
fn json_type_phrase_names_every_json_type() {
    for (value, phrase) in [
        (serde_json::json!(null), "JSON null"),
        (serde_json::json!(true), "a JSON boolean"),
        (serde_json::json!(1), "a JSON number"),
        (serde_json::json!("s"), "a JSON string"),
        (serde_json::json!([1]), "a JSON array"),
        (serde_json::json!({ "k": 1 }), "a JSON object"),
    ] {
        assert_eq!(json_type_phrase(&value), phrase, "{value}");
    }
}
