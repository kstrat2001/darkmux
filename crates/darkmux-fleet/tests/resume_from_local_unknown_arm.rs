//! (#2584 follow-up) Runtime proof for the `RoutingDecision::Remote {
//! local_unknown: true }` arm of `dispatch_routed_via` — the arm the PR's
//! own comments and the sibling structural conformance test in
//! `routing.rs` both called "not producible portably" from a unit test.
//!
//! That claim is false; it was only unreachable from the EXISTING unit
//! test binary. `darkmux_flow::resolve_machine_id()` caches the
//! `hostname(1)` shell-out result in a process-wide `OnceLock` the first
//! time anything reaches that line with no `DARKMUX_MACHINE_ID` resolved.
//! Every test in `routing.rs`'s inline `mod tests` runs in the SAME
//! process, and several of them call `dispatch_routed_via` (transitively
//! calling `resolve_machine_id`) with a real, resolvable machine id — so
//! by the time any test wanted `local_unknown: true`, the `OnceLock` was
//! already frozen at `Some(<real hostname>)` for the rest of that
//! process's life, no matter what env is mutated afterward.
//!
//! A SEPARATE integration-test binary (this file) starts with a fresh,
//! uninitialized `OnceLock`. Forcing the arm from here needs only:
//!
//! 1. `DARKMUX_MACHINE_ID` unset (so `config_access::machine_id()` falls
//!    through to the hostname fallback) — and this crate's `[dev-dependencies]`
//!    already build `darkmux-types` with its `test-support` feature, which
//!    empties the config tier by construction (#811), so there is no
//!    `config.machine_id` to also unset.
//! 2. The `hostname` shell-out itself failing to spawn. Emptying `PATH`
//!    (a single empty-string entry, which POSIX `execvp` treats as "search
//!    only the current directory") is enough — `hostname` isn't in this
//!    test binary's working directory, so `Command::new("hostname").output()`
//!    returns `Err`, `.ok()` collapses that to `None`, and the cached value
//!    is `None` for the rest of this process.
//!
//! No new dependencies; this crate already carries `tempfile` as a
//! dev-dependency for the sibling ordering test in `routing.rs`.

use darkmux_crew::dispatch::{CompactionDispatchArgs, DispatchOpts};
use std::time::Duration;

/// Same shape as `routing.rs`'s private `local_opts` test helper — that one
/// isn't reachable from an external integration-test binary, so it's
/// duplicated here rather than widened for one caller.
fn opts_for(role_id: &str) -> DispatchOpts {
    DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: role_id.to_string(),
        message: "hi".to_string(),
        session_id: None,
        timeout_seconds: 60,
        skip_preflight: false,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: CompactionDispatchArgs::default(),
        profile_name: None,
        config_path: None,
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: None,
    }
}

/// Bare TCP listener recording every accepted connection — the fleet-queue
/// stand-in. Copied in shape from `routing.rs`'s
/// `spawn_connection_counting_peer` (that one is `#[cfg(test)]`-private to
/// the unit-test binary, unreachable from here).
fn spawn_connection_counting_peer() -> (u16, std::sync::mpsc::Receiver<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(_stream) = stream else { continue };
            let _ = tx.send(());
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    (port, rx)
}

#[test]
fn dispatch_routed_via_refuses_resume_from_on_the_local_unknown_arm_before_the_queue_is_touched()
{
    // ── force `resolve_machine_id()` to cache `None`, before anything else
    //    in this process can call it ──────────────────────────────────
    let prev_machine = std::env::var("DARKMUX_MACHINE_ID").ok();
    let prev_path = std::env::var("PATH").ok();
    unsafe {
        std::env::remove_var("DARKMUX_MACHINE_ID");
        // A single empty PATH entry means "search only the current working
        // directory" (POSIX execvp) — reliably different from an UNSET
        // PATH, which some libc fall back to a default search list
        // (confstr(_CS_PATH)) for. `hostname` is not in this test binary's
        // cwd, so the shell-out fails to spawn.
        std::env::set_var("PATH", "");
    }

    let (port, rx) = spawn_connection_counting_peer();
    let flows_dir = tempfile::TempDir::new().unwrap();
    let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
    let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
    unsafe {
        std::env::set_var("DARKMUX_REDIS_URL", format!("redis://127.0.0.1:{port}"));
        std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path());
    }

    // Sanity: prove the arm this test claims to reach is the one actually
    // reached, before trusting the refusal below. If this ever fails, the
    // env forcing above stopped working (e.g. a libc PATH fallback) and
    // every assertion after it would otherwise pass VACUOUSLY against the
    // `local_unknown: false` arm the sibling unit test already covers.
    let local = darkmux_flow::resolve_machine_id();
    assert!(
        local.is_none(),
        "resolve_machine_id() must resolve to None for this test to reach the \
         `local_unknown: true` arm — got {local:?}. Either DARKMUX_MACHINE_ID leaked in \
         from the ambient environment, or this platform's `hostname` shell-out succeeded \
         despite the emptied PATH."
    );

    let mut opts = opts_for("pr-reviewer");
    opts.machine = Some("peer-b".to_string());
    opts.resume_from = Some(std::path::PathBuf::from("/tmp/darkmux-2584-checkpoint"));

    let err = darkmux_fleet::dispatch_routed_via(opts, |_opts| {
        panic!(
            "local_dispatch must never be invoked for a --machine=peer-b dispatch on the \
             local_unknown arm either"
        );
    })
    .expect_err("--resume-from with --machine=<peer> must refuse on this arm too, not route \
                 to the queue");
    let msg = format!("{err:#}");

    unsafe {
        match prev_machine {
            Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
            None => std::env::remove_var("DARKMUX_MACHINE_ID"),
        }
        match prev_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        match prev_redis {
            Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
            None => std::env::remove_var("DARKMUX_REDIS_URL"),
        }
        match prev_flows {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }

    assert!(
        msg.contains("--machine=peer-b"),
        "must name the pinned target machine as the reason: {msg}"
    );
    assert!(
        msg.contains(
            "darkmux never silently starts a dispatch fresh under a name that looked \
             like a resume"
        ),
        "must carry the same promise the other two guards state: {msg}"
    );

    match rx.recv_timeout(Duration::from_millis(300)) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        Ok(()) => panic!(
            "dispatch_via_queue must never run for a refused resume, but the mock \
             fleet-queue peer accepted a connection"
        ),
        Err(e) => panic!("unexpected mock channel state: {e:?}"),
    }

    let files: Vec<_> = std::fs::read_dir(flows_dir.path())
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        files.is_empty(),
        "no flow record may be written before the resume-from refusal fires; \
         found in {}: {files:?}",
        flows_dir.path().display()
    );
}
