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
use std::fs;
use std::path::Path;

#[test]
fn runtime_vendored_wire_fixtures_match_the_canonical_copies() {
    for name in ["create_mod_wire.json", "finding_key_cases.json"] {
        let canonical = Path::new("tests/fixtures").join(name);
        let vendored = Path::new("runtime/tests/fixtures").join(name);
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
    }
}
