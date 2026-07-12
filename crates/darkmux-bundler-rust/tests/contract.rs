//! Contract test: the REAL compiled binary, dispatched through
//! `darkmux-lab`'s own `external_bundles` — the exact function
//! `darkmux pr-review run --bundler <cmd>` calls in production. This is
//! the proof this plugin actually satisfies the frozen `--bundler`
//! contract end to end, not just that its internal functions return the
//! right Rust types.

use darkmux_lab::lab::bundle::external::external_bundles;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_darkmux-bundler-rust")
}

#[test]
fn diff_only_mode_satisfies_the_frozen_contract() {
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("d.diff");
    std::fs::write(
        &diff_path,
        [
            "+++ b/src/lib.rs",
            "@@ -1,5 +1,4 @@",
            " fn process(x: u32) -> u32 {",
            "-    validate(x);",
            "     let y = x + 1;",
            "     y",
            " }",
            "",
        ]
        .join("\n"),
    )
    .unwrap();

    // `worktree: None` — the exact call shape darkmux's own no-checkout
    // self-review workflow uses (`--github`/`--head-sha`, never
    // `--worktree`).
    let set = external_bundles(bin_path(), None, &diff_path).expect("plugin must satisfy the contract");
    assert_eq!(set.bundles.len(), 1);
    let b = &set.bundles[0];
    assert_eq!(b.id, "process@src/lib.rs");
    assert_eq!(b.fact_family, "differential");
    assert!(!b.truncated);
    assert!(b.facts.iter().any(|f| f.contains("`validate(...)`")));
}

#[test]
fn worktree_mode_satisfies_the_frozen_contract() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn process(x: u32) -> u32 {\n    let a = 1;\n    a + x\n}\n",
    )
    .unwrap();
    let diff_path = dir.path().join("d.diff");
    std::fs::write(
        &diff_path,
        ["+++ b/src/lib.rs", "@@ -2,1 +2,1 @@", "-    let a = 0;", "+    let a = 1;", ""].join("\n"),
    )
    .unwrap();

    let set = external_bundles(bin_path(), Some(dir.path()), &diff_path).expect("plugin must satisfy the contract");
    assert_eq!(set.bundles.len(), 1);
    assert_eq!(set.bundles[0].id, "process@src/lib.rs");
}

#[test]
fn no_rust_files_in_diff_fails_loudly_not_silently() {
    // #1113 doctrine: never a silent pass. A diff with zero .rs content
    // must fail the dispatch with a clear reason, never emit an empty
    // bundle array that reads as "nothing to review."
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("d.diff");
    std::fs::write(&diff_path, "+++ b/README.md\n@@ -1,1 +1,1 @@\n-old\n+new\n").unwrap();

    let err = external_bundles(bin_path(), None, &diff_path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("exited with") || msg.contains("no .rs files"),
        "expected a loud, named failure, got: {msg}"
    );
}

#[test]
fn a_brand_new_file_resolves_every_function_not_just_one() {
    // The multi-function-per-hunk case: unified diff has no old-side
    // content for a new file, so the whole file lands in ONE hunk.
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("d.diff");
    std::fs::write(
        &diff_path,
        [
            "+++ b/src/new.rs",
            "@@ -0,0 +1,7 @@",
            "+fn first() {",
            "+    1;",
            "+}",
            "+",
            "+fn second() {",
            "+    2;",
            "+}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();

    let set = external_bundles(bin_path(), None, &diff_path).expect("plugin must satisfy the contract");
    assert_eq!(set.bundles.len(), 2, "both functions in the one hunk must be bundled, not just one");
    let ids: Vec<&str> = set.bundles.iter().map(|b| b.id.as_str()).collect();
    assert!(ids.contains(&"first@src/new.rs"));
    assert!(ids.contains(&"second@src/new.rs"));
}

