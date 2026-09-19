//! (#2808) Conformance: the compactor window reaches the dispatch through the
//! resolver, not by some other route.
//!
//! **What this guards, narrowly.** `apply_compactor_window` is the one place
//! that resolves the compactor's window and records it for the runtime, and it
//! is unit-tested behaviorally (`apply_compactor_window_records_the_compactors_
//! own_window_not_the_primarys`). What a unit test on that function cannot see
//! is somebody ceasing to CALL it — inlining the assignment back into
//! `dispatch_via_internal`'s residency block, which needs a live LMStudio and a
//! real profile to reach, so no test observes it. That is the gap this file
//! covers, and the only one.
//!
//! **What an earlier version of this file claimed, and why it was wrong.** It
//! swept for the spelling of the assignment's right-hand side and said it
//! "proves the assignment's right-hand side is the resolver's output".
//! Literally true, and it invited a reader to believe far more. A realistic
//! regression walks past it: rewriting what FEEDS the resolver, one line up,
//! while leaving `= load_window` byte-identical, reverts #2808 and #1616
//! together and left that sweep plus all 1,929 crate tests green. The
//! behavioral test now catches that; this file covers only the wiring.

use std::fs;

const SITE: &str = "src/dispatch_internal.rs";

#[test]
fn the_dispatch_path_still_routes_through_the_compactor_window_seam() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dispatch_internal.rs"))
        .expect("dispatch_internal.rs must be readable");

    let calls = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .filter(|l| l.contains("apply_compactor_window(&mut compaction"))
        .count();

    assert_eq!(
        calls, 1,
        "{SITE} must call `apply_compactor_window` exactly once on the dispatch path. \
         Without it the runtime gets no `--compactor-context-window`, bounds its \
         compaction excerpt by nothing, and posts a ~30,000-token excerpt to a \
         16,000-token compactor — HTTP 400 on every compaction, silently, because \
         each refusal is a non-fatal skip record (#2808)."
    );

    // And nothing else may write the field: a second writer would race the
    // seam and make the unit test's guarantee local rather than total.
    let writers: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .filter(|l| !l.starts_with("if let") && !l.starts_with("let "))
        .filter(|l| !l.contains("=="))
        .filter(|l| match l.split_once('=') {
            Some((lhs, _)) => lhs.contains("compactor_context_window"),
            None => false,
        })
        .collect();

    assert_eq!(
        writers.len(),
        1,
        "exactly one assignment to `compactor_context_window` may exist, and it belongs \
         inside `apply_compactor_window` where it is tested. Found:\n    {}",
        writers.join("\n    ")
    );
}
