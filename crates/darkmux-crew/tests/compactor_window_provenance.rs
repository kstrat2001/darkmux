//! (#2808) Conformance: the compactor's window must come from the COMPACTOR
//! resolver, never from the primary's `context_window`.
//!
//! The defect this fix closes is a confusion between two windows that are
//! both `Option<u32>` and sit two lines apart: the primary model's declared
//! `n_ctx` and the compactor's. Compaction bounded its excerpt by neither,
//! posted the whole middle, and a 32,000-token primary beside a 16,000-token
//! compactor refused every compaction with HTTP 400 — 69 in a row on the
//! measured run, zero successes, thread running away to 49,000.
//!
//! **Why a source sweep and not a behavioral test.** The assignment lives
//! inside `dispatch_via_internal`'s residency block, which needs a live
//! LMStudio and a real profile on disk to reach, so no unit test observes it.
//! The argv test one module over pins that the FLAG is emitted, but it builds
//! `CompactionDispatchArgs` literally, so it cannot see where the value came
//! from — mutating the assignment to `compaction.context_window` (the
//! primary's) leaves the entire 1,929-test crate suite green. That mutation
//! is precisely the bug, re-introduced, so it needs a guard.
//!
//! **What this does and does not prove.** It proves the assignment's
//! right-hand side is the resolver's output. It does not prove the resolver
//! is correct — that is `resolve_compactor_load_window`'s own unit tests,
//! which pin that it prefers the compactor's declared `n_ctx` and falls back
//! to the primary's only as a NAMED fallback (#1616).

use std::fs;

/// The one production site that may set this field.
const SITE: &str = "src/dispatch_internal.rs";

#[test]
fn the_compactor_window_is_assigned_from_the_compactor_resolver() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dispatch_internal.rs"))
        .expect("dispatch_internal.rs must be readable");

    let assignments: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        // The field must be on the LEFT of the `=`: `if let Some(w) =
        // compaction.compactor_context_window` READS it and is not a site
        // this guard is about.
        .filter(|l| !l.starts_with("if let") && !l.starts_with("let "))
        .filter(|l| !l.contains("=="))
        .filter(|l| match l.split_once('=') {
            Some((lhs, _)) => lhs.contains("compactor_context_window"),
            None => false,
        })
        .collect();

    assert!(
        !assignments.is_empty(),
        "{SITE} no longer assigns `compactor_context_window` at all — the runtime \
         is back to bounding its compaction excerpt by nothing (#2808)"
    );

    for line in &assignments {
        assert!(
            line.contains("load_window"),
            "`compactor_context_window` must be assigned from \
             `resolve_compactor_load_window`'s output, which prefers the COMPACTOR's own \
             declared n_ctx. This line assigns something else, and if that something is \
             `compaction.context_window` it is the primary's window — the exact confusion \
             #2808 is about, and one the whole crate suite stays green under:\n    {line}"
        );
    }
}
