//! Golden test (#1222 Phase B packet 3, known-answer gate 10): a small
//! synthetic TS worktree + diff under `tests/fixtures/bundle/`, asserted
//! against a COMMITTED, pretty-printed `BundleSet` JSON fixture. All
//! fixture content (the TS source, the diff, the golden output) is
//! synthetic — authored for this test, not copied from any corpus.
//!
//! To regenerate `golden.json` after a deliberate behavior change, run:
//! `DARKMUX_BUNDLE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab --test bundle_golden`
//! then review the diff before committing — this test is the fidelity
//! gate, so a golden update should never be routine.

use darkmux_lab::lab::bundle::{build_bundles, FileSource};
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle")
}

#[test]
fn golden_bundle_set_matches_fixture() {
    let dir = fixture_dir();
    let worktree = dir.join("worktree");
    let diff_text = std::fs::read_to_string(dir.join("diff.patch")).expect("read diff.patch fixture");
    let source = FileSource::worktree(&worktree);
    let set = build_bundles(&source, &diff_text).expect("build_bundles over the synthetic fixture");

    let actual = serde_json::to_string_pretty(&set).expect("serialize BundleSet");
    let golden_path = dir.join("golden.json");

    if std::env::var("DARKMUX_BUNDLE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(&golden_path, format!("{actual}\n")).expect("write golden.json");
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).expect("read golden.json — run with DARKMUX_BUNDLE_UPDATE_GOLDEN=1 to generate it");
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "BundleSet output drifted from the committed golden fixture at {}.\n\
         If this drift is an intended behavior change, regenerate with:\n\
         DARKMUX_BUNDLE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab --test bundle_golden\n\
         then review the diff before committing.",
        golden_path.display()
    );
    // Sanity checks on the fixture's shape — these fail loud if the
    // fixture stops exercising what it's meant to demonstrate (not just
    // a byte-for-byte regression on the golden file itself).
    assert!(set.bundles.iter().any(|b| b.fact_family == "param-flow"));
    assert!(set.bundles.iter().any(|b| b.fact_family == "differential"));
    assert!(set.bundles.iter().any(|b| b.fact_family == "siblings"));
    assert!(
        set.bundles
            .iter()
            .any(|b| b.facts.iter().any(|f| f.starts_with("default parameter(s):"))),
        "expected a default-parameter fact somewhere in the fixture's output"
    );
}
