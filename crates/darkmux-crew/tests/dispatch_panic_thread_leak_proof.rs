//! External, whole-`dispatch()`-level proof that a panic mid-run leaves no
//! tailer/sampler/watchdog thread behind (#2642).
//!
//! #2234's Test section asked for two things: unit-level proof that each
//! panic-safety guard fires on unwind (shipped in #2636/#2641 — see
//! `dispatch_internal_tests.rs`'s `spawn_guarded_{tailer,sampler,watchdog}_
//! wiring_survives_a_real_thread_spawn`), and an EXTERNAL, whole-`dispatch()`
//! proof: panic a dispatch mid-run in a process that survives, and assert no
//! tailer/sampler/watchdog thread remains. This file is the second half.
//!
//! Every closed proof before this one panicked INSIDE a function that only
//! constructs one guard and calls one spawn helper — never inside a real
//! `dispatch()` call driving its own actual control flow end to end. This
//! test calls the exact production `darkmux_crew::dispatch::dispatch`
//! (`darkmux dispatch`'s own entry point — see `dispatch.rs`: it is a
//! one-line forward into `dispatch_internal::dispatch`), injects a real
//! panic at the one point `dispatch_internal.rs` names for this purpose
//! (see that file's own `DARKMUX_TEST_PANIC_AFTER_GUARD_SPAWN` comment —
//! after all three `StopFlagGuard`s are armed and their threads are
//! genuinely running, before any of `dispatch()`'s own stop-and-join calls),
//! and asserts from OUTSIDE the function that the three threads it spawned
//! are gone.
//!
//! ## The shape chosen: a process-wide counter (#2642 option 1)
//!
//! `dispatch()` hands no thread handle to its caller — `tailer_handle` /
//! `sampler_handle` / `watchdog_handle` are local to its own stack frame,
//! joined before an ORDINARY return. So there is no cheap external signal to
//! assert on by default. This test uses the counter #2642 named as the
//! preferred shape: `darkmux_crew::concurrent_dispatch::
//! active_detached_thread_count()` reads a process-wide `AtomicUsize` that
//! every thread `spawn_detached_named` starts increments on its own first
//! line and decrements on `Drop` — see that function's own doc in
//! `concurrent_dispatch.rs`. Reasons to prefer this over the alternative
//! #2642 named (a `#[cfg(test)]`-only variant of `dispatch()` that returns
//! its join handles instead of joining them): it does not change
//! `dispatch()`'s public return shape, it is REAL, unconditional
//! instrumentation that cannot be compiled out of the production build (an
//! integration test links this crate as an ordinary dependency, built
//! WITHOUT `cfg(test)`, so a `#[cfg(test)]`-gated counter would not even
//! exist in the binary this test calls into), and it asserts the property
//! DIRECTLY — no thread remains — rather than a proxy for it.
//!
//! ## No real Docker, no real LMStudio
//!
//! Per this repo's standing guardrails (this worktree's task brief: "this
//! work uses the Docker-mocked path only"; CLAUDE.md's "No local models
//! unattended"), this test never touches a real Docker daemon or a real
//! LMStudio. `PATH` is pointed at a fake `docker` (same idiom as
//! `dispatch_internal_tests.rs`'s `install_fake_docker` and
//! `mock_dispatch_proof.rs`'s `write_docker_argv_capture_wrapper` — a
//! `#!/bin/sh\nexit 0\n` stand-in is sufficient because `child.spawn()`
//! only needs an executable to exist; nothing here ever reads its output,
//! since the injected panic fires before `wait_with_output()` is called)
//! and `DARKMUX_LMS_BIN` is pointed at a fake `lms` (the exact idiom
//! `tests/cli.rs`'s `dispatch_host_side_unset_compactor_disclosure_fires_
//! on_the_local_path` already uses at the repo root, reproduced here per
//! that test's own note that a cross-crate dev-dependency on a test-only
//! helper would be backwards) so the always-on telemetry sampler thread's
//! periodic `lms ps --json` never reaches a real backend. `--skip-preflight`
//! plus a `model_base_url_override` (mirroring `mock_dispatch_proof.rs`)
//! keep the host-side compactor-residency block from calling `lms` at all
//! either — the only `lms` calls possible on this path are the sampler's.
//!
//! ## Docker-gated tier, by direction not by necessity
//!
//! This test does not literally require a running Docker daemon (the PATH
//! shim replaces `docker` entirely) — but it is `#[ignore]`d and lives
//! alongside `mock_dispatch_proof.rs` per this project's own convention for
//! dispatch()-level integration proofs: it drives the full production
//! `dispatch_internal::dispatch()` control flow end to end (real subprocess
//! spawn, three real OS threads, real flow-record writes to an isolated
//! `DARKMUX_HOME`), which is meaningfully heavier than the fast no-Docker
//! unit tier even without a real container, and grouping it with its
//! siblings keeps that whole class of test out of the default `cargo
//! nextest run` loop. Run explicitly:
//!
//! ```sh
//! cargo test -p darkmux-crew --test dispatch_panic_thread_leak_proof -- --ignored --nocapture
//! ```

use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use darkmux_crew::concurrent_dispatch::active_detached_thread_count;
use darkmux_crew::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts};

/// Restores a process-wide env var to its prior value on drop. Duplicated
/// from `mock_dispatch_proof.rs`'s own `EnvVarGuard` rather than shared —
/// each `tests/*.rs` file compiles as its own binary and there is no
/// `tests/common/` module yet (see that file's own comment on the same
/// tradeoff). Panic-safe: even though every mutation this test makes is
/// restored BEFORE the injected panic fires (the panic happens inside
/// `dispatch()`, after every guard here has already been set), this still
/// runs Drop on an unexpected early panic elsewhere in the test body.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: this whole test is `#[serial_test::serial]`; no other
        // test in this binary reads or writes these keys concurrently.
        unsafe { std::env::set_var(key, value) };
        EnvVarGuard { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Write a minimal profiles registry naming exactly one LOCAL model, no
/// compactor bound — same shape as `mock_dispatch_proof.rs`'s
/// `write_mock_profiles_registry`, reproduced here for the same
/// no-shared-`tests/common/` reason as `EnvVarGuard` above.
fn write_profiles_registry(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("profiles.json");
    let body = json!({
        "schema_version": "1.5",
        "default_profile": "mock",
        "profiles": {
            "mock": {
                "models": [
                    { "id": "mock-model", "n_ctx": 8192 }
                ]
            }
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).expect("writing temp profiles.json");
    path
}

/// Drop a fake `docker` (unconditional `exit 0`, matching
/// `dispatch_internal_tests.rs`'s `install_fake_docker` / `tests/cli.rs`'s
/// `dispatch_host_side_unset_compactor_disclosure_fires_on_the_local_path`
/// idiom) and a fake `lms` (answers `ps --json` with an empty resident-set,
/// exits 0 for anything else) into `dir`. Returns their paths.
fn install_fake_docker_and_lms(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let fake_docker = dir.join("docker");
    fs::write(&fake_docker, "#!/bin/sh\nexit 0\n").expect("writing fake docker");

    let fake_lms = dir.join("lms");
    fs::write(
        &fake_lms,
        "#!/bin/sh\n\
         if [ \"$1\" = \"ps\" ]; then\n\
         echo '[]'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
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

    (fake_docker, fake_lms)
}

/// Poll [`active_detached_thread_count`] until it returns to `baseline`,
/// with an explicit deadline — per this task's own hard requirement, a
/// regression must FAIL this test, never hang the suite. `deadline` is
/// generous relative to the threads' own poll granularities (the tailer
/// and watchdog notice an abandonment within ~500ms; the sampler's fake
/// `lms ps --json` above returns in milliseconds rather than the real
/// `lms`'s documented up-to-~30s worst case) precisely so a genuine
/// regression — a guard's construction or Drop wiring deleted — reads as a
/// clear, bounded FAILURE rather than an indefinite hang.
fn wait_for_thread_count(baseline: usize, deadline: Duration) -> Result<(), usize> {
    let start = Instant::now();
    loop {
        let current = active_detached_thread_count();
        if current == baseline {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(current);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "heavy dispatch()-level proof — drives the real dispatch_internal::dispatch() \
            control flow end to end (real subprocess spawn, three real OS threads); \
            grouped with mock_dispatch_proof.rs's Docker-gated tier per project convention \
            rather than the fast unit tier, even though PATH-shimmed docker/lms mean it \
            needs no live daemon"]
#[serial_test::serial] // mutates PATH, DARKMUX_LMS_BIN, DARKMUX_HOME, and the panic hook
fn dispatch_panic_mid_run_leaves_no_tailer_sampler_watchdog_thread() {
    let baseline = active_detached_thread_count();
    assert_eq!(
        baseline, 0,
        "test harness assumption: this process has spawned no darkmux-managed detached \
         thread yet — a non-zero baseline means an earlier test in this binary leaked one, \
         which this test cannot distinguish from the regression it exists to catch"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&home_dir).unwrap();
    let flows_dir = tmp.path().join("flows");
    fs::create_dir_all(&flows_dir).unwrap();
    let ack_dir = tmp.path().join("ack");
    fs::create_dir_all(&ack_dir).unwrap();
    let fake_bin_dir = tmp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();

    let profiles_path = write_profiles_registry(tmp.path());
    let (_fake_docker, fake_lms) = install_fake_docker_and_lms(&fake_bin_dir);

    let real_path = std::env::var("PATH").unwrap_or_default();

    // (Hard constraint, this task's own brief) Pin DARKMUX_HOME, never
    // DARKMUX_CREW_DIR — DARKMUX_HOME is what `user_state_root()` actually
    // resolves against; DARKMUX_CREW_DIR outranks it and pinning that
    // instead would defeat this test's own isolation from a real
    // `~/.darkmux`.
    let _home_guard = EnvVarGuard::set("DARKMUX_HOME", &home_dir);
    let _flows_guard = EnvVarGuard::set("DARKMUX_FLOWS_DIR", &flows_dir);
    let _ack_guard = EnvVarGuard::set("DARKMUX_ACK_DIR", &ack_dir);
    // No real LMStudio, ever (standing guardrail) — the always-on
    // telemetry sampler shells out to `lms ps --json` on its own
    // background thread regardless of this dispatch's profile, so this is
    // required even though the dispatch itself names no compactor.
    let _lms_guard = EnvVarGuard::set("DARKMUX_LMS_BIN", &fake_lms);
    let _path_guard = EnvVarGuard::set("PATH", format!("{}:{real_path}", fake_bin_dir.display()));
    // The panic-injection trigger dispatch_internal.rs's own comment
    // documents (search that file for this exact var name).
    let _panic_guard = EnvVarGuard::set("DARKMUX_TEST_PANIC_AFTER_GUARD_SPAWN", "1");

    let opts = DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: "analyst".to_string(),
        message: "#2642 thread-leak proof — never actually reaches a model.".to_string(),
        session_id: Some(format!("dispatch-panic-thread-leak-proof-{}", std::process::id())),
        // Large relative to this test's own bounded wait below — the
        // watchdog must never fire on ITS OWN deadline during this test;
        // it fires because `_watchdog_abandon_guard`'s Drop marks it
        // abandoned when the panic unwinds, not because time ran out.
        timeout_seconds: 3600,
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
        // (mirrors mock_dispatch_proof.rs) Routes past the host-side
        // compactor-residency block entirely, so the only `lms` call this
        // dispatch can possibly make is the sampler's own periodic
        // `ps --json` — the fake `lms` above covers it. The URL itself is
        // never dialed: the fake `docker` never runs the real runtime
        // binary that would try.
        model_base_url_override: Some("http://127.0.0.1:1/v1".to_string()),
        step_id: None,
        system_prompt_override: None,
    };

    // Silence the expected panic's default backtrace print so test output
    // stays readable — same convention as
    // `dispatch_internal_tests.rs`'s `stop_flag_guard_fires_on_panic_unwind`
    // and `container_kill_guard_fires_on_panic_unwind`.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(opts)));
    std::panic::set_hook(prev_hook);

    assert!(
        outcome.is_err(),
        "dispatch() must actually have panicked — if this is Ok/Err instead, the \
         DARKMUX_TEST_PANIC_AFTER_GUARD_SPAWN hook in dispatch_internal.rs did not fire, \
         and this test is not exercising the panic path it exists to prove at all"
    );

    // THE assertion this whole file exists for: poll the process-wide
    // counter back to baseline within a bounded wait. A hang here is not
    // possible — `wait_for_thread_count` returns `Err` at its deadline
    // rather than looping forever, so a real regression FAILS the test
    // instead of wedging the suite (this task's own hard requirement).
    match wait_for_thread_count(baseline, Duration::from_secs(20)) {
        Ok(()) => {}
        Err(still_alive) => panic!(
            "expected all detached threads spawned by the panicked dispatch() to have exited \
             within 20s of the panic; {still_alive} still alive (baseline was {baseline}) — a \
             tailer/sampler/watchdog thread leaked past its guard's Drop"
        ),
    }
}
