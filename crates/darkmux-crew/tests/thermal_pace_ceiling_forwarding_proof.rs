//! (#2774 round-8) Pins the ONE ASSIGNMENT that decides which reading of
//! `runtime.thermal.max_pause_ms` reaches the container.
//!
//! `dispatch_internal.rs` sets
//! `max_pause_ms_env: Some(config_access::thermal_pace_staleness_ceiling_ms())`.
//! That accessor resolves `0` — an UNBOUNDED pause EPISODE — to the built-in
//! default, because the container uses this number for a different job:
//! `runtime/src/pace.rs::is_expired` computes `now - written_at > n`, so a
//! literal `0` there means "honor a pause for 0 ms" — every pause ignored,
//! the runtime racing at full speed on a machine the host believes it has
//! stopped.
//!
//! **The assignment had no test.** Reverting it to `thermal_max_pause_ms()`
//! (the raw episode-cap reading) left all 1853 darkmux-crew tests green. The
//! only thing that reddened was `darkmux-types`'
//! `every_config_value_is_read_by_something_outside_this_file`, which its own
//! doc calls "a substring scan over TEXT, not a call-graph" — so one future
//! test that merely NAMES the accessor satisfies it forever, after which this
//! line can silently revert. `dispatch_internal_tests.rs`'s
//! `build_docker_run_argv_forwards_max_pause_ms_when_set` pins the BUILDER
//! given an explicit value; nothing pinned the value it is GIVEN.
//!
//! ## Why this is a `dispatch()`-level test and not a unit test
//!
//! The assignment lives inside `dispatch()`, in a `DockerRunConfig` literal
//! built from ~30 resolved locals well past the point where a real `docker
//! run` would normally be required. There is no seam between "resolve the
//! knob" and "hand it to the builder" to assert on — that IS the defect this
//! pins. So the only honest proof reads the argv `dispatch()` actually
//! constructs.
//!
//! ## No Docker daemon, no LMStudio, no model — and NOT `#[ignore]`d
//!
//! Its siblings in this directory (`mock_dispatch_proof.rs`,
//! `dispatch_panic_thread_leak_proof.rs`) are `#[ignore]`d because they need
//! a real Docker daemon, or drive three real OS threads to a panic. This one
//! needs neither: `PATH` is pointed at a fake `docker` that is a shell script
//! recording its own argv and exiting — the shim IS docker, so no daemon
//! exists to require — and at a fake `lms` answering `ps --json` with an
//! empty resident set, so the always-on telemetry sampler never reaches a
//! backend. `skip_preflight` plus `model_base_url_override` keep the
//! host-side residency blocks from calling `lms` at all.
//!
//! It runs in the default `cargo nextest run` tier deliberately: a pin that
//! CI does not execute does not pin anything, and the whole point of this
//! file is that the production line can otherwise revert in silence.

use serde_json::json;
use std::fs;
use std::path::Path;

use darkmux_crew::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts};

/// Restores a process-wide env var to its prior value on drop. Duplicated
/// from `dispatch_panic_thread_leak_proof.rs` / `mock_dispatch_proof.rs`
/// rather than shared — each `tests/*.rs` file compiles as its own binary
/// and there is no `tests/common/` module yet (see those files' own note on
/// the same tradeoff).
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: this test is `#[serial_test::serial]`; no other test in
        // this binary reads or writes these keys concurrently.
        unsafe { std::env::set_var(key, value) };
        EnvVarGuard { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// One LOCAL model, no compactor bound — same shape as
/// `mock_dispatch_proof.rs`'s `write_mock_profiles_registry`.
fn write_profiles_registry(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("profiles.json");
    let body = json!({
        "schema_version": "1.5",
        "default_profile": "mock",
        "profiles": {
            "mock": { "models": [ { "id": "mock-model", "n_ctx": 8192 } ] }
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&body).unwrap())
        .expect("writing temp profiles.json");
    path
}

/// A fake `docker` that appends its own argv to `record` and exits, and a
/// fake `lms` that answers `ps --json` with an empty resident set. Same
/// idiom as `dispatch_internal_tests.rs`'s `install_fake_docker`.
fn install_fake_docker_and_lms(dir: &Path, record: &Path, lms_ps_json: &str) -> std::path::PathBuf {
    let fake_docker = dir.join("docker");
    fs::write(
        &fake_docker,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n", record.display()),
    )
    .expect("writing fake docker");

    let fake_lms = dir.join("lms");
    fs::write(
        &fake_lms,
        format!("#!/bin/sh\nif [ \"$1\" = \"ps\" ]; then\necho '{lms_ps_json}'\nexit 0\nfi\nexit 0\n"),
    )
    .expect("writing fake lms");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&fake_docker, &fake_lms] {
            let mut perms = fs::metadata(p).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(p, perms).unwrap();
        }
    }

    fake_lms
}

/// Drive one `dispatch()` through the PATH-shimmed `docker` with
/// `DARKMUX_THERMAL_MAX_PAUSE_MS` set to `max_pause_ms`, and return every
/// argv line the shim recorded.
fn captured_docker_argv(max_pause_ms: &str) -> String {
    captured_docker_argv_with(max_pause_ms, Some("http://127.0.0.1:1/v1"), None, "[]")
}

/// The general form: `base_url_override` is `DispatchOpts::
/// model_base_url_override`; `lmstudio_url` (when `Some`) is exported as
/// `DARKMUX_LMSTUDIO_URL`; `lms_ps_json` is what the fake `lms ps --json`
/// prints (a resident set lets the host-side residency path, which runs
/// whenever there is no override, find the model already loaded).
fn captured_docker_argv_with(
    max_pause_ms: &str,
    base_url_override: Option<&str>,
    lmstudio_url: Option<&str>,
    lms_ps_json: &str,
) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home_dir = tmp.path().join("home");
    let flows_dir = tmp.path().join("flows");
    let ack_dir = tmp.path().join("ack");
    let fake_bin_dir = tmp.path().join("fake-bin");
    for d in [&home_dir, &flows_dir, &ack_dir, &fake_bin_dir] {
        fs::create_dir_all(d).unwrap();
    }
    let record = tmp.path().join("docker-argv.txt");
    let profiles_path = write_profiles_registry(tmp.path());
    let fake_lms = install_fake_docker_and_lms(&fake_bin_dir, &record, lms_ps_json);

    let real_path = std::env::var("PATH").unwrap_or_default();

    // DARKMUX_HOME, never DARKMUX_CREW_DIR — `user_state_root()` resolves
    // against HOME, and pinning the crew dir instead would defeat this
    // test's own isolation from the operator's real `~/.darkmux`.
    let _home = EnvVarGuard::set("DARKMUX_HOME", &home_dir);
    let _flows = EnvVarGuard::set("DARKMUX_FLOWS_DIR", &flows_dir);
    let _ack = EnvVarGuard::set("DARKMUX_ACK_DIR", &ack_dir);
    let _lms = EnvVarGuard::set("DARKMUX_LMS_BIN", &fake_lms);
    let _path = EnvVarGuard::set("PATH", format!("{}:{real_path}", fake_bin_dir.display()));
    let _thermal = EnvVarGuard::set("DARKMUX_THERMAL_MAX_PAUSE_MS", max_pause_ms);
    let _lmstudio_url = lmstudio_url.map(|u| EnvVarGuard::set("DARKMUX_LMSTUDIO_URL", u));

    let opts = DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: "analyst".to_string(),
        message: "#2774 pace-ceiling forwarding proof — never reaches a model.".to_string(),
        session_id: Some(format!(
            "thermal-pace-ceiling-forwarding-proof-{}-{max_pause_ms}",
            std::process::id()
        )),
        timeout_seconds: 60,
        skip_preflight: true,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: CompactionDispatchArgs::default(),
        profile_name: Some("mock".to_string()),
        config_path: Some(profiles_path.to_string_lossy().to_string()),
        force_container: false,
        max_completion_tokens: None,
        image: None,
        // Routes past the host-side compactor-residency block entirely, so
        // the only `lms` call possible is the sampler's own `ps --json`.
        // The URL is never dialed: the fake `docker` never runs the real
        // runtime binary that would try.
        model_base_url_override: base_url_override.map(str::to_string),
        step_id: None,
        system_prompt_override: None,
    };

    // The dispatch itself FAILS (the shim produces no result envelope) and
    // that is fine — the artifact under test is the argv, which is built
    // and handed to `docker` before any of that matters.
    let _ = dispatch(opts);

    fs::read_to_string(&record).unwrap_or_default()
}

/// The line the shim recorded for the `docker run` invocation — the only
/// one carrying the env block. Other invocations (`docker kill` from the
/// container-kill guard) also land in the record file.
fn docker_run_line(recorded: &str) -> &str {
    recorded
        .lines()
        .find(|l| l.contains("DARKMUX_INACTIVITY_TIMEOUT_SECONDS="))
        .unwrap_or_else(|| {
            panic!(
                "no `docker run` invocation reached the PATH shim — this test proves nothing \
                 about the value forwarded. recorded:\n{recorded}"
            )
        })
}

/// THE assertion. `DARKMUX_THERMAL_MAX_PAUSE_MS=0` means an unbounded pause
/// EPISODE on the host; what the container must be handed is the STALENESS
/// CEILING, which substitutes the built-in default rather than forwarding a
/// literal `0`.
#[test]
#[serial_test::serial] // mutates PATH, DARKMUX_HOME, DARKMUX_LMS_BIN and the thermal knob
fn an_unbounded_max_pause_forwards_the_default_staleness_ceiling_not_a_literal_zero() {
    let recorded = captured_docker_argv("0");
    let run_line = docker_run_line(&recorded);

    assert!(
        !run_line.contains("DARKMUX_MAX_PAUSE_MS=0 ")
            && !run_line.ends_with("DARKMUX_MAX_PAUSE_MS=0"),
        "forwarding a literal 0 tells the container to expire every pause instantly \
         (`now - written_at > 0`), so the runtime keeps working on a machine the host has \
         stopped: {run_line}"
    );
    let expected = format!(
        "DARKMUX_MAX_PAUSE_MS={}",
        darkmux_types::config_access::THERMAL_MAX_PAUSE_MS_DEFAULT
    );
    assert!(
        run_line.contains(&expected),
        "expected the staleness-ceiling reading ({expected}) in the container's env block: \
         {run_line}"
    );
}

/// The non-degenerate direction, pinned so the assignment above cannot be
/// "fixed" by hardcoding the default and ignoring the operator's value.
#[test]
#[serial_test::serial]
fn a_finite_max_pause_forwards_the_operators_own_value() {
    let recorded = captured_docker_argv("120000");
    let run_line = docker_run_line(&recorded);
    assert!(
        run_line.contains("DARKMUX_MAX_PAUSE_MS=120000"),
        "a finite cap is both the episode cap AND the staleness ceiling — it must reach the \
         container verbatim: {run_line}"
    );
}

/// (#2904) `dispatch()` hands the container the configured LMStudio URL,
/// translated for Docker, when no mock override is set. Pins the ONE
/// assignment in `dispatch()` (`base_url_override:
/// container_lmstudio_base_url(...)`) that the unit tests in
/// `dispatch_internal_tests.rs` cannot reach: reverting it to
/// `opts.model_base_url_override.clone()` drops `--base-url` from the argv
/// and the container falls back to the runtime's `:1234` default.
///
/// No override means the host-side residency path runs, so the fake `lms`
/// reports the profile's model as already resident under darkmux's
/// namespace at the profile's `n_ctx` — nothing is loaded, and the URL is
/// never dialed (the fake `docker` never starts the runtime).
#[test]
#[serial_test::serial]
fn a_configured_lmstudio_url_reaches_the_container_translated_for_docker() {
    let resident = r#"[{"identifier":"darkmux:mock-model","modelKey":"mock-model","contextLength":8192,"status":"idle"}]"#;
    let recorded = captured_docker_argv_with("120000", None, Some("http://localhost:4321"), resident);
    // Proves a `docker run` reached the shim. The runtime args follow the
    // multi-line `--system` prompt, so they land on later record lines:
    // assert against the whole record, not just the env-block line.
    let _ = docker_run_line(&recorded);
    assert!(
        recorded.contains("--base-url http://host.docker.internal:4321/v1"),
        "the configured lmstudio_url must reach the container as its --base-url: {recorded}"
    );
}
