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

#[test]
fn fleet_roster_is_owner_only_mode() {
    with_home(|home| {
        // Build a roster and save it via the public API. We invoke the
        // binary so it picks up the same HOME we set above and resolves
        // its roster path through the production code path.
        let bin = env!("CARGO_BIN_EXE_darkmux");
        let out = std::process::Command::new(bin)
            .args(["fleet", "add", "test-node", "--address", "127.0.0.1:9999"])
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
        // openclaw config absence in this isolated env. What we care
        // about is whether save_json fired AND set the mode. If the
        // file mode is still pre_mode, save_json didn't run; that's
        // a setup miss, not the bug we're testing for.
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
