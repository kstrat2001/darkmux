//! TDD coverage for Wave-E.12 (#255): concurrent `darkmux fleet add`
//! invocations MUST NOT lose entries to a last-writer-wins race.
//!
//! Pre-fix: `cmd_fleet_add` does `load_roster()` → mutate → `save_roster()`
//! with no cross-process lock. Two invocations interleaved as:
//!
//!     A: load (sees R0)         B: load (sees R0)
//!     A: add machine-A          B: add machine-B
//!     A: save (R0+A)            B: save (R0+B)    ← machine-A lost
//!
//! Post-fix: a `mutate_roster<F>` helper wraps the read-modify-write
//! cycle in `flock(LOCK_EX)` on the roster path, mirroring the
//! `AuditFileSink` flock pattern (`src/flow.rs:479`). Concurrent
//! invocations serialize through the lock; no entry is lost.
//!
//! POSIX-only — `#[cfg(unix)]` gates the assertion. Windows doesn't
//! get cross-process flock without a polyfill (separate work).

#![cfg(unix)]

const N_CONCURRENT: usize = 12;

fn redis_available() -> bool {
    true // not needed for this test — no Redis dependency
}

#[test]
fn parallel_fleet_adds_keep_every_entry() {
    if !redis_available() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_darkmux");
    let home = tmp.path().to_path_buf();
    let roster_path = home.join(".darkmux").join("fleet.json");

    // Spawn N concurrent `darkmux fleet add` invocations, each
    // adding a distinct machine id. Without flock, at least one
    // entry will typically be lost to the load-then-save race.
    let mut handles = Vec::with_capacity(N_CONCURRENT);
    for i in 0..N_CONCURRENT {
        let bin = bin.to_string();
        let home = home.clone();
        let id = format!("node-{i:02}");
        let handle = std::thread::spawn(move || {
            std::process::Command::new(&bin)
                .args([
                    "fleet", "add", &id,
                    "--address", &format!("127.0.0.1:{}", 10000 + i),
                ])
                .env("HOME", &home)
                .env_remove("DARKMUX_HOME")
                .output()
                .expect("running `darkmux fleet add`")
        });
        handles.push(handle);
    }

    // Reap all invocations + confirm none failed.
    for h in handles {
        let out = h.join().expect("join fleet add thread");
        assert!(
            out.status.success(),
            "fleet add failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Critical invariant: every machine id is present in the
    // resulting roster. Pre-fix, this typically loses ≥ 1 entry.
    let body = std::fs::read_to_string(&roster_path)
        .unwrap_or_else(|e| panic!("read roster {}: {e}", roster_path.display()));
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .expect("roster JSON parses");
    let machines = parsed.get("machines")
        .and_then(|v| v.as_object())
        .expect("roster has `machines` object");

    let missing: Vec<String> = (0..N_CONCURRENT)
        .map(|i| format!("node-{i:02}"))
        .filter(|id| !machines.contains_key(id))
        .collect();
    assert!(
        missing.is_empty(),
        "concurrent fleet-add lost entries: {missing:?}; \
         roster contains {} of {N_CONCURRENT} expected entries",
        machines.len()
    );
}
