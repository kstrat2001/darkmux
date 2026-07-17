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

#[test]
fn parallel_lab_registers_keep_every_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_darkmux");
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
        let bin = bin.to_string();
        let home = home.clone();
        let fixture = fixture_dir.to_string_lossy().to_string();
        let name = format!("fix-{i:02}");
        handles.push(std::thread::spawn(move || {
            std::process::Command::new(&bin)
                .args(["lab", "fixture", "register", &fixture, "--name", &name])
                .current_dir(&home)
                .env("HOME", &home)
                .env_remove("DARKMUX_HOME")
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
