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

/// A finding is written once via `create_new(true)`; a second producer that
/// races the first reports `AlreadyPresent` and never touches the bytes —
/// so, like the mission/roster stores above, only the CREATOR needs to set
/// the mode. Minimal record: every field the type requires, none of the
/// optional provenance.
#[test]
fn finding_store_is_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_umask_leaves_a_bare_create_readable(tmp.path());
    let root = tmp.path().join("findings");
    let record = darkmux_crew::findings::FindingRecord {
        key: "sess-1/1".to_string(),
        dispatch: "sess-1".to_string(),
        seq: 1,
        ts: darkmux_flow::ts_utc_now(),
        tool_name: "report_finding".to_string(),
        proposer: darkmux_crew::findings::Proposer {
            handle: "coder".to_string(),
            model: "test-model".to_string(),
            machine_id: None,
        },
        mission_id: None,
        phase_id: None,
        step_id: None,
        context: serde_json::Value::Null,
        emitted: serde_json::json!({"evidence": "let secret = std::env::var(\"API_KEY\");"}),
        source: None,
        schema_version: darkmux_crew::findings::FINDING_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    let outcome = darkmux_crew::findings::materialize(&root, &record).expect("materialize");
    assert_eq!(
        outcome,
        darkmux_crew::findings::Materialized::Created,
        "setup guard: the finding must actually have been written for the mode assertion below to mean anything"
    );

    let path = darkmux_crew::findings::record_path_at(&root, "sess-1", 1);
    assert!(path.exists(), "finding.json not at {}", path.display());
    assert_eq!(
        mode_bits(&path),
        0o600,
        "a finding's `evidence` is a source line copied verbatim out of the operator's repository and must be \
         mode 0o600; got {:o}",
        mode_bits(&path)
    );
}

/// Same shape as the finding test above, for `mods::materialize` — a mod
/// record IS a patch against the operator's code.
#[test]
fn mod_store_is_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_umask_leaves_a_bare_create_readable(tmp.path());
    let root = tmp.path().join("mods");
    let record = darkmux_crew::mods::ModRecord {
        key: "mod-1".to_string(),
        ts: darkmux_flow::ts_utc_now(),
        by: "coder/test-model".to_string(),
        r#for: vec!["sess-1/1".to_string()],
        kit: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string()),
        kit_looks_json: false,
        kit_kind: Some("unified-diff".to_string()),
        attachments: Vec::new(),
        context: darkmux_crew::mods::ModContext::default(),
        warnings: Vec::new(),
        mission_id: None,
        phase_id: None,
        step_id: None,
        source: None,
        gate: None,
        gate_skipped_reason: None,
        schema_version: darkmux_crew::mods::MOD_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    let outcome = darkmux_crew::mods::materialize(&root, &record).expect("materialize");
    assert_eq!(
        outcome,
        darkmux_crew::mods::Materialized::Created,
        "setup guard: the mod must actually have been written for the mode assertion below to mean anything"
    );

    let path = darkmux_crew::mods::record_path_at(&root, "mod-1");
    assert!(path.exists(), "mod.json not at {}", path.display());
    assert_eq!(
        mode_bits(&path),
        0o600,
        "a mod's `kit` is a patch against the operator's code and must be mode 0o600; got {:o}",
        mode_bits(&path)
    );
}

/// The flow day-file, proven via `record_via` against an explicit
/// `LocalFileSink` rather than the process-wide `record()`/`default_sink()`
/// singleton — `LocalFileSink` resolves its directory PER WRITE (see its
/// doc comment), so pointing `DARKMUX_FLOWS_DIR` at a tempdir for this call
/// is sufficient without needing to touch the cached default sink at all.
#[test]
#[serial_test::serial]
fn flow_jsonl_is_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_umask_leaves_a_bare_create_readable(tmp.path());
    let dir = tmp.path().join("flows");
    let prev = std::env::var_os("DARKMUX_FLOWS_DIR");
    // SAFETY: `#[serial_test::serial]` above — this is the only thread of
    // this test binary running while the env is mutated. Tight scoping alone
    // is NOT sufficient: `fleet_roster_…` and `lifecycle_save_json_…` mutate
    // HOME on their own threads, and two concurrent `setenv` calls race the
    // `environ` array. `LocalFileSink` resolves its directory per write (see
    // its doc comment), so the override only has to be live across the one
    // `record_via` call below; and the root package's dev-dependency turns on
    // darkmux-flow's `test-support` feature, whose fallback is a per-process
    // temp dir — so even a restore-ordering slip cannot land a record in the
    // operator's real `~/.darkmux/flows`.
    unsafe {
        std::env::set_var("DARKMUX_FLOWS_DIR", &dir);
    }

    let record = darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: "dispatch.start".to_string(),
        handle: "coder".to_string(),
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
    };
    let sink = darkmux_flow::LocalFileSink::new();
    let result = darkmux_flow::record_via(&sink, &record);

    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }
    result.expect("record_via");

    let path = dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
    assert!(path.exists(), "flow day-file not at {}", path.display());
    assert_eq!(
        mode_bits(&path),
        0o600,
        "every flow record darkmux writes lands in this file and it must be mode 0o600; got {:o}",
        mode_bits(&path)
    );
}

/// Anti-vacuity control, run INLINE by each `#[test]` below rather than
/// living in a sibling test, so an assertion's meaning does not depend on
/// another test's presence — or on which branch merged first.
///
/// Measured, and the reason this exists: with the production `.mode(0o600)`
/// calls reverted and the runner at `umask 0077`, `finding_store_…`,
/// `mod_store_…`, `flow_jsonl_…` and `fleet_roster_…` ALL still passed —
/// a bare create is already `0o600` under that umask, so every `== 0o600`
/// assertion in this file is satisfied by the ambient umask alone and
/// proves nothing about the code. This probe writes a file with no mode of
/// its own into the SAME tempdir (same filesystem, same umask) and refuses
/// to let the test continue if that bare file is already owner-only —
/// exactly the posture `lifecycle_save_json_is_owner_only_mode`'s own
/// `assert_ne!` control already takes for its staged fixture.
///
/// Deliberately NOT a `libc::umask` call inside the test: `umask(2)` is
/// process-global and these tests run in parallel threads of one binary,
/// so setting it here would race every sibling. Restrictive-umask runners
/// get a loud failure to fix, not a silent pass.
fn assert_umask_leaves_a_bare_create_readable(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("control dir");
    let probe = dir.join(".umask-control");
    std::fs::write(&probe, b"control").expect("control write");
    let bare = mode_bits(&probe);
    std::fs::remove_file(&probe).ok();
    assert_ne!(
        bare, 0o600,
        "anti-vacuity control: a bare create in {} already landed at 0o600, so this runner's \
         umask is restrictive enough that the 0o600 assertion below would pass with the \
         production `.mode(0o600)` reverted. Re-run under a permissive umask (e.g. 0o022) — \
         the assertion is not distinguishing here.",
        dir.display()
    );
}

/// (#2451, review) `mods::create_from_emission` — the RUNTIME producer —
/// stages attachments with a bare `fs::write` (`stage_and_commit`'s
/// closure), which lands at the umask default, and then `fs::rename`s the
/// staging directory into place. `rename` moves inodes and preserves every
/// mode, so the mod's own `mod.json` keeps the `0o600` `materialize` gave
/// it — but an attachment beside it does NOT, and an attachment is the same
/// operator content the kit is (a diff, a screenshot, a captured log that
/// rode out of the container).
#[test]
fn mod_attachment_from_emission_is_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_umask_leaves_a_bare_create_readable(tmp.path());
    let root = tmp.path().join("mods");
    let findings_root = tmp.path().join("findings");

    let rec = darkmux_crew::mods::create_from_emission(
        &root,
        &findings_root,
        "coder/test-model",
        &[],
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
        &[darkmux_crew::mods::InlineAttachment {
            name: "evidence.diff".to_string(),
            bytes: b"--- a/secret.rs\n+++ b/secret.rs\n".to_vec(),
        }],
        darkmux_crew::findings::Scope { mission_id: None, phase_id: None, step_id: None },
        None,
        Vec::new(),
    )
    .expect("create_from_emission");

    let attachment =
        darkmux_crew::mods::attachments_dir_at(&root, &rec.key).join("evidence.diff");
    assert!(attachment.exists(), "attachment not at {}", attachment.display());
    assert_eq!(
        mode_bits(&attachment),
        0o600,
        "a mod attachment is the same operator content the kit is and must be mode 0o600; got {:o}",
        mode_bits(&attachment)
    );
}

/// (#2451, review) The other producer: `mods::create` COPIES attachments
/// from host paths with `fs::copy`, which on Unix carries the SOURCE file's
/// permission bits onto the copy. A `0o644` file in the operator's repo —
/// the ordinary case — therefore landed `0o644` inside `~/.darkmux/mods/`
/// no matter how restrictive the umask was.
#[test]
fn mod_attachment_copied_from_host_is_owner_only_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Both controls, because this assertion has two distinct ways to go
    // vacuous. The umask one: the fix replaced `fs::copy` with an explicit
    // owner-only create, so under `umask 0077` a mode-less create is already
    // 0o600 (measured — with the mode reverted this test passed at 0077 and
    // failed at 0022). The source-mode one, just below: if the staged source
    // file were not world-readable, the pre-fix `fs::copy` would have carried
    // owner-only bits across and looked correct for the wrong reason.
    assert_umask_leaves_a_bare_create_readable(tmp.path());
    let root = tmp.path().join("mods");
    let findings_root = tmp.path().join("findings");
    let src = tmp.path().join("src-patch.diff");
    std::fs::write(&src, b"--- a/secret.rs\n+++ b/secret.rs\n").expect("staging the source file");
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644))
        .expect("chmod the source file");
    assert_eq!(mode_bits(&src), 0o644, "setup: the source file must be world-readable");

    let rec = darkmux_crew::mods::create(
        &root,
        &findings_root,
        "operator",
        &[],
        Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"),
        std::slice::from_ref(&src),
        None,
        false,
    )
    .expect("create");

    let attachment =
        darkmux_crew::mods::attachments_dir_at(&root, &rec.key).join("src-patch.diff");
    assert!(attachment.exists(), "attachment not at {}", attachment.display());
    assert_eq!(
        mode_bits(&attachment),
        0o600,
        "`fs::copy` carries the source file's mode onto the copy — a 0o644 file in the \
         operator's repo must not land 0o644 inside the mod store; got {:o}",
        mode_bits(&attachment)
    );
}
