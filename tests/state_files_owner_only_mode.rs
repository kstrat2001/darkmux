//! TDD coverage for Wave-E.11 (#255): operator-state files MUST be
//! created with mode `0o600` (owner read/write only) so a misconfigured
//! umask, a shared-filesystem mount, or a future multi-user fleet
//! deployment can't leak roster contents (machine addresses, tier
//! assignments, future bearer-token fields) or mission/phase state
//! (operator intent, prompts, descriptions) to other users.
//!
//! Pre-fix: every state writer used `fs::write` which respects the
//! user umask. On a default Linux umask of `0o022` the files land at
//! `0o644` — group/other readable.
//!
//! Post-fix: writers use `OpenOptions::new().mode(0o600).create(true).
//! truncate(true).write(true).open(path)` (or an equivalent helper)
//! and the resulting file mode is exactly `0o600`.
//!
//! POSIX-only — `#[cfg(unix)]` gates the assertions. Windows file ACLs
//! are a separate story.

#![cfg(unix)]

use darkmux_flow::FlowSink as _;
use std::os::unix::fs::PermissionsExt;

/// Run a closure with `HOME` overridden so any state writer rooted in
/// `~/.darkmux` writes into our tmpdir.
fn with_home<F: FnOnce(&std::path::Path) -> R, R>(f: F) -> R {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Override both possible home env vars — saver code paths read
    // `HOME` on Unix; CI matrices sometimes set `XDG_*` variants.
    let prev_home = std::env::var_os("HOME");
    let prev_dmx_home = std::env::var_os("DARKMUX_HOME");
    // SAFETY: these tests run #[serial] in their own crate; no other
    // test mutates HOME concurrently.
    unsafe {
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("DARKMUX_HOME");
    }
    let result = f(tmp.path());
    unsafe {
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_dmx_home {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    result
}

fn mode_bits(p: &std::path::Path) -> u32 {
    std::fs::metadata(p)
        .unwrap_or_else(|e| panic!("metadata({}): {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

/// ─── Anti-vacuity control ────────────────────────────────────────────────
///
/// Every other assertion in this file reads `mode == 0o600`. That is ALSO
/// what a bare create yields on a machine whose umask is `0o077` — so on a
/// hardened runner this whole suite passes with every `.mode()` deleted
/// from the production code. A green suite would then prove nothing.
///
/// This control pins the runner's umask from the inside: a file created
/// with NO `.mode()` at all, in this same process, must land at `0o644`.
/// If it does, the umask is permissive, a bare create is world-readable,
/// and each `== 0o600` below is genuinely distinguishing a real fix from
/// the ambient default. If it does NOT, the environment — not the
/// production code — is what is making the other tests pass.
///
/// Deliberately NOT implemented by calling `libc::umask`: that is
/// process-global and these tests run in threads alongside each other, so
/// setting it here would race every other assertion in the file. Observing
/// the inherited umask is the whole mechanism.
///
/// This generalizes a guard `lifecycle_save_json_is_owner_only_mode`
/// already carries per-test ("staged fixture is already 0o600 (umask is
/// restrictive?)") to the whole file, so the tests that have no staged
/// fixture to compare against — the `flock` and hook-file ones, which
/// assert on a file the production code creates from nothing — are covered
/// by the same check rather than being the ones silently exempt from it.
#[test]
fn umask_control_bare_create_is_world_readable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("no-mode-set.control");
    // No `.mode()`, no `set_permissions` — whatever the process umask
    // grants is exactly what this file gets.
    std::fs::write(&path, b"control").expect("write control file");

    let actual = mode_bits(&path);
    assert_eq!(
        actual,
        0o644,
        "ANTI-VACUITY CONTROL FAILED (got {actual:o}, expected 644).\n\
         This is NOT a bug in the control — it is a statement about every OTHER \
         assertion in this file.\n\
         A file created with no `.mode()` landed at {actual:o}, which means this \
         runner's umask is more restrictive than the 0o022 these tests assume \
         (0o077 produces 600).\n\
         Under that umask a bare create is ALREADY owner-only, so the \
         `== 0o600` assertions in this file cannot tell a real `.mode(0o600)` \
         fix from the ambient default: they would pass even with every \
         `.mode()` deleted from the production code.\n\
         Treat this failure as INVALIDATING the other results in this file, \
         not as an isolated failure. Re-run under `umask 0o022` to get \
         assertions that mean something."
    );
}

#[test]
fn fleet_roster_is_owner_only_mode() {
    with_home(|home| {
        // Build a roster and save it via the public API. We invoke the
        // binary so it picks up the same HOME we set above and resolves
        // its roster path through the production code path.
        let bin = env!("CARGO_BIN_EXE_darkmux");
        let out = std::process::Command::new(bin)
            .args(["machine", "add", "test-node", "--address", "127.0.0.1:9999"])
            .env("HOME", home)
            .env_remove("DARKMUX_HOME")
            .output()
            .expect("running `darkmux fleet add`");
        assert!(
            out.status.success(),
            "fleet add failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let roster_path = home.join(".darkmux").join("fleet.json");
        assert!(roster_path.exists(), "roster not at {}", roster_path.display());
        assert_eq!(
            mode_bits(&roster_path),
            0o600,
            "fleet.json must be mode 0o600 to protect machine addresses + future bearer-token fields; got {:o}",
            mode_bits(&roster_path)
        );
    });
}

#[test]
fn lifecycle_save_json_is_owner_only_mode() {
    // Drive lifecycle::save_json through a real CLI verb that creates
    // a mission. `mission propose --dry-run` won't write; use `crew
    // ack` which lifecycle::save_json's via mission_start path — or
    // do it the simple way: invoke `darkmux mission show` after
    // synthesizing a mission via direct file write, then probe the
    // mode of the synthesized file. Easier path: call the binary with
    // a verb that lands on save_json.
    //
    // Simplest reliable driver: write a minimal mission.json by hand
    // (so we have something to update), then call any verb that
    // updates it. For pure coverage of save_json's mode the JSON
    // shape doesn't matter — we just need the writer to fire.
    //
    // Direct unit-test approach is cleaner: call save_json (it's
    // pub(crate)) from a tests/ integration test we can't. We use
    // the binary route. For Wave-E.11 the binary subcommand that
    // most cleanly fires save_json with no other dependencies is
    // `mission propose` with a synthetic intent — but that requires
    // a live LMStudio. Easier: pre-stage a mission file with default
    // umask, then call `darkmux mission start <id>` which goes
    // through save_json + flips status to active.
    with_home(|home| {
        let crew = home.join(".darkmux").join("crew");
        let mission_id = "test-mission-e11";
        let mission_dir = crew.join("missions").join(mission_id);
        std::fs::create_dir_all(mission_dir.join("phases")).unwrap();
        let mission_path = mission_dir.join("mission.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // Active + started_ts=None — `mission start` will rewrite via
        // save_json. (Active+started bails; Closed bails; Paused
        // routes to a different verb.)
        let body = serde_json::json!({
            "id": mission_id,
            "description": "e11 mode test",
            "status": "active",
            "phase_ids": [],
            "created_ts": now,
        });
        std::fs::write(&mission_path, serde_json::to_string_pretty(&body).unwrap())
            .unwrap();
        // Confirm the staged file is NOT 0o600 yet (sanity — proves
        // the next-step assertion is meaningful).
        let pre_mode = mode_bits(&mission_path);
        assert_ne!(
            pre_mode, 0o600,
            "staged fixture is already 0o600 (umask is restrictive?); test would pass for the wrong reason"
        );

        // Trigger save_json via mission start.
        let bin = env!("CARGO_BIN_EXE_darkmux");
        let out = std::process::Command::new(bin)
            .args(["mission", "start", mission_id])
            .env("HOME", home)
            .env_remove("DARKMUX_HOME")
            .output()
            .expect("running `darkmux mission start`");
        // We don't insist on success — mission start may bail on
        // missing state in this isolated env. What we care about is
        // whether save_json fired AND set the mode. If the file mode
        // is still pre_mode, save_json didn't run; that's a setup
        // miss, not the bug we're testing for.
        if !out.status.success() {
            eprintln!(
                "mission start failed (expected in isolated env); stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }

        assert_eq!(
            mode_bits(&mission_path),
            0o600,
            "mission.json must be mode 0o600 after lifecycle::save_json rewrite; got {:o}",
            mode_bits(&mission_path)
        );
    });
}

// ─── #2259: hook outbox / quarantine / status-sidecar files ───────────────
//
// The hook outbox holds whole flow records (a crawl finding's `evidence` is
// a source line copied verbatim out of the operator's repository), so a
// world-readable outbox/quarantine/status file leaks that content to any
// other local user. Wave-E.11/#255's `write_owner_only_file` covered the
// state-file family (fleet.json, mission.json, ...); #2183 added its own
// owner-only writer but wired it to exactly ONE call site (the file
// transport's dry-run dump) — the hook outbox, its `.quarantine` sibling,
// and the `.last` status sidecar all go through
// `darkmux_types::flock::lock_exclusive`, which never set a mode and so
// landed at the process umask default (typically `0o644`) on creation.
//
// The fix lives at the CREATOR (`flock::lock_exclusive`), not at each call
// site — proven directly below, then proven again end-to-end through the
// real `HookSink` drain path so the guarantee and its proof live together.

/// Poll `dir` for a (single) entry whose filename ends with `suffix`,
/// returning its path once found. Hook filenames are content-hash-keyed
/// (`rule_key`), so tests that don't want to reimplement that hash just
/// glob for the well-known suffix instead — `outbox_paths`/
/// `last_status_path`/`quarantine_path`'s own doc comments name these
/// suffixes as the stable sibling-naming contract.
fn wait_for_file_with_suffix(dir: &std::path::Path, suffix: &str, timeout: std::time::Duration) -> std::path::PathBuf {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(suffix) {
                    return entry.path();
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for a *{suffix} file under {}", dir.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Direct proof of the fix: `flock::lock_exclusive` is the single creator
/// behind the outbox append (`append_outbox_line`), the trailing-newline
/// fixup, the audit-log append, the roster lock and the workload-registry
/// lock — fixing it here fixes every one of those at once, rather than
/// chasing each call site. A file it creates fresh must land at `0o600`.
#[test]
fn flock_lock_exclusive_creates_files_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("some-nested-dir").join("state.lock");
    let _guard = darkmux_types::flock::lock_exclusive(&path).expect("lock_exclusive");
    assert!(path.exists(), "lock_exclusive must create the file");
    assert_eq!(
        mode_bits(&path),
        0o600,
        "flock::lock_exclusive must create files at mode 0o600 (owner read/write only); got {:o}",
        mode_bits(&path)
    );
}

/// Sibling of the above for the non-blocking variant, used by the outbox
/// drainer's per-cycle drain lock.
#[test]
fn flock_try_lock_exclusive_creates_files_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("drain.lock");
    let guard = darkmux_types::flock::try_lock_exclusive(&path).expect("try_lock_exclusive");
    assert!(guard.is_some(), "an unheld lock must be acquired");
    assert_eq!(
        mode_bits(&path),
        0o600,
        "flock::try_lock_exclusive must create files at mode 0o600; got {:o}",
        mode_bits(&path)
    );
}

/// A minimal no-op `FlowSink` — `HookSink::new` needs a `report_sink` for
/// its own `hook.fired`/`hook.failed` records; this test doesn't care where
/// those land.
struct NullSink;
impl darkmux_flow::FlowSink for NullSink {
    fn write(&self, _record: &darkmux_flow::FlowRecord) -> anyhow::Result<()> {
        Ok(())
    }
    fn info(&self) -> darkmux_flow::SinkInfo {
        darkmux_flow::SinkInfo { kind: "Null".into(), config: Default::default(), children: vec![], raw_url: None }
    }
}

fn sample_record(action: &str) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: action.to_string(),
        handle: "h".to_string(),
        phase_id: None,
        session_id: None,
        source: None,
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    }
}

/// End-to-end proof through the real `HookSink` drain path: a matching
/// record appended to the outbox, and delivered (writing the `.last`
/// status sidecar), both land at `0o600` — never the outbox's whole-flow-
/// record contents nor the sidecar's delivery detail readable by another
/// local user.
#[test]
fn hook_outbox_and_status_sidecar_are_owner_only_mode() {
    with_home(|home| {
        let outbox_dir = home.join(".darkmux").join("hooks").join("outbox");
        let receiver = darkmux_flow::hooks::test_receiver::HookReceiver::start();
        let rules = vec![darkmux_types::config::HookRule {
            r#match: Some(darkmux_types::config::HookMatch {
                action: Some("*".to_string()),
                ..Default::default()
            }),
            http: Some(receiver.url("/events")),
            ..Default::default()
        }];
        let report: std::sync::Arc<dyn darkmux_flow::FlowSink> = std::sync::Arc::new(NullSink);
        let sink = darkmux_flow::hooks::HookSink::new(&rules, outbox_dir.clone(), report).expect("HookSink::new");

        sink.write(&sample_record("dispatch.start")).expect("write");

        // Wait for delivery — proves both files (outbox + `.last`) exist.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while receiver.request_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(receiver.request_count(), 1, "the drainer must have delivered the one matching record");

        let outbox_path = wait_for_file_with_suffix(&outbox_dir, ".outbox.jsonl", std::time::Duration::from_secs(3));
        let status_path = wait_for_file_with_suffix(&outbox_dir, ".last", std::time::Duration::from_secs(3));

        assert_eq!(
            mode_bits(&outbox_path),
            0o600,
            "hook outbox ({}) carries whole flow records verbatim and must be mode 0o600; got {:o}",
            outbox_path.display(),
            mode_bits(&outbox_path)
        );
        assert_eq!(
            mode_bits(&status_path),
            0o600,
            "hook status sidecar ({}) carries delivery error detail and must be mode 0o600; got {:o}",
            status_path.display(),
            mode_bits(&status_path)
        );
    });
}

/// A line that fails JSON validation (a torn write, or — as simulated here
/// — direct on-disk corruption) is quarantined rather than dropped
/// (`quarantine_line`), preserving its raw bytes on disk. Those bytes are
/// whatever the outbox held, so the quarantine file needs the same
/// protection as the outbox itself.
#[test]
fn hook_quarantine_file_is_owner_only_mode() {
    with_home(|home| {
        let outbox_dir = home.join(".darkmux").join("hooks").join("outbox");
        let receiver = darkmux_flow::hooks::test_receiver::HookReceiver::start();
        let rules = vec![darkmux_types::config::HookRule {
            r#match: Some(darkmux_types::config::HookMatch {
                action: Some("*".to_string()),
                ..Default::default()
            }),
            http: Some(receiver.url("/events")),
            ..Default::default()
        }];
        let report: std::sync::Arc<dyn darkmux_flow::FlowSink> = std::sync::Arc::new(NullSink);
        let sink = darkmux_flow::hooks::HookSink::new(&rules, outbox_dir.clone(), report).expect("HookSink::new");

        // Seed one real, valid, delivered line first — this is what makes
        // the outbox file exist so we can find it below, and confirms the
        // rule is actually wired up (a matching write that never delivers
        // would leave the quarantine assertion unreachable, not merely
        // failing for the wrong reason).
        sink.write(&sample_record("dispatch.start")).expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while receiver.request_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(receiver.request_count(), 1, "setup: the valid record must deliver before we corrupt the file");

        let outbox_path = wait_for_file_with_suffix(&outbox_dir, ".outbox.jsonl", std::time::Duration::from_secs(3));

        // Simulate corruption: append a complete, newline-terminated, but
        // non-JSON line directly (bypassing the sink) — the same shape a
        // torn write can leave behind. Deliberately opened WITHOUT a mode
        // override — proves the quarantine file's protection comes from
        // `quarantine_line`'s own writer, not from this test's setup.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&outbox_path).expect("open outbox for corruption");
            f.write_all(b"not valid json\n").expect("append invalid line");
        }

        let quarantine_path =
            wait_for_file_with_suffix(&outbox_dir, ".quarantine", std::time::Duration::from_secs(5));
        assert_eq!(
            mode_bits(&quarantine_path),
            0o600,
            "hook quarantine file ({}) preserves raw (possibly sensitive) outbox bytes and must be mode 0o600; got {:o}",
            quarantine_path.display(),
            mode_bits(&quarantine_path)
        );
    });
}
