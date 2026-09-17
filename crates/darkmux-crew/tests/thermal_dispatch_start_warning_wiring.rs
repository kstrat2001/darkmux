//! (#2774 round-9 MF1) Conformance: the dispatch-start thermal warning is
//! COMPOSED by the governor, not re-derived at the print site.
//!
//! The content of those lines is unit-tested in
//! `thermal_governor::tests` (`dispatch_start_warnings`), and that is
//! where the behavior lives. What no test could see is the WIRING: the
//! print site is eleven lines inside `spawn_guarded_sampler`'s ~250-line
//! setup block in a 600KB module, writing to stderr, reached only by a
//! real container-backed dispatch. Round 4 already paid for this exact
//! blind spot once — it gave this surface the band-disarm notes and its
//! breaker sentence stayed a hardcoded string literal, so when
//! `min_cpu_speed_limit_pct > 100` turned the breaker into a
//! trip-on-every-dispatch, the line went on calling it ordinary
//! protection and nothing went red.
//!
//! This is the same lint shape `darkmux-profiles`'s `pin_cwd_conformance`
//! and this crate's own `cwd_policy_conformance` use for the same class of
//! hazard: a rule that lives in one place, checked by reading the source
//! rather than by a roster someone has to remember to update. It is
//! deliberately modest — it asserts the call is present and the old
//! hand-rolled sentence is gone, nothing about what the lines SAY.

use std::path::PathBuf;

fn dispatch_internal_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dispatch_internal.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn the_dispatch_start_thermal_warning_comes_from_the_governor() {
    let src = dispatch_internal_src();
    assert!(
        src.contains("thermal_governor.dispatch_start_warnings()"),
        "the dispatch-start thermal warning must be composed by \
         `ThermalGovernor::dispatch_start_warnings`, whose content IS tested — deleting this \
         call silently removes the operator's only dispatch-time notice that the thermal ladder \
         or breaker is misconfigured"
    );
}

#[test]
fn the_old_hand_rolled_breaker_sentence_is_not_reintroduced() {
    let src = dispatch_internal_src();
    assert!(
        !src.contains("and the sustained cpu_speed_limit floor) still runs."),
        "this sentence is now the governor's to write, and only when the floor is ORDINARY — a \
         copy here would assert unconditionally that the breaker is behaving normally, which is \
         false whenever `min_cpu_speed_limit_pct` is above the 100% ceiling of the reading it is \
         compared against"
    );
}
