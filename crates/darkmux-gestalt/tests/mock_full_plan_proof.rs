//! Full-round-trip proof (mock-model harness, v1, item 3): `plan_acquire`,
//! `plan_waves`, and `plan_release` can compute a real plan — and that plan
//! can be genuinely EXECUTED against a `ModelHost`/`ResourceProbe` — with
//! zero subprocess calls, zero network calls, and zero real `lms` binary
//! anywhere in the loop.
//!
//! Two things make this a proof rather than a restatement of the crate's
//! own unit tests:
//!
//!  1. It's a `tests/` integration test — it can only see this crate's
//!     `pub` surface, the exact surface a caller outside `darkmux-gestalt`
//!     (a future executor, `darkmux gestalt plan --dry-run`, this
//!     workspace's `darkmux-crew`) would use.
//!  2. It chains a `Plan`'s actions into ACTUAL calls against the mock
//!     host (`MockHost::load`/`unload`) — the crate's existing tests
//!     assert `Plan` shape or `MockHost` mechanics in isolation; this test
//!     walks a `Plan` and drives the host with it, proving the two sides
//!     fit together end to end. There is no production "executor" yet
//!     (packet 3, per the crate's module docs) — the walk here is a
//!     deliberately tiny, test-local stand-in for one, not a new
//!     production abstraction.
//!
//! `MockHost`/`MockProbe` (`darkmux_gestalt::mock`) already implement
//! `ModelHost`/`ResourceProbe` — this test reuses them rather than
//! building new mock ports, since duplicating what the crate already
//! ships (and already golden-tests) would just be a second copy to keep
//! in sync.
//!
//! Zero I/O is a STRUCTURAL guarantee, not merely an assertion, for
//! `plan_acquire`/`plan_release`/`plan_waves` themselves: the crate's own
//! module docs commit every symbol outside `ports`/`mock` to zero
//! `std::process`/`std::fs`/clock reads. The only thing that COULD reach a
//! real `lms` binary here is the host port — and this test's host is
//! `MockHost`, an in-memory `Vec`-backed double with no subprocess/network
//! code anywhere in its implementation (see `src/mock.rs`).

use darkmux_gestalt::mock::{HostOp, MockHost, MockProbe};
use darkmux_gestalt::{
    desired, plan_acquire, plan_release, plan_waves, AcquireOpts, AcquireScope, Action, Budget,
    CallerIntent, Deadline, DesiredEntry, FixedEstimator, ModelHost, PoolFact, PoolId, Pools,
    ResourceProbe, WaveMode,
};
use std::collections::BTreeMap;
use std::time::Duration;

fn deadline() -> Deadline {
    Deadline(Duration::from_secs(30))
}

/// One "unified memory" pool with generous headroom — this test is about
/// the planning/execution WIRING, not pool-pressure edge cases (those are
/// the crate's own extensive planner test-table's job).
fn roomy_pools() -> Pools {
    let mut pools = BTreeMap::new();
    pools.insert(
        PoolId("unified".into()),
        PoolFact { capacity_bytes: 64_000_000_000, available_bytes: 40_000_000_000 },
    );
    pools
}

#[test]
fn mock_ports_drive_a_full_acquire_then_release_round_trip() {
    // ── Facts: nothing resident yet, one catalog entry, roomy pool ───────
    let mut host = MockHost::new().cataloged("mock-model", 5_000_000_000);
    let mut probe = MockProbe(roomy_pools());
    let facts = host.facts(probe.0.clone(), Budget::default());

    // ── Desired: one primary seat wants "mock-model" at 8K ctx ───────────
    let desired_entries = vec![DesiredEntry {
        model_key: "mock-model".to_string(),
        n_ctx: Some(8_000),
        identifier: None,
        remote: false,
        seat: "primary".to_string(),
    }];
    let (placements, quarantined) = desired::ingest(&desired_entries);
    assert!(quarantined.is_empty(), "a well-formed local entry must never quarantine");
    assert_eq!(placements.len(), 1);

    // ── plan_acquire: pure function, Facts in, Plan out ──────────────────
    let est = FixedEstimator(BTreeMap::from([("mock-model".to_string(), 5_000_000_000)]));
    let opts = AcquireOpts { intent: CallerIntent::Auto, scope: AcquireScope::Additive };
    let plan = plan_acquire(&placements, &facts, opts, &est);

    assert_eq!(plan.actions.len(), 1, "exactly one placement ⇒ exactly one planned action");
    let (model_key, identifier, min_ctx) = match &plan.actions[0].action {
        Action::Load { model_key, identifier, min_ctx } => {
            (model_key.clone(), identifier.clone(), *min_ctx)
        }
        other => panic!("expected a fresh Load (nothing resident yet), got {other:?}"),
    };
    assert_eq!(model_key, "mock-model");
    assert_eq!(identifier, "darkmux:mock-model", "namespaced per the #1274 convention");
    assert_eq!(min_ctx, 8_000);

    // ── plan_waves: same Facts, same placements — schedules the acquire ──
    let schedule = plan_waves(&placements, &facts, &est, WaveMode::Auto)
        .expect("one placement in a roomy pool never refuses ForceParallel-shaped scheduling");
    assert_eq!(schedule.waves.iter().map(|w| w.len()).sum::<usize>(), 1, "one placement scheduled exactly once");

    // ── Execute the plan's one Load against the MockHost — a tiny
    //    test-local stand-in for the not-yet-built production executor
    //    (see the module doc). This is the "does the plan the pure core
    //    computed actually drive the port correctly" proof.
    for planned in &plan.actions {
        if let Action::Load { model_key, identifier, min_ctx } = &planned.action {
            host.load(model_key, identifier, *min_ctx, deadline())
                .expect("mock load succeeds against a cataloged, roomy-pool model");
        }
    }
    // Note: `MockHost::facts()` (used to build `facts` above) reads the
    // mock's `residents`/`catalog` fields directly and does NOT go through
    // the `ModelHost::list_resident`/`list_catalog` trait methods, so it
    // records no ops — only the actual `load` call below does.
    assert_eq!(
        host.ops,
        vec![HostOp::Load {
            model_key: "mock-model".into(),
            identifier: "darkmux:mock-model".into(),
            min_ctx: 8_000,
        }],
        "the host saw exactly the one Load the plan drove — nothing else, and certainly \
         no subprocess/network call, since MockHost has none to make"
    );

    // ── Re-probe: the resident is now visible, proving state genuinely
    //    evolved from executing the plan (not just recorded as an op).
    let residents_after_load = host.list_resident().expect("list succeeds");
    assert_eq!(residents_after_load.len(), 1);
    assert_eq!(residents_after_load[0].identifier, "darkmux:mock-model");

    // ── plan_release: the seat is done — release it back down. Fresh
    //    Facts snapshot (per the crate's #1274 staleness discipline: facts
    //    are snapshots, never mutated in place) reflecting the new resident.
    let facts_after_load = host.facts(probe.pools().expect("probe never fails in this test"), Budget::default());
    let release_plan = plan_release(&placements, &[], &facts_after_load);
    assert_eq!(release_plan.actions.len(), 1, "the one resident darkmux placed, and nothing still active, ⇒ one Unload");
    let target = match &release_plan.actions[0].action {
        Action::Unload { target } => target.clone(),
        other => panic!("expected an Unload, got {other:?}"),
    };
    assert_eq!(target.identifier(), "darkmux:mock-model");

    host.unload(&target, deadline()).expect("mock unload succeeds against the resident it just loaded");
    let residents_after_release = host.list_resident().expect("list succeeds");
    assert!(residents_after_release.is_empty(), "release plan's Unload, executed, leaves nothing resident");

    // ── Final honesty check: nothing in this test ever touched a
    //    subprocess or the network. `MockHost`/`MockProbe` are in-memory
    //    doubles (see src/mock.rs) — there is no `lms` binary, no `docker`,
    //    no HTTP client anywhere in the call graph this test exercised.
    assert_eq!(
        host.ops,
        vec![
            HostOp::Load {
                model_key: "mock-model".into(),
                identifier: "darkmux:mock-model".into(),
                min_ctx: 8_000,
            },
            HostOp::ListResident,
            HostOp::Unload { identifier: "darkmux:mock-model".into() },
            HostOp::ListResident,
        ],
        "the complete op sequence this test drove: Load, the post-load list_resident check, \
         Unload, the post-release list_resident check — every one of them a Vec mutation on \
         MockHost, never a syscall"
    );
}
