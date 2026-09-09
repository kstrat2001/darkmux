//! (#2544) `runtime/` is its own standalone Cargo workspace (see
//! `runtime/Cargo.toml`'s own comment), deliberately excluded from this
//! repo's root workspace. Two of its tests (`runtime/src/tools/mod.rs`)
//! read wire-format fixtures whose CANONICAL copy lives at repo-root
//! `tests/fixtures/{create_mod_wire,finding_key_cases}.json` — the same
//! files `crates/darkmux-crew` reads directly, so both sides of the
//! runtime<->host wire boundary assert against one shared fixture.
//!
//! `cargo mutants --manifest-path runtime/Cargo.toml --copy-vcs true` (the
//! #2544 second mutation invocation `quality.yml` runs) copies ONLY
//! `runtime/` into its scratch build directory — confirmed via its own
//! `copy_tree` debug log, `paths: ["<repo>/runtime"]` — so a fixture path
//! reaching one directory above `runtime/` cannot resolve inside that copy.
//! Before this fix, both tests failed there on EVERY invocation (mutant or
//! not), which fails the whole run's baseline (`BaselineFailed`, exit 4 —
//! nothing is ever mutated) — a permanent false red on every runtime PR,
//! not something the diff under test could ever have caused.
//!
//! The fix vendors a read-only copy of both files at
//! `runtime/tests/fixtures/` (inside the copied tree) and points the
//! runtime-side tests there instead of one directory up. This test is the
//! guard against that vendored copy silently drifting from the canonical
//! one — which would let the runtime and host sides disagree about the
//! wire format while every test suite involved stays green. Regenerate the
//! vendored copy (`cp tests/fixtures/<name> runtime/tests/fixtures/<name>`)
//! if this ever fails; never hand-edit it back into sync.
//!
//! (#2602) Walks `runtime/tests/fixtures/` rather than checking a hardcoded
//! `["create_mod_wire.json", "finding_key_cases.json"]` pair: a THIRD
//! shared-fixture read added later (a new runtime-side test that vendors
//! another repo-root fixture the same way) would silently go uncovered by
//! a hardcoded list — it would still be caught loudly by the crate's own
//! baseline the moment its test tried to read the missing vendored file
//! (`BaselineFailed`, same as the original #2544 failure), so this is
//! defense in depth rather than the only backstop, but a directory walk
//! costs nothing and removes the dependency on remembering to touch this
//! file too. Only TOP-LEVEL files with a same-named canonical counterpart
//! under `tests/fixtures/` are compared — `runtime/tests/fixtures/` also
//! holds fixtures that were never vendored from the repo root at all
//! (`README.md`, the `compactor_*` prompt fixtures, `cycle-traces/`,
//! `promoter-emissions/`), and those have no canonical copy to drift from.
//!
//! (#2602 round 2) The walk's own vacuity check — "did we find at least the
//! two known pairs" — was a COUNT, not a presence check: `checked.len() >=
//! 2` passes the moment ANY two same-named vendored/canonical pairs match,
//! including two that have nothing to do with the wire fixtures this guard
//! exists to protect. If both real pairs (`create_mod_wire.json`,
//! `finding_key_cases.json`) were ever renamed or removed on both sides
//! while two unrelated same-named files happened to sit in both
//! directories, this test would report "found 2, expected >= 2" and pass —
//! green while checking zero real vendored copies, the same silent-vacuity
//! shape #1716's own summary-script self-test exists to catch elsewhere in
//! this repo. `check_known_fixtures_present` below asserts the NAMED
//! fixtures are present, matching what the panic message already claimed.
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;

/// The two vendored wire fixtures this guard exists to protect (#2544).
/// Named once so the presence check and its panic message can never name a
/// fixture the check doesn't actually look for, or vice versa.
const KNOWN_VENDORED_FIXTURES: &[&str] = &["create_mod_wire.json", "finding_key_cases.json"];

/// (#2602 round 2) Pure presence check, split out of the main test so it can
/// be red-proved directly against a fabricated `checked` list with no
/// filesystem I/O involved. Returns `Err` naming the first missing fixture
/// unless every name in `KNOWN_VENDORED_FIXTURES` is actually present in
/// `checked` — NOT just `checked.len() >= KNOWN_VENDORED_FIXTURES.len()`,
/// which is the vacuous version this replaces (see the module doc comment).
fn check_known_fixtures_present(checked: &[OsString]) -> Result<(), String> {
    for name in KNOWN_VENDORED_FIXTURES {
        if !checked.iter().any(|c| c.as_os_str() == OsStr::new(name)) {
            return Err(format!(
                "expected {name} among the vendored wire fixtures actually found under \
                 runtime/tests/fixtures/; found {checked:?} — did the vendored copy get \
                 renamed or removed, or did tests/fixtures/ itself move?"
            ));
        }
    }
    Ok(())
}

#[test]
fn runtime_vendored_wire_fixtures_match_the_canonical_copies() {
    let canonical_dir = Path::new("tests/fixtures");
    let vendored_dir = Path::new("runtime/tests/fixtures");
    let entries = fs::read_dir(vendored_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", vendored_dir.display()));

    let mut checked = Vec::new();
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("read_dir entry in {}: {e}", vendored_dir.display()));
        let vendored = entry.path();
        if !vendored.is_file() {
            continue; // subdirectories (cycle-traces/, promoter-emissions/, ...) aren't vendored copies
        }
        let name = entry.file_name();
        let canonical = canonical_dir.join(&name);
        if !canonical.is_file() {
            continue; // not vendored from the repo root (README.md, compactor_*, ...)
        }
        let canonical_text = fs::read_to_string(&canonical)
            .unwrap_or_else(|e| panic!("read {}: {e}", canonical.display()));
        let vendored_text = fs::read_to_string(&vendored)
            .unwrap_or_else(|e| panic!("read {}: {e}", vendored.display()));
        assert_eq!(
            canonical_text, vendored_text,
            "{} has drifted from {} — runtime/'s copy is vendored (#2544) because \
             cargo-mutants' `--manifest-path runtime/Cargo.toml` copy scope cannot \
             reach the repo-root original; regenerate the vendored copy from the \
             canonical one, don't hand-edit either into agreement",
            vendored.display(),
            canonical.display(),
        );
        checked.push(name);
    }

    // (#2602 round 2) Assert the SPECIFIC known fixtures were found, not
    // merely that at least as many pairs matched as the list is long — see
    // `check_known_fixtures_present`'s doc comment for the vacuity this
    // closes, and the two unit tests below for the red-prove.
    if let Err(msg) = check_known_fixtures_present(&checked) {
        panic!("{msg}");
    }
}

#[test]
fn known_fixture_check_rejects_two_unrelated_same_named_matches() {
    // The reviewer's exact vacuity proof: both real vendored pairs gone,
    // replaced by two unrelated same-named files present on both sides of
    // the vendored/canonical split. The OLD `checked.len() >= 2` threshold
    // would have passed this; the presence check must reject it.
    let checked = vec![
        OsString::from("unrelated_a.json"),
        OsString::from("unrelated_b.json"),
    ];
    assert!(
        check_known_fixtures_present(&checked).is_err(),
        "a checked list missing both real vendored fixtures must be rejected, \
         even though it has >= 2 entries from unrelated same-named files"
    );
}

#[test]
fn known_fixture_check_rejects_one_of_the_two_missing() {
    // Half the same vacuity: one real fixture present, the other silently
    // renamed or removed. `checked.len() >= 2` (satisfied here too, via one
    // real fixture plus one unrelated same-named file) would also pass this.
    let checked = vec![
        OsString::from(KNOWN_VENDORED_FIXTURES[0]),
        OsString::from("unrelated.json"),
    ];
    assert!(check_known_fixtures_present(&checked).is_err());
}

#[test]
fn known_fixture_check_accepts_the_real_pair() {
    let checked = vec![
        OsString::from(KNOWN_VENDORED_FIXTURES[0]),
        OsString::from(KNOWN_VENDORED_FIXTURES[1]),
    ];
    assert!(check_known_fixtures_present(&checked).is_ok());
}
