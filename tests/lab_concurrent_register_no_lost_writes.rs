//! TDD coverage for #496: concurrent `darkmux lab register` invocations
//! MUST NOT lose entries to a last-writer-wins race.
//!
//! Pre-fix: `cmd_register` did `LabRegistry::load` → mutate → `save`
//! with no cross-process lock. Two invocations interleaved as:
//!
//!     A: load (sees R0)         B: load (sees R0)
//!     A: register fix-A         B: register fix-B
//!     A: save (R0+A)            B: save (R0+B)    ← fix-A lost
//!
//! Post-fix: `LabRegistry::with_locked` wraps the read-modify-write
//! cycle in `flock(LOCK_EX)` on a sidecar `<registry>.lock`, mirroring
//! the `AuditFileSink` flock pattern. Concurrent invocations serialize
//! through the lock; no entry is lost.
//!
//! POSIX-only — `#[cfg(unix)]` gates the cross-process flock guarantee.

#![cfg(unix)]

const N_CONCURRENT: usize = 12;

// ===== DARKMUX-SPAWN-HELPERS: BEGIN (#2710) ==========================
//
// The only place this file names the darkmux binary.
//
// Before #2710 the spawn below did `.env("HOME", …)
// .env_remove("DARKMUX_HOME")` and neutralized nothing else. Measured at
// that head with the dir set exported to sentinels and a fake `$HOME`
// this one was EXIT=0 with zero files leaked — but that is a fact about
// which destinations `lab fixture register` happens to touch today, not
// about the isolation, which was the same unneutralized shape as the two
// targets in this sweep that DID write into a sentinel (one of them into
// a hash chain). Converted for the shape, not for a measured leak.
//
// `DARKMUX_HOME` stays REMOVED, deliberately: the assertion is on
// `<tempdir>/.darkmux/lab-registry.json`, the DEFAULT user-scope
// resolution off `$HOME` with a cwd that has no `.darkmux` ancestor.
// `neutralize_state_vars` removes it along with the rest — including
// `DARKMUX_LAB_DIR`, which this test never named.
use darkmux_types::test_isolation::neutralize_state_vars;

/// A `std::process::Command` for the darkmux binary, scoped to `home`
/// (as both `$HOME` and cwd) and neutralized. Neutralize FIRST, pin
/// SECOND — `Command` applies `.env` and `.env_remove` in call order.
fn darkmux_cmd(home: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_darkmux"));
    neutralize_state_vars(&mut cmd);
    cmd.current_dir(home).env("HOME", home);
    cmd
}

// ===== DARKMUX-SPAWN-HELPERS: END (#2710) ============================

#[test]
fn parallel_lab_registers_keep_every_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().to_path_buf();

    // A single fixture dir registered under N distinct names. The
    // `.fixture.json` `name` is overridden per-invocation via `--name`.
    let fixture_dir = home.join("fixture");
    std::fs::create_dir_all(&fixture_dir).expect("mkdir fixture");
    std::fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "shared"}"#)
        .expect("write .fixture.json");
    std::fs::write(fixture_dir.join("placeholder.txt"), "x").expect("write placeholder");

    // User-scope registry lands under HOME/.darkmux. Running from the
    // tempdir (no `.darkmux` ancestor) keeps scope resolution on user.
    let registry_path = home.join(".darkmux").join("lab-registry.json");

    let mut handles = Vec::with_capacity(N_CONCURRENT);
    for i in 0..N_CONCURRENT {
        let home = home.clone();
        let fixture = fixture_dir.to_string_lossy().to_string();
        let name = format!("fix-{i:02}");
        handles.push(std::thread::spawn(move || {
            darkmux_cmd(&home)
                .args(["lab", "fixture", "register", &fixture, "--name", &name])
                .output()
                .expect("running `darkmux lab fixture register`")
        }));
    }

    for h in handles {
        let out = h.join().expect("join lab fixture register thread");
        assert!(
            out.status.success(),
            "lab fixture register failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Critical invariant: every name is present. Pre-fix this typically
    // loses ≥ 1 entry to the load-then-save race.
    let body = std::fs::read_to_string(&registry_path)
        .unwrap_or_else(|e| panic!("read registry {}: {e}", registry_path.display()));
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("registry JSON parses");
    let fixtures = parsed
        .get("fixtures")
        .and_then(|v| v.as_object())
        .expect("registry has `fixtures` object");

    let missing: Vec<String> = (0..N_CONCURRENT)
        .map(|i| format!("fix-{i:02}"))
        .filter(|name| !fixtures.contains_key(name))
        .collect();
    assert!(
        missing.is_empty(),
        "concurrent lab-register lost entries: {missing:?}; \
         registry contains {} of {N_CONCURRENT} expected entries",
        fixtures.len()
    );
}
