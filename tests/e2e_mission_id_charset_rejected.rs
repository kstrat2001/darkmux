//! E2E TDD scenario for Wave-E.5 (#255): `darkmux mission dispatch`
//! MUST reject operator-supplied mission_ids containing path-traversal
//! or other non-identifier-shaped characters at the CLI boundary.
//!
//! Pre-fix: `mission_id` flows verbatim into the `session_id` format
//! string and into the WorkJob payload. PR-D.1 security-auditor MEDIUM
//! flagged: `mission_id="../etc"` reaches the mission-lookup code path,
//! which does exact-match against load_missions() (no traversal in
//! itself), but the value lands in audit-chain JSON unrendered + in
//! future "look up by mission" filters as a substring match.
//!
//! Post-fix (this PR): cmd_mission_dispatch validates mission_id via
//! `fleet::validate_identifier("mission_id", mission_id)` at the top
//! of the handler. Bails with operator-actionable error before reaching
//! load_missions / Redis / etc.

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};

fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // (#1662) Where the suite is REQUIRED, a missing redis is a HARD
    // FAILURE, never a skip.
    //
    // The skip exists so a contributor without redis installed isn't
    // blocked locally. But CI is this project's merge gate (local runs are
    // targeted-module by doctrine), and for the entire life of this harness
    // no workflow installed redis-server — so every e2e test on every run
    // returned early and reported `ok`. A real boot pays a release build
    // plus two daemon spawns; `dual_node_harness_boots_cleanly` was
    // finishing in 8 milliseconds. The fleet layer was guarded by nothing,
    // loudly reporting that it was guarded.
    //
    // A dependency that silently converts "did not run" into "passed" is
    // the same defect class as a status inferred rather than recorded: the
    // absence of evidence rendered as evidence of absence.
    //
    // Keyed on DARKMUX_E2E_REQUIRED, deliberately NOT on `CI`. GitHub sets
    // `CI` on EVERY runner, and the macOS workspace job runs these same
    // binaries via `cargo test --workspace` without installing redis — so a
    // `CI` gate would have failed the job that is correctly not responsible
    // for this suite. The env var names the actual requirement ("this job
    // opted in to running the fleet e2e") instead of a proxy for it, and
    // only `fleet-e2e` sets it.
    if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
        panic!(
            "redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set — the job that \
             opted in must never silently skip the fleet e2e suite (#1662). Install it \
             (`apt-get install -y redis-server`) or fix the runner image — do NOT relax \
             this back into a skip."
        );
    }
    ok
}

#[test]
fn mission_dispatch_rejects_path_traversal_mission_id() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    // (#2727) Shares this binary's one redis-server: rejection happens
    // in CLI-arg validation, before any redis or dispatch-queue
    // interaction, so there is nothing here for a shared server to leak
    // between the two tests in this file.
    let harness = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a")],
        "mission_dispatch_rejects_path_traversal_mission_id",
    )
    .expect("FleetHarness::boot_sharing_redis");
    let node = harness.node("node-a").unwrap();

    let out = node
        .cmd()
        .args([
            "mission", "dispatch",
            "../etc", // path-traversal mission_id
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("running darkmux mission dispatch");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "dispatch with path-traversal mission_id should fail; stdout={stdout}\nstderr={stderr}"
    );
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("invalid char") || combined.contains("mission_id"),
        "expected mission_id charset rejection; got:\n{combined}"
    );
}

#[test]
fn mission_dispatch_rejects_special_chars_in_mission_id() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    // (#2727) See the path-traversal test above for why this file shares
    // its redis.
    let harness = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a")],
        "mission_dispatch_rejects_special_chars_in_mission_id",
    )
    .expect("FleetHarness::boot_sharing_redis");
    let node = harness.node("node-a").unwrap();

    // Whitespace + uppercase + special chars all violate [a-z0-9_-].
    let out = node
        .cmd()
        .args([
            "mission", "dispatch",
            "Foo Bar$!",
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("running darkmux mission dispatch");

    assert!(
        !out.status.success(),
        "dispatch with special-char mission_id should fail"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("invalid char") || combined.contains("mission_id"),
        "expected mission_id charset rejection; got:\n{combined}"
    );
}

/// (#2727) Proves the isolation mechanism `boot_sharing_redis` depends on:
/// two harnesses sharing ONE physical `redis-server`, told apart only by
/// `DARKMUX_REDIS_STREAM`, cannot see each other's flow records.
///
/// Lives HERE — in a binary that already uses `boot_sharing_redis` for its
/// own production tests above — rather than in `harness.rs` itself. That
/// file is `#[path]`-included into all eight `e2e_*.rs` binaries, so a
/// test living there runs (and would spawn its own shared redis) in EVERY
/// one of them, including the six that never otherwise touch
/// `boot_sharing_redis` — turning a proof of the reduction into a source
/// of six brand new redis-server processes, one per binary, which is
/// exactly the regression this issue is about. Placed here, it reuses
/// this binary's ALREADY-shared instance and costs nothing extra.
///
/// **Mutation self-check (recorded in the PR that added this test):**
/// hardcode the SAME literal in place of both `"...-a"` / `"...-b"` stream
/// arguments below and this test goes RED — `stream_a` (queried by the
/// harness-A literal) then also contains harness B's marker, because both
/// harnesses were writing into the one stream that name now names.
/// Restore the distinct literals and it is green again. That is the
/// isolation property `boot_sharing_redis` callers are trusting: distinct
/// `stream` arguments, not distinct servers, are what keeps two
/// shared-redis tests apart.
#[test]
fn shared_redis_streams_do_not_cross_contaminate() {
    if !redis_available() {
        eprintln!("skipping shared_redis_streams_do_not_cross_contaminate: no redis-server");
        return;
    }

    let harness_a = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a")],
        "shared_redis_isolation_test_a",
    )
    .expect("boot_sharing_redis A");
    let harness_b = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a")],
        "shared_redis_isolation_test_b",
    )
    .expect("boot_sharing_redis B");

    // If this assertion ever fails, the rest of the test is meaningless:
    // two DEDICATED redis instances are isolated by having separate
    // keyspaces, which proves nothing about the STREAM-NAME mechanism
    // this test exists to check. The two harnesses must genuinely be
    // sharing one physical server — and, in THIS binary, must also be
    // sharing it with the two production tests above (verified
    // separately: this file's total redis-server count is 1 whether it
    // has two `boot_sharing_redis` production tests or three).
    assert_eq!(
        harness_a.redis_url(),
        harness_b.redis_url(),
        "both harnesses must land on the SAME physical redis-server for this test to \
         exercise the stream-isolation mechanism rather than trivially pass because they \
         happen to be on separate servers"
    );

    let node_a = harness_a.node("node-a").expect("node-a in harness A");
    let node_b = harness_b.node("node-a").expect("node-a in harness B");

    for (node, marker) in [
        (node_a, "marker-from-harness-a"),
        (node_b, "marker-from-harness-b"),
    ] {
        let out = node
            .cmd()
            .args(["flow", "note", "--text", marker, "--source", "isolation-test"])
            .output()
            .expect("running darkmux flow note");
        assert!(
            out.status.success(),
            "darkmux flow note failed: stdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    let client = redis::Client::open(harness_a.redis_url()).expect("redis client");
    let mut conn = client.get_connection().expect("redis connection");

    // Every entry's field/value pairs, flattened into one string, is
    // enough to check for the marker's presence without parsing the
    // XRANGE reply shape in full.
    let stream_contents = |conn: &mut redis::Connection, stream: &str| -> String {
        let entries: redis::Value = redis::cmd("XRANGE")
            .arg(stream)
            .arg("-")
            .arg("+")
            .query(conn)
            .unwrap_or_else(|e| panic!("XRANGE {stream}: {e}"));
        format!("{entries:?}")
    };

    let stream_a = stream_contents(&mut conn, "shared_redis_isolation_test_a");
    let stream_b = stream_contents(&mut conn, "shared_redis_isolation_test_b");

    assert!(
        stream_a.contains("marker-from-harness-a"),
        "harness A's own stream must contain A's record; got: {stream_a}"
    );
    assert!(
        !stream_a.contains("marker-from-harness-b"),
        "harness A's stream leaked harness B's record — the stream-name isolation \
         `boot_sharing_redis` relies on is broken: {stream_a}"
    );
    assert!(
        stream_b.contains("marker-from-harness-b"),
        "harness B's own stream must contain B's record; got: {stream_b}"
    );
    assert!(
        !stream_b.contains("marker-from-harness-a"),
        "harness B's stream leaked harness A's record — the stream-name isolation \
         `boot_sharing_redis` relies on is broken: {stream_b}"
    );
}
