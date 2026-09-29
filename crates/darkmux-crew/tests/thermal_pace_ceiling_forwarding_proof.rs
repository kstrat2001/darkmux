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

/// Env vars the stable fakes read their per-call parameters from. The
/// fakes inherit them from this process (set with [`EnvVarGuard`]).
const RECORD_VAR: &str = "THERMAL_PROOF_DOCKER_RECORD";
const LMS_PS_VAR: &str = "THERMAL_PROOF_LMS_PS_JSON";

/// A fake `docker` that appends its own argv to `$THERMAL_PROOF_DOCKER_RECORD`
/// and exits (answering `image inspect` with a runtime image built for this
/// darkmux, so the dispatch's image gate passes and the `docker run` argv
/// under test is reached), and a fake `lms` that answers `ps --json` with
/// `$THERMAL_PROOF_LMS_PS_JSON`.
///
/// (#2923) The scripts are written ONCE per build, at a stable path, and
/// parameterized through the environment, never rewritten per test. macOS
/// runs an XProtect assessment on the first exec of every newly written
/// executable, and those assessments serialize: measured on this machine,
/// 48 concurrent first-execs of fresh scripts took up to 10.4s and 200 took
/// up to 21.2s, while re-execs of an already-assessed script took 0.02s and
/// `sh <fresh file>` (no exec of the new file) 0.05s. A per-test fresh fake
/// therefore turned a loaded run into a 15s `docker image inspect` timeout
/// (`runtime_image::INSPECT_TIMEOUT`) with an empty record. The bound is
/// right for a real, long-installed `docker`; the fake was what was slow.
fn stable_fake_bin() -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("thermal-proof-fake-bin");
    fs::create_dir_all(&dir).expect("creating the fake bin dir");
    let docker = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${RECORD_VAR}\"\n\
         if [ \"$1\" = image ] && [ \"$2\" = inspect ]; then echo 'sha256:fake|{}'; fi\nexit 0\n",
        env!("CARGO_PKG_VERSION")
    );
    let lms = format!(
        "#!/bin/sh\nif [ \"$1\" = \"ps\" ]; then\nprintf '%s\\n' \"${LMS_PS_VAR}\"\nexit 0\nfi\nexit 0\n"
    );
    for (name, body) in [("docker", docker), ("lms", lms)] {
        write_executable_once(&dir.join(name), &body);
    }
    dir
}

/// Write `body` to `path` as an executable ONLY when the file is absent or
/// differs, via a same-directory temp file and an atomic rename, so parallel
/// test processes never exec a half-written file and an unchanged fake is
/// never re-created (a re-created file is a new file to XProtect).
fn write_executable_once(path: &Path, body: &str) {
    if fs::read_to_string(path).ok().as_deref() == Some(body) {
        return;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, body).expect("writing a fake");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::rename(&tmp, path).expect("publishing a fake");
}

/// Pins the #2923 fix: a second request for the fakes must not re-create
/// them (a re-created file is re-assessed by XProtect on its next exec).
#[test]
fn the_fakes_are_written_once_not_per_test() {
    let first = stable_fake_bin();
    let modified = |d: &Path| fs::metadata(d.join("docker")).unwrap().modified().unwrap();
    let before = modified(&first);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = stable_fake_bin();
    assert_eq!(first, second, "one stable location");
    assert_eq!(before, modified(&second), "an unchanged fake must not be rewritten");
}

/// Drive one `dispatch()` through the PATH-shimmed `docker` with
/// `DARKMUX_THERMAL_MAX_PAUSE_MS` set to `max_pause_ms`, and return every
/// argv line the shim recorded.
fn captured_docker_argv(max_pause_ms: &str) -> String {
    captured_docker_argv_with(max_pause_ms, Some("http://127.0.0.1:1/v1"), None, "[]", None)
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
    remote_origin: Option<&str>,
) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home_dir = tmp.path().join("home");
    let flows_dir = tmp.path().join("flows");
    let ack_dir = tmp.path().join("ack");
    for d in [&home_dir, &flows_dir, &ack_dir] {
        fs::create_dir_all(d).unwrap();
    }
    let record = tmp.path().join("docker-argv.txt");
    let profiles_path = write_profiles_registry(tmp.path());
    let fake_bin_dir = stable_fake_bin();
    let fake_lms = fake_bin_dir.join("lms");
    let _record = EnvVarGuard::set(RECORD_VAR, &record);
    let _lms_ps = EnvVarGuard::set(LMS_PS_VAR, lms_ps_json);

    let real_path = std::env::var("PATH").unwrap_or_default();

    // DARKMUX_HOME: `user_state_root()` resolves against it.
    let _home = EnvVarGuard::set("DARKMUX_HOME", &home_dir);
    let _flows = EnvVarGuard::set("DARKMUX_FLOWS_DIR", &flows_dir);
    let _ack = EnvVarGuard::set("DARKMUX_ACK_DIR", &ack_dir);
    let _lms = EnvVarGuard::set("DARKMUX_LMS_BIN", &fake_lms);
    let _path = EnvVarGuard::set("PATH", format!("{}:{real_path}", fake_bin_dir.display()));
    let _thermal = EnvVarGuard::set("DARKMUX_THERMAL_MAX_PAUSE_MS", max_pause_ms);
    let _lmstudio_url = lmstudio_url.map(|u| EnvVarGuard::set("DARKMUX_LMSTUDIO_URL", u));

    let opts = DispatchOpts {
        // (#2914) Work never runs on the utility model.
        allow_utility_model: false,
        remote_origin: remote_origin.map(str::to_string),
        live_channel: true,
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: "analyst".to_string(),
        message: "#2774 pace-ceiling forwarding proof — never reaches a model.".to_string(),
        session: darkmux_types::session_id::SessionId::adhoc(darkmux_types::session_id::RunId::standalone("test").unwrap(), "coder", format!(
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
    let recorded = captured_docker_argv_with("120000", None, Some("http://localhost:4321"), resident, None);
    // Proves a `docker run` reached the shim. The runtime args follow the
    // multi-line `--system` prompt, so they land on later record lines:
    // assert against the whole record, not just the env-block line.
    let _ = docker_run_line(&recorded);
    assert!(
        recorded.contains("--base-url http://host.docker.internal:4321/v1"),
        "the configured lmstudio_url must reach the container as its --base-url: {recorded}"
    );
}

/// (#2923 review C6) The container runs by the content id the image gate
/// checked, never by a tag that could be re-pointed between the check and
/// `docker run`. The shim's `image inspect` answers `sha256:fake` for a
/// matching `darkmux-runtime:latest`, so that id, and not the tag, must be
/// the image `docker run` is handed.
#[test]
#[serial_test::serial] // mutates PATH, DARKMUX_HOME, DARKMUX_LMS_BIN and the thermal knob
fn the_container_runs_by_the_checked_image_id_not_by_tag() {
    let recorded = captured_docker_argv("1000");
    let run_line = docker_run_line(&recorded);
    assert!(run_line.contains(" -- sha256:fake "), "runs by id: {run_line}");
    assert!(
        !run_line.contains("darkmux-runtime:latest"),
        "a tag can be re-pointed after the check: {run_line}"
    );
}

/// (#2916 re-review C2) Pins the CALL SITE, not just the helper: the argv
/// `dispatch()` really builds for a job another machine submitted mounts no
/// shared toolchain cache, while a local dispatch still does.
#[test]
#[serial_test::serial]
fn a_remote_origin_dispatch_really_mounts_no_shared_cache() {
    let local = captured_docker_argv_with("1000", Some("http://127.0.0.1:1/v1"), None, "[]", None);
    assert!(docker_run_line(&local).contains(":/darkmux-cache"), "a local dispatch keeps its cache mount");
    let remote = captured_docker_argv_with("1000", Some("http://127.0.0.1:1/v1"), None, "[]", Some("laptop"));
    let line = docker_run_line(&remote);
    assert!(!line.contains("darkmux-cache"), "a remote-origin dispatch must not mount the shared cache: {line}");
}
