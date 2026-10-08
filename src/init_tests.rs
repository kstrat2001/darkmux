//! (#3136) Tests for `darkmux init` as a whole: `init::init` end to end, the
//! IO halves of the model fill (`fill_worker_model`, `fill_utility_model`),
//! and `cmd_init`'s exit code. The pure helpers keep their own tests in
//! `init.rs`; these drive the real function against a throwaway machine.
//!
//! Every test here runs inside an [`InitEnv`], which points the whole run
//! somewhere disposable before `init` is called:
//!
//! * `IsolatedState` pins `DARKMUX_HOME` (and every other darkmux state
//!   variable) under a temp root, so `profiles.json` and `config.json` land
//!   there.
//! * `HOME` is moved to a separate temp dir, because three of `init`'s
//!   writes ignore `DARKMUX_HOME` and go through `dirs::home_dir()`: the
//!   installed skills (`~/.claude/skills`), the SessionStart hook
//!   (`~/.claude/settings.json`), and `bootstrap_config`'s default-home
//!   check. `InitEnv::new` asserts `dirs::home_dir()` really resolves to the
//!   temp dir before anything else runs.
//! * `DARKMUX_LMS_BIN` names a stub `lms` script that answers `ls --json`
//!   and `ps --json` from files the test writes, and logs every call, so no
//!   test can reach a real LM Studio and a test can assert `lms` was never
//!   asked.
//!
//! Because `HOME` is process-global and read by unrelated code, each test
//! holds `#[serial_test::serial]` AND runs in a process of its own
//! (`run_in_own_process!`, a no-op under nextest's process-per-test mode).

use super::*;
use darkmux_types::test_isolation::IsolatedState;
use std::ffi::OsString;
use tempfile::TempDir;

/// One model in the stub `lms ls --json` catalog.
fn model(key: &str, size_bytes: u64, kind: &str) -> Value {
    serde_json::json!({ "modelKey": key, "sizeBytes": size_bytes, "type": kind })
}

/// Small enough to fit the worker budget of any machine with a gigabyte of
/// RAM, so a test that expects a fill does not depend on the host.
const TINY: u64 = 1_000_000;
/// Larger than any machine's worker budget.
const COLOSSAL: u64 = 1_000_000_000_000_000;
const GB: u64 = 1_000_000_000;

/// The utility id the shipped example registry names.
fn shipped_utility() -> &'static str {
    let (a, b) = utility_value_span(EXAMPLE_PROFILES_JSON).expect("the example has a utility binding");
    &EXAMPLE_PROFILES_JSON[a..b]
}

struct InitEnv {
    state: IsolatedState,
    home: TempDir,
    stub: TempDir,
    prev: Vec<(&'static str, Option<OsString>)>,
}

impl InitEnv {
    fn new() -> Self {
        let state = IsolatedState::new();
        let home = TempDir::new().unwrap();
        let stub = TempDir::new().unwrap();
        let lms = stub.path().join("lms");
        fs::write(
            &lms,
            "#!/bin/sh\n\
             dir=\"$(dirname \"$0\")\"\n\
             echo \"$*\" >> \"$dir/calls.log\"\n\
             case \"$1\" in\n\
               ls) [ -f \"$dir/ls.json\" ] || exit 1; cat \"$dir/ls.json\" ;;\n\
               ps) [ -f \"$dir/ps.json\" ] && cat \"$dir/ps.json\" || echo '[]' ;;\n\
               *) exit 2 ;;\n\
             esac\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&lms, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = Self { state, home, stub, prev: Vec::new() };
        let home_path = env.home.path().to_path_buf();
        env.set("HOME", home_path.as_os_str().to_owned());
        env.set("DARKMUX_LMS_BIN", lms.into_os_string());
        // A skills override would change which source `init` installs from;
        // the target is what matters here, and it is under HOME.
        env.unset("DARKMUX_SKILLS_DIR");
        // The guard that makes everything below safe: if `HOME` did not take,
        // stop before `init` can write a real home directory.
        assert_eq!(dirs::home_dir().as_deref(), Some(env.home.path()), "HOME redirect did not take");
        assert!(
            user_profile_registry_path().unwrap().starts_with(env.state.path()),
            "the registry must resolve under the isolated root"
        );
        env
    }

    fn set(&mut self, var: &'static str, value: OsString) {
        self.prev.push((var, std::env::var_os(var)));
        // SAFETY: every caller holds #[serial_test::serial] in its own process.
        unsafe { std::env::set_var(var, value) };
    }

    fn unset(&mut self, var: &'static str) {
        self.prev.push((var, std::env::var_os(var)));
        // SAFETY: as above.
        unsafe { std::env::remove_var(var) };
    }

    /// What the stub answers to `lms ls --json` / `lms ps --json`.
    fn lms(&self, catalog: &[Value], loaded: &[&str]) {
        fs::write(self.stub.path().join("ls.json"), Value::Array(catalog.to_vec()).to_string()).unwrap();
        let ps: Vec<Value> = loaded.iter().map(|id| serde_json::json!({ "identifier": id, "modelKey": id })).collect();
        fs::write(self.stub.path().join("ps.json"), Value::Array(ps).to_string()).unwrap();
    }

    /// Point `lms` at a path that does not exist, so spawning it fails.
    fn lms_missing(&mut self) {
        let missing = self.stub.path().join("no-such-lms");
        self.set("DARKMUX_LMS_BIN", missing.into_os_string());
    }

    /// Every `lms` invocation so far, one per line.
    fn lms_calls(&self) -> String {
        fs::read_to_string(self.stub.path().join("calls.log")).unwrap_or_default()
    }

    fn registry(&self) -> PathBuf {
        self.state.join("profiles.json")
    }

    fn config(&self) -> PathBuf {
        self.state.join("config.json")
    }

    fn settings(&self) -> PathBuf {
        self.home.path().join(".claude").join("settings.json")
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.path().join(".claude").join("skills")
    }

    fn doc(&self, name: &str) -> PathBuf {
        self.home.path().join("project").join(name)
    }

    fn write_registry(&self, text: &str) {
        fs::write(self.registry(), text).unwrap();
    }

    fn read_registry(&self) -> String {
        fs::read_to_string(self.registry()).unwrap()
    }
}

impl Drop for InitEnv {
    fn drop(&mut self) {
        for (var, value) in self.prev.drain(..).rev() {
            // SAFETY: as in `set`.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

fn opts() -> InitOptions {
    InitOptions::default()
}

/// The example registry with every worker placeholder already filled, so the
/// worker fill has nothing to do and only the utility half runs.
fn example_without_placeholder() -> String {
    EXAMPLE_PROFILES_JSON.replace(PLACEHOLDER_MODEL_ID, "operator/chosen-model")
}

/// The utility id currently bound in `text`.
fn utility_in(text: &str) -> String {
    let (a, b) = utility_value_span(text).expect("utility binding present");
    text[a..b].to_string()
}

// ── First run, re-run, dry run ────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn a_first_run_writes_the_registry_config_and_skills_and_fills_the_worker() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    // The shipped utility is downloaded, and smaller, so it is not the worker.
    env.lms(&[model("tiny-llm", TINY, "llm"), model(shipped_utility(), TINY / 2, "llm")], &[]);

    let r = init(&opts()).unwrap();

    assert_eq!(r.profile_registry_path.as_deref(), Some(env.registry().as_path()));
    assert!(r.profile_registry_created && !r.profile_registry_already_present);
    let reg = env.read_registry();
    assert!(!reg.contains(PLACEHOLDER_MODEL_ID), "every placeholder filled: {reg}");
    assert_eq!(reg.matches("tiny-llm").count(), EXAMPLE_PROFILES_JSON.matches(PLACEHOLDER_MODEL_ID).count());
    assert_eq!(r.worker_model_filled.as_deref(), Some("tiny-llm"));
    assert_eq!(r.worker_model_unfilled_reason, None);
    // The shipped utility id is downloaded under its own key: left alone.
    assert_eq!(r.utility_model_filled, None);
    assert_eq!(r.utility_model_unfilled_reason, None);
    assert_eq!(utility_in(&reg), shipped_utility());

    assert_eq!(r.config_path.as_deref(), Some(env.config().as_path()));
    assert!(r.config_created && !r.config_already_present);
    let cfg: Value = serde_json::from_str(&fs::read_to_string(env.config()).unwrap()).unwrap();
    assert!(cfg.get("schema_version").is_some(), "{cfg}");

    assert_eq!(r.skills_targets, vec![env.skills_dir()]);
    assert!(!r.skills_installed.is_empty());
    assert!(r.skills_overwritten.is_empty() && r.skills_protected.is_empty());
    for name in &r.skills_installed {
        assert!(env.skills_dir().join(name).join("SKILL.md").is_file(), "{name} installed");
    }

    // Nothing optional was asked for, so nothing optional happened.
    assert_eq!(r.hook_added, None);
    assert!(!env.settings().exists(), "no hook without --with-hook");
    assert_eq!(r.claude_md_path, None);
    assert_eq!(r.agents_md_path, None);
}

#[test]
#[serial_test::serial]
fn a_re_run_reports_everything_present_and_changes_neither_file() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm"), model(shipped_utility(), TINY / 2, "llm")], &[]);
    let first = init(&opts()).unwrap();
    let reg = env.read_registry();
    let cfg = fs::read_to_string(env.config()).unwrap();
    // A model downloaded since the first run must not displace the filled one.
    env.lms(
        &[model("tiny-llm", TINY, "llm"), model("bigger-llm", 2 * TINY, "llm"), model(shipped_utility(), 2 * GB, "llm")],
        &[],
    );

    let r = init(&opts()).unwrap();

    assert!(r.profile_registry_already_present && !r.profile_registry_created);
    assert!(r.config_already_present && !r.config_created);
    assert_eq!(r.worker_model_filled, None);
    assert_eq!(r.worker_model_unfilled_reason, None);
    assert_eq!(r.utility_model_filled, None);
    assert_eq!(env.read_registry(), reg, "registry untouched on a re-run");
    assert_eq!(fs::read_to_string(env.config()).unwrap(), cfg, "config untouched on a re-run");
    // The unmodified skills refresh in place.
    assert!(r.skills_installed.is_empty());
    let mut refreshed = r.skills_overwritten.clone();
    let mut installed = first.skills_installed.clone();
    refreshed.sort();
    installed.sort();
    assert_eq!(refreshed, installed);
    assert!(r.skills_protected.is_empty());
}

#[test]
#[serial_test::serial]
fn a_dry_run_reports_intent_writes_nothing_and_never_asks_lms() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let claude_md = env.doc("CLAUDE.md");
    let agents_md = env.doc("AGENTS.md");

    let r = init(&InitOptions {
        with_hook: true,
        with_claude_md: Some(claude_md.clone()),
        with_agents_md: Some(agents_md.clone()),
        force: false,
        dry_run: true,
    })
    .unwrap();

    assert!(r.profile_registry_created, "dry run reports the intent");
    assert!(r.config_created && !r.config_already_present);
    assert!(!r.skills_installed.is_empty(), "dry run lists what it would install");
    assert_eq!(r.hook_added.as_deref(), Some(env.settings().as_path()));
    assert!(!r.hook_already_present);
    assert!(r.claude_md_appended && r.agents_md_appended);
    assert_eq!(r.worker_model_filled, None);
    assert_eq!(r.worker_model_unfilled_reason, None);
    for p in [env.registry(), env.config(), env.settings(), env.skills_dir(), claude_md, agents_md] {
        assert!(!p.exists(), "dry run wrote {}", p.display());
    }
    assert_eq!(env.lms_calls(), "", "dry run must not ask LM Studio");
}

#[test]
#[serial_test::serial]
fn a_dry_run_over_an_existing_install_reports_it_present_and_touches_nothing() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.write_registry(EXAMPLE_PROFILES_JSON);
    fs::write(env.config(), "{}\n").unwrap();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);

    let r = init(&InitOptions { dry_run: true, ..opts() }).unwrap();

    assert!(r.profile_registry_already_present && !r.profile_registry_created);
    assert!(r.config_already_present && !r.config_created);
    assert_eq!(env.read_registry(), EXAMPLE_PROFILES_JSON, "placeholder left for a real run");
    assert_eq!(env.lms_calls(), "");
}

#[test]
#[serial_test::serial]
fn an_existing_config_is_never_overwritten_even_with_force() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    fs::write(env.config(), "{ \"machine_id\": \"operator-named\" }\n").unwrap();

    let r = init(&InitOptions { force: true, ..opts() }).unwrap();

    assert!(r.config_already_present && !r.config_created);
    assert_eq!(fs::read_to_string(env.config()).unwrap(), "{ \"machine_id\": \"operator-named\" }\n");
}

// ── fill_worker_model, through init ───────────────────────────────────────

#[test]
#[serial_test::serial]
fn the_worker_fill_picks_by_loaded_then_largest_that_fits_and_says_why_when_it_cannot() {
    darkmux_types::run_in_own_process!();
    struct Case {
        name: &'static str,
        catalog: Vec<Value>,
        loaded: Vec<&'static str>,
        /// `Ok(id)`: filled with this id. `Err(fragment)`: left as the
        /// placeholder, with a reason containing this.
        want: std::result::Result<&'static str, &'static str>,
    }
    let cases = vec![
        Case {
            name: "the largest LLM under the budget wins",
            catalog: vec![model("small", TINY, "llm"), model("smaller", TINY / 2, "llm"), model("huge", COLOSSAL, "llm")],
            loaded: vec![],
            want: Ok("small"),
        },
        Case {
            name: "a loaded model wins even over the budget, namespace stripped",
            catalog: vec![model("small", TINY, "llm"), model("huge", COLOSSAL, "llm")],
            loaded: vec!["darkmux:huge"],
            want: Ok("huge"),
        },
        Case {
            name: "a loaded model LM Studio does not list falls through to the budget pick",
            catalog: vec![model("small", TINY, "llm")],
            loaded: vec!["ghost"],
            want: Ok("small"),
        },
        Case {
            name: "an embedding model is never a worker",
            catalog: vec![model("embedder", TINY, "embedding")],
            loaded: vec!["embedder"],
            want: Err("has no downloaded LLM that fits"),
        },
        Case {
            name: "nothing fits",
            catalog: vec![model("huge", COLOSSAL, "llm")],
            loaded: vec![],
            want: Err("has no downloaded LLM that fits"),
        },
        Case { name: "nothing downloaded", catalog: vec![], loaded: vec![], want: Err("has no downloaded LLM that fits") },
    ];
    for case in cases {
        let env = InitEnv::new();
        env.lms(&case.catalog, &case.loaded);
        let r = init(&opts()).unwrap();
        let reg = env.read_registry();
        match case.want {
            Ok(id) => {
                assert_eq!(r.worker_model_filled.as_deref(), Some(id), "{}", case.name);
                assert_eq!(r.worker_model_unfilled_reason, None, "{}", case.name);
                assert!(!reg.contains(PLACEHOLDER_MODEL_ID), "{}", case.name);
                assert!(reg.contains(&format!("\"id\": \"{id}\"")), "{}: {reg}", case.name);
            }
            Err(fragment) => {
                assert_eq!(r.worker_model_filled, None, "{}", case.name);
                let reason = r.worker_model_unfilled_reason.unwrap_or_default();
                assert!(reason.contains(fragment), "{}: {reason}", case.name);
                assert!(reason.contains("re-run `darkmux init`"), "{}: {reason}", case.name);
                assert_eq!(
                    reg.matches(PLACEHOLDER_MODEL_ID).count(),
                    EXAMPLE_PROFILES_JSON.matches(PLACEHOLDER_MODEL_ID).count(),
                    "{}: every placeholder stays",
                    case.name
                );
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn an_unreachable_lms_leaves_both_bindings_and_names_the_fix() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();

    let r = init(&opts()).unwrap();

    let reason = r.worker_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("could not ask LM Studio what is downloaded"), "{reason}");
    assert!(reason.contains("lms bootstrap"), "{reason}");
    assert_eq!(r.worker_model_filled, None);
    // The utility half stays quiet: the worker already reported lms.
    assert_eq!(r.utility_model_filled, None);
    assert_eq!(r.utility_model_unfilled_reason, None);
    assert_eq!(env.read_registry(), EXAMPLE_PROFILES_JSON);
}

#[test]
#[serial_test::serial]
fn a_registry_the_operator_already_filled_is_never_rewritten_or_asked_about() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let mine = example_without_placeholder();
    env.write_registry(&mine);
    env.lms(&[model("tiny-llm", TINY, "llm"), model(shipped_utility(), 2 * GB, "llm")], &[]);

    let r = init(&opts()).unwrap();

    assert!(r.profile_registry_already_present);
    assert_eq!(r.worker_model_filled, None);
    assert_eq!(r.worker_model_unfilled_reason, None);
    assert_eq!(env.read_registry(), mine);
    assert!(!env.lms_calls().contains("ps"), "no placeholder, so no loaded-model lookup: {}", env.lms_calls());
}

#[test]
#[serial_test::serial]
fn a_registry_that_cannot_be_read_is_reported_by_both_halves() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    // A directory where the file should be: `exists()` is true, reading fails.
    fs::create_dir_all(env.registry()).unwrap();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);

    let r = init(&opts()).unwrap();

    assert!(r.profile_registry_already_present);
    for reason in [r.worker_model_unfilled_reason, r.utility_model_unfilled_reason] {
        let reason = reason.expect("a reason");
        assert!(reason.starts_with("reading "), "{reason}");
        assert!(reason.contains("profiles.json"), "{reason}");
    }
}

/// Make `path` read-only; `false` when the process can write it anyway
/// (running as root), in which case a write-failure test proves nothing.
fn make_read_only(path: &Path) -> bool {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).unwrap();
    fs::OpenOptions::new().write(true).open(path).is_err()
}

#[test]
#[serial_test::serial]
fn a_registry_that_cannot_be_written_is_reported_by_both_halves() {
    darkmux_types::run_in_own_process!();
    // The worker half: a placeholder to fill and a model to fill it with.
    let env = InitEnv::new();
    env.write_registry(EXAMPLE_PROFILES_JSON);
    env.lms(&[model("tiny-llm", TINY, "llm"), model(shipped_utility(), 2 * GB, "llm")], &[]);
    if !make_read_only(&env.registry()) {
        return;
    }
    let r = init(&opts()).unwrap();
    let reason = r.worker_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("writing ") && reason.contains("profiles.json"), "{reason}");
    assert_eq!(r.worker_model_filled, None);
    drop(env);

    // The utility half: no placeholder, and the shipped utility id is absent.
    let env = InitEnv::new();
    env.write_registry(&example_without_placeholder());
    env.lms(&[model("other-4b", 2 * GB, "llm")], &[]);
    assert!(make_read_only(&env.registry()));
    let r = init(&opts()).unwrap();
    let reason = r.utility_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("writing ") && reason.contains("profiles.json"), "{reason}");
    assert_eq!(r.utility_model_filled, None);
}

// ── fill_utility_model, through init ──────────────────────────────────────

#[test]
#[serial_test::serial]
fn the_utility_fill_rewrites_only_the_shipped_binding_and_only_when_it_is_missing() {
    darkmux_types::run_in_own_process!();
    let shipped = shipped_utility();
    let bare = shipped.rsplit('/').next().unwrap();
    struct Case {
        name: &'static str,
        registry: String,
        catalog: Vec<Value>,
        /// `Ok(Some(id))`: rewritten to id. `Ok(None)`: left alone, silently.
        /// `Err(fragment)`: left alone, with a reason containing this.
        want: std::result::Result<Option<String>, &'static str>,
    }
    let filled = example_without_placeholder();
    let custom = filled.replace(shipped, "operator/own-utility");
    let no_internal = {
        let mut v: Value = serde_json::from_str(&filled).unwrap();
        v.as_object_mut().unwrap().remove("internal");
        serde_json::to_string_pretty(&v).unwrap()
    };
    let cases = vec![
        Case {
            name: "downloaded under the shipped key: kept",
            registry: filled.clone(),
            catalog: vec![model(shipped, 2 * GB, "llm")],
            want: Ok(None),
        },
        Case {
            name: "downloaded without the publisher prefix: the bare key",
            registry: filled.clone(),
            catalog: vec![model(bare, 2 * GB, "llm"), model("other", 3 * GB, "llm")],
            want: Ok(Some(bare.to_string())),
        },
        Case {
            name: "absent: the smallest real LLM, never a toy or an embedding",
            registry: filled.clone(),
            catalog: vec![
                model("toy-0.5b", GB / 2, "llm"),
                model("embed-2gb", 2 * GB, "embedding"),
                model("mid-3b", 3 * GB, "llm"),
                model("small-2b", 2 * GB, "llm"),
            ],
            want: Ok(Some("small-2b".to_string())),
        },
        Case {
            name: "absent, and only toys downloaded",
            registry: filled.clone(),
            catalog: vec![model("toy-0.5b", GB / 2, "llm")],
            want: Err("is not downloaded and no LLM of at least 1 GB is"),
        },
        Case {
            name: "an id the operator set by hand is theirs, downloaded or not",
            registry: custom,
            catalog: vec![model("small-2b", 2 * GB, "llm")],
            want: Ok(None),
        },
        Case {
            name: "no utility binding at all",
            registry: no_internal,
            catalog: vec![model("small-2b", 2 * GB, "llm")],
            want: Ok(None),
        },
    ];
    for case in cases {
        let env = InitEnv::new();
        env.write_registry(&case.registry);
        env.lms(&case.catalog, &[]);
        let r = init(&opts()).unwrap();
        let after = env.read_registry();
        assert_eq!(r.worker_model_unfilled_reason, None, "{}", case.name);
        match case.want {
            Ok(Some(id)) => {
                assert_eq!(r.utility_model_filled.as_deref(), Some(id.as_str()), "{}", case.name);
                assert_eq!(r.utility_model_unfilled_reason, None, "{}", case.name);
                assert_eq!(utility_in(&after), id, "{}", case.name);
                // Only the binding moved: the rest of the file is byte-identical.
                assert_eq!(after.replacen(&id, shipped, 1), case.registry, "{}", case.name);
            }
            Ok(None) => {
                assert_eq!(r.utility_model_filled, None, "{}", case.name);
                assert_eq!(r.utility_model_unfilled_reason, None, "{}", case.name);
                assert_eq!(after, case.registry, "{}", case.name);
            }
            Err(fragment) => {
                assert_eq!(r.utility_model_filled, None, "{}", case.name);
                let reason = r.utility_model_unfilled_reason.unwrap_or_default();
                assert!(reason.contains(fragment), "{}: {reason}", case.name);
                assert!(reason.contains(&format!("`{shipped}`")), "{}: names the id: {reason}", case.name);
                assert_eq!(after, case.registry, "{}", case.name);
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn a_fresh_install_fills_the_worker_and_the_utility_in_one_run() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("only-llm", 2 * GB, "llm")], &["only-llm"]);

    let r = init(&opts()).unwrap();

    assert_eq!(r.worker_model_filled.as_deref(), Some("only-llm"));
    assert_eq!(r.utility_model_filled.as_deref(), Some("only-llm"));
    let reg = env.read_registry();
    assert!(!reg.contains(PLACEHOLDER_MODEL_ID));
    assert_eq!(utility_in(&reg), "only-llm");
}

// ── Skills ────────────────────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn an_edited_darkmux_skill_is_protected_and_force_overwrites_it() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let first = init(&opts()).unwrap();
    let name = first.skills_installed.iter().find(|s| s.starts_with("darkmux-")).unwrap().clone();
    let skill_md = env.skills_dir().join(&name).join("SKILL.md");
    fs::write(&skill_md, "my own edit\n").unwrap();

    let kept = init(&opts()).unwrap();
    assert_eq!(kept.skills_protected, vec![name.clone()]);
    assert!(!kept.skills_overwritten.contains(&name));
    assert!(kept.skills_force_overwrote_modified.is_empty());
    assert_eq!(fs::read_to_string(&skill_md).unwrap(), "my own edit\n", "the edit survives");

    let forced = init(&InitOptions { force: true, ..opts() }).unwrap();
    assert!(forced.skills_protected.is_empty());
    assert_eq!(forced.skills_force_overwrote_modified, vec![name.clone()]);
    assert!(forced.skills_overwritten.contains(&name));
    assert_ne!(fs::read_to_string(&skill_md).unwrap(), "my own edit\n");
}

// ── SessionStart hook ─────────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn the_hook_is_added_once_reported_present_after_and_replaced_not_duplicated_by_force() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    fs::create_dir_all(env.settings().parent().unwrap()).unwrap();
    fs::write(env.settings(), "{ \"theme\": \"dark\" }\n").unwrap();
    let hook = || InitOptions { with_hook: true, ..opts() };
    let session_start = |env: &InitEnv| -> Vec<Value> {
        let v: Value = serde_json::from_str(&fs::read_to_string(env.settings()).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark", "the operator's settings survive");
        v["hooks"]["SessionStart"].as_array().cloned().unwrap()
    };

    let r = init(&hook()).unwrap();
    assert_eq!(r.hook_added.as_deref(), Some(env.settings().as_path()));
    assert!(!r.hook_already_present);
    assert_eq!(session_start(&env).len(), 1);

    let r = init(&hook()).unwrap();
    assert!(r.hook_already_present);
    assert_eq!(session_start(&env).len(), 1);

    let r = init(&InitOptions { force: true, ..hook() }).unwrap();
    assert!(!r.hook_already_present, "force re-adds");
    let hooks = session_start(&env);
    assert_eq!(hooks.len(), 1, "not duplicated");
    assert!(hooks[0]["command"].as_str().unwrap().contains(HOOK_MARKER));
}

// ── CLAUDE.md / AGENTS.md ─────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn the_integration_sections_insert_append_stay_put_and_refresh_with_force() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let claude_md = env.doc("CLAUDE.md");
    let agents_md = env.doc("AGENTS.md");
    // CLAUDE.md does not exist yet; AGENTS.md has the operator's own prose.
    fs::create_dir_all(agents_md.parent().unwrap()).unwrap();
    fs::write(&agents_md, "# My agents\n\nKeep this.\n").unwrap();
    let docs = |force| InitOptions {
        with_claude_md: Some(claude_md.clone()),
        with_agents_md: Some(agents_md.clone()),
        force,
        ..opts()
    };

    let r = init(&docs(false)).unwrap();
    assert_eq!(r.claude_md_path.as_deref(), Some(claude_md.as_path()));
    assert_eq!(r.agents_md_path.as_deref(), Some(agents_md.as_path()));
    assert!(r.claude_md_appended && !r.claude_md_already_present);
    assert!(r.agents_md_appended && !r.agents_md_already_present);
    assert_eq!(fs::read_to_string(&claude_md).unwrap(), darkmux_claude_md_section());
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        format!("# My agents\n\nKeep this.\n\n{}", darkmux_agents_md_section())
    );

    let r = init(&docs(false)).unwrap();
    assert!(r.claude_md_already_present && !r.claude_md_appended);
    assert!(r.agents_md_already_present && !r.agents_md_appended);

    // A stale block, refreshed in place by --force; prose on both sides stays.
    let stale = format!("intro\n\n{CLAUDE_MD_HEADER}\n\nold words\n\n{CLAUDE_MD_FOOTER}\n\noutro\n");
    fs::write(&claude_md, &stale).unwrap();
    let r = init(&docs(true)).unwrap();
    assert!(r.claude_md_appended && !r.claude_md_already_present);
    assert!(r.agents_md_appended, "force refreshes the AGENTS.md block too");
    let refreshed = fs::read_to_string(&claude_md).unwrap();
    assert!(refreshed.starts_with("intro\n\n") && refreshed.ends_with("\n\noutro\n"), "{refreshed}");
    assert!(!refreshed.contains("old words") && refreshed.contains(INTEGRATION_SECTION_BODY));
    assert_eq!(refreshed.matches(CLAUDE_MD_HEADER).count(), 1);
    assert_eq!(fs::read_to_string(&agents_md).unwrap().matches(AGENTS_MD_HEADER).count(), 1);
}

#[test]
#[serial_test::serial]
fn a_malformed_block_under_force_fails_init_and_leaves_the_doc_alone() {
    darkmux_types::run_in_own_process!();
    for (flag, header) in [("claude", CLAUDE_MD_HEADER), ("agents", AGENTS_MD_HEADER)] {
        let env = InitEnv::new();
        env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
        let doc = env.doc("DOC.md");
        fs::create_dir_all(doc.parent().unwrap()).unwrap();
        let broken = format!("mine\n{header}\nhalf a block, no end marker\n");
        fs::write(&doc, &broken).unwrap();
        let o = if flag == "claude" {
            InitOptions { with_claude_md: Some(doc.clone()), force: true, ..opts() }
        } else {
            InitOptions { with_agents_md: Some(doc.clone()), force: true, ..opts() }
        };
        let err = init(&o).unwrap_err().to_string();
        assert!(err.contains("end marker"), "{flag}: {err}");
        assert_eq!(fs::read_to_string(&doc).unwrap(), broken, "{flag}");
        // And `cmd_init` propagates it rather than exiting 0.
        let cmd = if flag == "claude" {
            crate::cmd_init(false, Some(doc.clone()), None, true, false)
        } else {
            crate::cmd_init(false, None, Some(doc.clone()), true, false)
        };
        assert!(cmd.is_err(), "{flag}: cmd_init must fail");
    }
}

// ── cmd_init ──────────────────────────────────────────────────────────────

/// `cmd_init` exits 0 on every outcome `init` reports as a success; its
/// printed text is asserted by the CLI tests in `tests/cli.rs`, which can
/// read the child's stdout. Here every reporting branch runs at least once.
#[test]
#[serial_test::serial]
fn cmd_init_exits_zero_for_every_successful_outcome() {
    darkmux_types::run_in_own_process!();
    // Dry run, fresh machine, with every option.
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let md = env.doc("CLAUDE.md");
    let ag = env.doc("AGENTS.md");
    assert_eq!(crate::cmd_init(true, Some(md.clone()), Some(ag.clone()), false, true).unwrap(), 0);
    assert!(!env.registry().exists());

    // Real first run with every option, worker and utility both filled.
    env.lms(&[model("only-llm", 2 * GB, "llm")], &[]);
    assert_eq!(crate::cmd_init(true, Some(md.clone()), Some(ag.clone()), false, false).unwrap(), 0);
    assert!(env.read_registry().contains("only-llm"));

    // Re-run: everything already present, one skill edited (protected).
    let edited = fs::read_dir(env.skills_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("darkmux-"))
        .unwrap()
        .join("SKILL.md");
    fs::write(&edited, "edit\n").unwrap();
    assert_eq!(crate::cmd_init(true, Some(md.clone()), Some(ag.clone()), false, false).unwrap(), 0);
    assert_eq!(fs::read_to_string(&edited).unwrap(), "edit\n");

    // Forced: the edited skill overwritten.
    assert_eq!(crate::cmd_init(false, None, None, true, false).unwrap(), 0);
    assert_ne!(fs::read_to_string(&edited).unwrap(), "edit\n");
    drop(env);

    // A fresh machine where neither model can be set.
    let env = InitEnv::new();
    env.lms(&[model("toy", GB / 2, "embedding")], &[]);
    assert_eq!(crate::cmd_init(false, None, None, false, false).unwrap(), 0);
    assert!(env.read_registry().contains(PLACEHOLDER_MODEL_ID));
    drop(env);

    // The utility reason branch.
    let env = InitEnv::new();
    env.write_registry(&example_without_placeholder());
    env.lms(&[model("toy", GB / 2, "llm")], &[]);
    assert_eq!(crate::cmd_init(false, None, None, false, false).unwrap(), 0);
}
