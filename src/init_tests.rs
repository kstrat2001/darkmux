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

/// `cmd_init` with the flags as positional values, as the tests below read.
fn cmd_init_with(
    with_hook: bool,
    with_claude_md: Option<PathBuf>,
    with_agents_md: Option<PathBuf>,
    force: bool,
    dry_run: bool,
) -> Result<i32> {
    crate::cmd_init(InitOptions { with_hook, with_claude_md, with_agents_md, force, dry_run, engine_url: None })
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
        // (#3020) The utility window is sized from the AI headroom doctor
        // reads off the live host; pin it, so no test depends on how much
        // memory this machine has free. Plenty, unless a test says otherwise.
        env.headroom(Some(1 << 50));
        // (#316) No server answers unless a test serves one, so a server this
        // machine runs (on 8080, or at a registry endpoint) cannot answer a test.
        env.set(crate::init_engine::TEST_SERVERS_HOOK, OsString::new());
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

    /// The AI headroom (bytes) `init` sizes the utility window from;
    /// `None` reads as "could not be measured".
    fn headroom(&self, bytes: Option<u64>) {
        set_ai_headroom_override(Some(bytes));
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
        set_ai_headroom_override(None);
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
fn a_dry_run_reports_intent_writes_nothing_and_asks_lms_only_whether_it_answers() {
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
        engine_url: None,
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
    // (#316) The plan names the engine, so the dry run probes it (`lms ps`);
    // it never asks for the catalog, which only the model fill reads.
    assert_eq!(env.lms_calls().lines().collect::<Vec<_>>(), vec!["ps --json"], "dry run asks only `lms ps`");
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
    // (#316) No engine was chosen yet (the placeholder is still there), so
    // the plan probes `lms ps`; it never reads the catalog.
    assert_eq!(env.lms_calls(), "ps --json\n");
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
    // (#316) No placeholder, so no engine probe (`may_detect`) and no loaded-model lookup.
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

/// The registry `text` as darkmux loads it, with every validation applied.
fn load(env: &InitEnv) -> darkmux_types::ProfileRegistry {
    darkmux_profiles::profiles::load_registry(Some(env.registry().to_str().unwrap()))
        .expect("the registry init wrote must load")
        .registry
}

/// (#3020) Seen on an 8 GB machine: LM Studio held no usable LLM (a Gemma
/// still downloading, an embedding model) and doctor reported 0 GB available
/// for AI. `init` left the shipped `qwen/qwen3-4b-instruct-2507` at 120,000
/// tokens registered. It must leave the utility unregistered and say why,
/// naming the fleet route.
#[test]
#[serial_test::serial]
fn an_8gb_machine_with_no_ai_headroom_registers_no_utility_model() {
    darkmux_types::run_in_own_process!();
    for (name, catalog) in [
        ("only an embedding", vec![model("text-embedding-nomic", GB / 10, "embedding")]),
        ("only toys", vec![model("toy-0.5b", GB / 2, "llm"), model("text-embedding-nomic", GB / 10, "embedding")]),
        ("a real LLM, but no headroom", vec![model("google/gemma-4-e4b", 6 * GB, "llm")]),
    ] {
        let env = InitEnv::new();
        env.write_registry(&example_without_placeholder());
        env.lms(&catalog, &[]);
        env.headroom(Some(0));

        let r = init(&opts()).unwrap();

        let reg = load(&env);
        assert_eq!(reg.utility_model_id(), None, "{name}: no utility registered: {}", env.read_registry());
        assert!(reg.validate().is_empty(), "{name}: {:?}", reg.validate());
        assert_eq!(r.utility_model_filled, None, "{name}");
        let reason = r.utility_model_unregistered_reason.clone().unwrap_or_default();
        assert!(reason.contains(&format!("`{}`", shipped_utility())), "{name}: names the id: {reason}");
        if name == "a real LLM, but no headroom" {
            assert!(reason.contains("0 GB available for AI"), "{reason}");
            assert!(reason.contains("profile@machine"), "{reason}");
        }
    }
}

/// (#3020) With some headroom, the shipped window shrinks to what the machine
/// can hold beside its worker, and only the window changes.
#[test]
#[serial_test::serial]
fn a_tight_machine_gets_the_utility_window_its_headroom_can_hold() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let registry = EXAMPLE_PROFILES_JSON.replace(PLACEHOLDER_MODEL_ID, "worker-8b");
    env.write_registry(&registry);
    let (worker, utility) = (5 * GB, 2 * GB);
    env.lms(&[model("worker-8b", worker, "llm"), model(shipped_utility(), utility, "llm")], &[]);
    let rate = darkmux_profiles::model_ledger::fallback_kv_rate_for_size(utility);
    env.headroom(Some(worker + utility + 2 * 64_000 * rate));

    let r = init(&opts()).unwrap();

    assert_eq!(r.utility_model_n_ctx, Some(64_000));
    assert_eq!(r.utility_model_unregistered_reason, None);
    assert_eq!(load(&env).utility_model_n_ctx(), Some(64_000));
    assert_eq!(env.read_registry(), registry.replacen("\"n_ctx\": 120000 }", "\"n_ctx\": 64000 }", 1));
}

/// (#3020) The floor is the smallest window a worker profile declares (the
/// shipped `fast` profile's 32000): a smaller utility window is not registered.
#[test]
#[serial_test::serial]
fn a_utility_window_below_the_smallest_worker_window_is_not_registered() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.write_registry(&example_without_placeholder());
    let utility = 2 * GB;
    env.lms(&[model(shipped_utility(), utility, "llm")], &[]);
    let rate = darkmux_profiles::model_ledger::fallback_kv_rate_for_size(utility);
    env.headroom(Some(utility + 2 * 20_000 * rate));

    let r = init(&opts()).unwrap();

    let reason = r.utility_model_unregistered_reason.unwrap_or_default();
    assert!(reason.contains("20000-token") && reason.contains("32000 tokens"), "{reason}");
    assert_eq!(load(&env).utility_model_id(), None);
}

/// (#3020) A window the operator set by hand is theirs: never resized, and
/// the binding never removed for RAM, however little headroom there is.
#[test]
#[serial_test::serial]
fn a_hand_set_utility_window_is_never_resized_or_removed() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let registry = example_without_placeholder().replacen("\"n_ctx\": 120000 }", "\"n_ctx\": 64000 }", 1);
    env.write_registry(&registry);
    env.lms(&[model(shipped_utility(), 2 * GB, "llm")], &[]);
    env.headroom(Some(0));

    let r = init(&opts()).unwrap();

    assert_eq!(r.utility_model_unregistered_reason, None);
    assert_eq!(r.utility_model_n_ctx, None);
    assert_eq!(env.read_registry(), registry);
}

/// (#3020 review) A hand-set window on the shipped id is the operator's
/// binding: with its model not downloaded, init keeps it and says so.
#[test]
#[serial_test::serial]
fn a_hand_set_window_whose_model_is_missing_is_kept_and_reported() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let registry = example_without_placeholder().replacen("\"n_ctx\": 120000 }", "\"n_ctx\": 64000 }", 1);
    env.write_registry(&registry);
    env.lms(&[model("text-embedding-nomic", GB / 10, "embedding")], &[]);

    let r = init(&opts()).unwrap();

    assert_eq!(env.read_registry(), registry, "the hand-set binding is untouched");
    assert_eq!(r.utility_model_unregistered_reason, None);
    let reason = r.utility_model_unfilled_reason.unwrap_or_default();
    assert!(reason.contains(&format!("`{}` is not downloaded", shipped_utility())), "{reason}");
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
            cmd_init_with(false, Some(doc.clone()), None, true, false)
        } else {
            cmd_init_with(false, None, Some(doc.clone()), true, false)
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
    assert_eq!(cmd_init_with(true, Some(md.clone()), Some(ag.clone()), false, true).unwrap(), 0);
    assert!(!env.registry().exists());

    // Real first run with every option, worker and utility both filled.
    env.lms(&[model("only-llm", 2 * GB, "llm")], &[]);
    assert_eq!(cmd_init_with(true, Some(md.clone()), Some(ag.clone()), false, false).unwrap(), 0);
    assert!(env.read_registry().contains("only-llm"));

    // Re-run: everything already present, one skill edited (protected).
    let edited = fs::read_dir(env.skills_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("darkmux-"))
        .unwrap()
        .join("SKILL.md");
    fs::write(&edited, "edit\n").unwrap();
    assert_eq!(cmd_init_with(true, Some(md.clone()), Some(ag.clone()), false, false).unwrap(), 0);
    assert_eq!(fs::read_to_string(&edited).unwrap(), "edit\n");

    // Forced: the edited skill overwritten.
    assert_eq!(cmd_init_with(false, None, None, true, false).unwrap(), 0);
    assert_ne!(fs::read_to_string(&edited).unwrap(), "edit\n");
    drop(env);

    // A fresh machine where neither model can be set.
    let env = InitEnv::new();
    env.lms(&[model("toy", GB / 2, "embedding")], &[]);
    assert_eq!(cmd_init_with(false, None, None, false, false).unwrap(), 0);
    assert!(env.read_registry().contains(PLACEHOLDER_MODEL_ID));
    drop(env);

    // The utility reason branch.
    let env = InitEnv::new();
    env.write_registry(&example_without_placeholder());
    env.lms(&[model("toy", GB / 2, "llm")], &[]);
    assert_eq!(cmd_init_with(false, None, None, false, false).unwrap(), 0);
}

// ── #3143: a failing `lms`, and an undetected RAM size ───────────────────

/// (#3143 item 2) An `lms` that runs but fails (`ls` exits non-zero) or
/// answers with something that is not a JSON list is a failure to ask, not
/// an empty catalog: the reason must say LM Studio could not be asked, never
/// that nothing fits.
#[test]
#[serial_test::serial]
fn a_failing_lms_ls_reads_as_could_not_ask_not_as_nothing_fits() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    // No `ls.json`: the stub's `ls` exits 1.
    let r = init(&opts()).unwrap();
    let reason = r.worker_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("could not ask LM Studio what is downloaded"), "{reason}");
    assert!(!reason.contains("fits in"), "{reason}");
    // The utility half stays quiet: the worker line already reported lms.
    assert_eq!(r.utility_model_unfilled_reason, None);
    assert_eq!(r.utility_model_filled, None);
    assert_eq!(env.read_registry(), EXAMPLE_PROFILES_JSON);

    // `ls` succeeds but prints something that is not JSON.
    fs::write(env.stub.path().join("ls.json"), "LM Studio is updating, try again\n").unwrap();
    let r = init(&opts()).unwrap();
    let reason = r.worker_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("could not ask LM Studio what is downloaded"), "{reason}");
    assert!(reason.contains("not a JSON list"), "{reason}");
    assert_eq!(r.utility_model_unfilled_reason, None);

    // An empty catalog is still an answer: "nothing fits", not "could not ask".
    env.lms(&[], &[]);
    let r = init(&opts()).unwrap();
    let reason = r.worker_model_unfilled_reason.expect("a reason");
    assert!(reason.starts_with("LM Studio has no downloaded LLM that fits in"), "{reason}");
}

/// (#3143 item 4) When the RAM size could not be detected (0), the reason
/// says so instead of claiming nothing fits in 0 GB.
#[test]
fn the_no_worker_reason_names_an_undetected_ram_size() {
    let known = no_worker_model_reason(64);
    assert!(known.contains("fits in 64 GB"), "{known}");
    let unknown = no_worker_model_reason(0);
    assert!(!unknown.contains("0 GB"), "{unknown}");
    assert!(unknown.contains("could not detect this machine's RAM"), "{unknown}");
}

/// (#3207 review) A shipped utility id the operator moved onto an UNMANAGED
/// endpoint is not LM Studio's to plan: init leaves the binding as written,
/// even when LM Studio has nothing that could serve it (which would otherwise
/// unbind it) and with no headroom (which would otherwise resize it).
#[test]
#[serial_test::serial]
fn a_utility_binding_on_an_unmanaged_endpoint_is_left_alone() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let mut v: Value = serde_json::from_str(&example_without_placeholder()).unwrap();
    v["endpoints"]["mlx-local"] = serde_json::json!({ "url": "http://127.0.0.1:8080/v1" });
    v["internal"]["utility"]["endpoint"] = serde_json::json!("mlx-local");
    let registry = serde_json::to_string_pretty(&v).unwrap();
    env.write_registry(&registry);
    env.lms(&[], &[]);
    env.headroom(Some(0));

    let r = init(&opts()).unwrap();

    assert_eq!(r.utility_model_unregistered_reason, None, "never unbound");
    assert_eq!(r.utility_model_unfilled_reason, None);
    assert_eq!((r.utility_model_filled.clone(), r.utility_model_n_ctx), (None, None));
    assert_eq!(load(&env).utility_model_id(), Some(shipped_utility()), "the binding is as written");
    assert!(env.read_registry().contains("\"endpoint\": \"mlx-local\""));
}

/// (#3207 re-review) A registry the loader accepts with one entry quarantined
/// (#1282) is the same registry to init: a mistyped profile elsewhere must not
/// switch the guard off and let init unbind an unmanaged utility binding.
#[test]
fn a_utility_binding_on_an_unmanaged_endpoint_is_left_alone_beside_a_quarantined_entry() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    let mut v: Value = serde_json::from_str(&example_without_placeholder()).unwrap();
    v["endpoints"]["mlx-local"] = serde_json::json!({ "url": "http://127.0.0.1:8080/v1" });
    v["internal"]["utility"]["endpoint"] = serde_json::json!("mlx-local");
    v["profiles"]["broken"] = serde_json::json!({ "models": "not-a-list" });
    let registry = serde_json::to_string_pretty(&v).unwrap();
    env.write_registry(&registry);
    env.lms(&[], &[]);
    env.headroom(Some(0));

    let r = init(&opts()).unwrap();

    assert_eq!(r.utility_model_unregistered_reason, None, "never unbound");
    assert_eq!(load(&env).utility_model_id(), Some(shipped_utility()), "the binding is as written");
    assert!(env.read_registry().contains("\"endpoint\": \"mlx-local\""));
}

/// (#316) Serve `GET /v1/models` listing `ids` to `requests` connections on a
/// loopback port, and let the probe reach it. Returns the `/v1` base URL.
fn serve_models(ids: &[&str], requests: usize) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let data: Vec<Value> = ids.iter().map(|id| serde_json::json!({ "id": id, "object": "model" })).collect();
    let body = serde_json::json!({ "object": "list", "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = std::thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    // SAFETY: every caller holds #[serial_test::serial] in its own process;
    // `InitEnv` set this variable first, so its drop restores it.
    unsafe { std::env::set_var(crate::init_engine::TEST_SERVERS_HOOK, &base) };
    (base, handle)
}

fn port_of(base: &str) -> &str {
    base.rsplit(':').next().unwrap().trim_end_matches("/v1")
}

fn config_engine(env: &InitEnv) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(env.config()).unwrap()).unwrap();
    v["default_engine"].as_str().map(str::to_string)
}

/// (#316, ENGINES.md 9.2) A machine whose only engine is an OpenAI-compatible
/// server gets it recorded: an endpoint in profiles.json, `default_engine` in
/// config.json naming it, and the worker filled from the one model it lists.
/// The result loads, and the setting resolves against it.
#[test]
#[serial_test::serial]
fn a_fresh_init_records_the_only_answering_server_and_plans_its_one_model() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    let (base, server) = serve_models(&["mlx-community/Qwen3-4B-4bit"], 1);

    let r = init(&opts()).unwrap();
    server.join().unwrap();

    let id = format!("local-{}", port_of(&base));
    let Some(Outcome::Chosen { endpoint, added: true, .. }) = &r.engine else { panic!("{:?}", r.engine) };
    assert_eq!(endpoint.as_deref(), Some(id.as_str()));
    assert_eq!(config_engine(&env).as_deref(), Some(id.as_str()));
    let reg = load(&env);
    assert_eq!(reg.endpoints[&id].url.as_deref(), Some(base.as_str()));
    let resolved = darkmux_types::default_engine::resolve(&id, darkmux_types::config_access::Source::Config, &reg);
    assert!(resolved.is_ok(), "{resolved:?}");
    assert_eq!(r.worker_model_filled.as_deref(), Some("mlx-community/Qwen3-4B-4bit"));
    assert!(!env.read_registry().contains(PLACEHOLDER_MODEL_ID));
    assert_eq!(r.models_engine.as_deref(), Some(id.as_str()));
    // (#316 finding B) The shipped utility the server does not list is
    // unbound, on disk, not kept for the server to fetch.
    let utility = r.utility_model_unregistered_reason.unwrap_or_default();
    assert!(utility.contains("does not list") && utility.contains(shipped_utility()), "{utility}");
    assert_eq!(r.utility_model_unfilled_reason, None);
    assert!(reg.internal.as_ref().and_then(|i| i.utility.as_ref()).is_none(), "the utility binding was kept");
    assert!(!env.lms_calls().contains("ls"), "LM Studio's catalog is not this engine's: {}", env.lms_calls());
}

#[test]
#[serial_test::serial]
fn a_server_listing_several_models_leaves_the_placeholder_and_names_them() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    let (_base, server) = serve_models(&["a/one", "b/two"], 1);

    let r = init(&opts()).unwrap();
    server.join().unwrap();

    assert_eq!(r.worker_model_filled, None);
    let reason = r.worker_model_unfilled_reason.unwrap_or_default();
    assert!(reason.contains("`a/one`, `b/two`") && reason.contains("context"), "{reason}");
    assert!(env.read_registry().contains(PLACEHOLDER_MODEL_ID));
}

/// Nothing answering writes no engine: the config keeps `lmstudio`, the
/// registry gains no endpoint, and the report names every address probed.
#[test]
#[serial_test::serial]
fn nothing_answering_records_no_engine_and_names_what_was_probed() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    // SAFETY: as in `serve_models`.
    unsafe { std::env::set_var(crate::init_engine::TEST_SERVERS_HOOK, &dead_url) };

    let r = init(&opts()).unwrap();

    let Some(Outcome::NoneAnswering { probed }) = &r.engine else { panic!("{:?}", r.engine) };
    assert!(probed.iter().any(|p| p.starts_with("LM Studio")) && probed.contains(&dead_url), "{probed:?}");
    assert_eq!(config_engine(&env).as_deref(), Some("lmstudio"));
    assert_eq!(env.read_registry(), EXAMPLE_PROFILES_JSON);
}

/// A re-run against an existing config keeps its engine and probes no
/// server: an engine down one morning must not move the machine.
#[test]
#[serial_test::serial]
fn a_rerun_keeps_the_configured_engine_unless_the_operator_names_one() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    init(&opts()).unwrap();
    assert_eq!(config_engine(&env).as_deref(), Some("lmstudio"));

    let (base, server) = serve_models(&["m/one"], 1);
    let kept = init(&opts()).unwrap();
    assert_eq!(kept.engine, Some(Outcome::Kept { engine: "lmstudio".to_string(), from: "config.json" }));
    assert_eq!(config_engine(&env).as_deref(), Some("lmstudio"));

    let named = init(&InitOptions { engine_url: Some(base.trim_end_matches("/v1").to_string()), ..opts() }).unwrap();
    server.join().unwrap();
    let id = format!("local-{}", port_of(&base));
    assert!(matches!(&named.engine, Some(Outcome::Chosen { endpoint: Some(e), .. }) if *e == id), "{:?}", named.engine);
    assert_eq!(config_engine(&env).as_deref(), Some(id.as_str()));
}

/// The dry run probes and prints the same plan, and writes nothing.
#[test]
#[serial_test::serial]
fn a_dry_run_names_the_server_it_would_record_and_writes_nothing() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    let (base, server) = serve_models(&["m/one"], 1);

    let r = init(&InitOptions { dry_run: true, ..opts() }).unwrap();
    server.join().unwrap();

    let engine = r.engine.expect("an engine outcome");
    let line = engine.lines(false).join("\n");
    assert!(line.contains(&format!("`local-{}`", port_of(&base))) && line.contains("would set"), "{line}");
    assert!(!env.registry().exists() && !env.config().exists());
}

/// (#316 review finding 1) `DARKMUX_DEFAULT_ENGINE` chose the engine, so a
/// fresh init keeps it: no server is recorded and no model planned on one.
#[test]
#[serial_test::serial]
fn the_variable_keeps_its_engine_and_refuses_an_engine_url() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    env.set("DARKMUX_DEFAULT_ENGINE", "lmstudio".into());
    let (base, server) = serve_models(&["m/one"], 1);

    let r = init(&opts()).unwrap();
    assert_eq!(r.engine, Some(Outcome::Kept { engine: "lmstudio".into(), from: "DARKMUX_DEFAULT_ENGINE" }));
    assert_eq!(r.worker_model_filled.as_deref(), Some("tiny-llm"), "planned on LM Studio");

    let named = init(&InitOptions { engine_url: Some(base.clone()), ..opts() }).unwrap();
    server.join().unwrap();
    let why = named.engine_unrecorded.unwrap_or_default();
    assert!(why.contains("DARKMUX_DEFAULT_ENGINE"), "{why}");
    assert_eq!(config_engine(&env).as_deref(), Some("lmstudio"));
    assert!(!env.read_registry().contains("local-"), "no half record");
}

/// (#316 review finding 2) A server that cannot be recorded is not planned
/// on either: here `DARKMUX_PROFILES` points dispatch at another registry,
/// so nothing is written and the worker comes from the configured engine.
#[test]
#[serial_test::serial]
fn a_server_that_cannot_be_recorded_writes_nothing_and_plans_nothing_on_it() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let elsewhere = env.home.path().join("other.json");
    env.set("DARKMUX_PROFILES", elsewhere.clone().into_os_string());
    let (base, server) = serve_models(&["m/one"], 1);

    let r = init(&InitOptions { engine_url: Some(base), ..opts() }).unwrap();
    server.join().unwrap();

    let why = r.engine_unrecorded.clone().unwrap_or_default();
    // Refused before any write, naming the variable (not found later by
    // `config set`'s own check, which would leave the registry to put back).
    assert!(why.starts_with("DARKMUX_PROFILES points dispatch at"), "{why}");
    assert!(r.engine.as_ref().unwrap().lines(false)[0].contains("would set"), "{:?}", r.engine);
    assert_eq!(config_engine(&env).as_deref(), Some("lmstudio"));
    assert!(!env.read_registry().contains("local-"), "no endpoint written");
    assert_eq!(r.models_engine, None);
    assert_eq!(r.worker_model_filled.as_deref(), Some("tiny-llm"));
}

/// (#316 review finding 3) A tuned registry with no config.json (deleted, or
/// from before config.json existed) is not moved to whatever answers.
#[test]
#[serial_test::serial]
fn a_tuned_registry_without_a_config_is_not_moved() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    let mine = example_without_placeholder();
    env.write_registry(&mine);
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    // SAFETY: as in `serve_models`.
    unsafe { std::env::set_var(crate::init_engine::TEST_SERVERS_HOOK, &url) };

    let r = init(&opts()).unwrap();

    assert_eq!(r.engine, Some(Outcome::Kept { engine: "lmstudio".into(), from: "config.json" }));
    assert_eq!(env.read_registry(), mine);
}

/// (#316 finding B) With the worker already set, unbinding the shipped
/// utility is the only change, and it still reaches the file.
#[test]
#[serial_test::serial]
fn an_unlisted_utility_is_unbound_on_disk_when_nothing_else_changes() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    env.write_registry(&EXAMPLE_PROFILES_JSON.replace(PLACEHOLDER_MODEL_ID, "m/one"));
    let (base, server) = serve_models(&["m/one"], 1);

    let r = init(&InitOptions { engine_url: Some(base), ..opts() }).unwrap();
    server.join().unwrap();

    assert_eq!(r.worker_model_filled, None, "the worker was already set");
    assert!(r.utility_model_unregistered_reason.is_some(), "{r:?}");
    let reg = load(&env);
    assert!(reg.internal.as_ref().and_then(|i| i.utility.as_ref()).is_none(), "the utility binding was kept on disk");
}

/// (#3287) A config.json that `config set` cannot write is found only after
/// the registry gained the endpoint: the registry is put back as it was, the
/// reason says so, and the models are planned on the engine still configured.
#[test]
#[serial_test::serial]
fn a_config_that_refuses_the_engine_puts_the_registry_back() {
    darkmux_types::run_in_own_process!();
    let mut env = InitEnv::new();
    env.lms_missing();
    fs::create_dir_all(env.config().parent().unwrap()).unwrap();
    fs::write(env.config(), "{ not json").unwrap();
    let (_base, server) = serve_models(&["m/one"], 1);

    let r = init(&opts()).unwrap();
    server.join().unwrap();

    let why = r.engine_unrecorded.clone().unwrap_or_default();
    assert!(why.ends_with("(profiles.json was put back)"), "{why}");
    assert_eq!(env.read_registry(), EXAMPLE_PROFILES_JSON, "the endpoint was taken back out");
    assert_eq!(fs::read_to_string(env.config()).unwrap(), "{ not json", "the config is not rewritten");
    assert_eq!(r.models_engine, None, "an engine not recorded is not planned on");
}

/// (#3287) On a re-run, the engine config.json names is the one the models
/// are planned from: its registry endpoint is asked what it serves, and a
/// name that is no endpoint, or an endpoint that does not answer, is the
/// reason the worker stays unfilled.
#[test]
#[serial_test::serial]
fn the_configured_server_is_the_one_models_are_planned_from() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    fs::write(env.config(), r#"{ "default_engine": "mlx" }"#).unwrap();
    let report = InitReport::default();

    env.write_registry(EXAMPLE_PROFILES_JSON);
    let ModelsEngine::Server { name, models } = models_engine(&report, &env.registry(), &env.config()) else {
        panic!("a server is configured")
    };
    let why = models.unwrap_err();
    assert_eq!(name, "mlx");
    assert!(why.contains("the default engine `mlx` is not an endpoint in"), "{why}");

    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    env.write_registry(&crate::init_engine::add_endpoint(EXAMPLE_PROFILES_JSON, "mlx", &dead_url).unwrap());
    let ModelsEngine::Server { models, .. } = models_engine(&report, &env.registry(), &env.config()) else {
        panic!("a server is configured")
    };
    let why = models.unwrap_err();
    assert!(why.contains(&format!("did not answer `GET /models` at {dead_url}")), "{why}");

    let (base, server) = serve_models(&["m/one"], 1);
    env.write_registry(&crate::init_engine::add_endpoint(EXAMPLE_PROFILES_JSON, "mlx", &base).unwrap());
    let ModelsEngine::Server { models, .. } = models_engine(&report, &env.registry(), &env.config()) else {
        panic!("a server is configured")
    };
    server.join().unwrap();
    assert_eq!(models, Ok(vec!["m/one".to_string()]));

    fs::write(env.config(), r#"{ "default_engine": " LMStudio " }"#).unwrap();
    assert!(matches!(models_engine(&report, &env.registry(), &env.config()), ModelsEngine::Lmstudio));
}

// ── #3143 item 5: a run that will fail writes nothing first ──────────────

/// (#3143 item 5) Every input `init` can refuse is checked before its first
/// write: a malformed CLAUDE.md or AGENTS.md block under `--force`, a
/// settings.json that is not an object (`--with-hook`), and a bad
/// `--engine-url`. Each refusal leaves the machine as it was: no registry,
/// no config, no skills, no hook, and the doc untouched. The error names the
/// file to fix.
#[test]
#[serial_test::serial]
fn a_refused_input_fails_init_before_anything_is_written() {
    darkmux_types::run_in_own_process!();
    let env = InitEnv::new();
    env.lms(&[model("tiny-llm", TINY, "llm")], &[]);
    let claude_md = env.doc("CLAUDE.md");
    let agents_md = env.doc("AGENTS.md");
    fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
    let no_end = |header: &str| format!("# Project\n\n{header}\n\nno end marker here.\n");
    fs::write(&claude_md, no_end(CLAUDE_MD_HEADER)).unwrap();
    fs::write(&agents_md, no_end(AGENTS_MD_HEADER)).unwrap();

    let cases: Vec<(&str, InitOptions, String)> = vec![
        (
            "malformed CLAUDE.md",
            InitOptions { with_claude_md: Some(claude_md.clone()), force: true, ..opts() },
            claude_md.display().to_string(),
        ),
        (
            "malformed AGENTS.md",
            InitOptions { with_agents_md: Some(agents_md.clone()), force: true, ..opts() },
            agents_md.display().to_string(),
        ),
        ("bad --engine-url", InitOptions { engine_url: Some("not-a-url".into()), ..opts() }, "--engine-url".into()),
    ];
    for (label, o, names) in cases {
        let err = init(&o).expect_err(label);
        assert!(format!("{err:#}").contains(&names), "{label}: the error names {names}: {err:#}");
        for p in [env.registry(), env.config(), env.settings(), env.skills_dir()] {
            assert!(!p.exists(), "{label}: {} was written before the refusal", p.display());
        }
    }
    assert_eq!(fs::read_to_string(&claude_md).unwrap(), no_end(CLAUDE_MD_HEADER));
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), no_end(AGENTS_MD_HEADER));

    fs::create_dir_all(env.settings().parent().unwrap()).unwrap();
    fs::write(env.settings(), "[]\n").unwrap();
    let err = init(&InitOptions { with_hook: true, ..opts() }).expect_err("settings.json is a list");
    assert!(format!("{err:#}").contains(&env.settings().display().to_string()), "names the settings file: {err:#}");
    for p in [env.registry(), env.config(), env.skills_dir()] {
        assert!(!p.exists(), "settings.json: {} was written before the refusal", p.display());
    }
    assert_eq!(fs::read_to_string(env.settings()).unwrap(), "[]\n");
}
