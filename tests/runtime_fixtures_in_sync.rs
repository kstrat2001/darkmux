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
use std::fs;
use std::path::Path;

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

    // A glob that silently matched nothing would be a WORSE guard than the
    // hardcoded list it replaced — prove it actually found the two known
    // vendored fixtures, not just that it ran without error.
    assert!(
        checked.len() >= 2,
        "expected at least the two known vendored wire fixtures \
         (create_mod_wire.json, finding_key_cases.json) under {}; found {} ({:?}) — \
         did the vendored copies get renamed, removed, or did tests/fixtures/ itself move?",
        vendored_dir.display(),
        checked.len(),
        checked,
    );
}
