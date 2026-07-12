//! Contract test: spawns the REAL compiled binary and validates its
//! stdout directly against the documented `--bundler` JSON shape — no
//! darkmux dependency of any kind, matching how a real consumer (or a
//! real third-party plugin author checking their OWN output) would
//! verify conformance: read the docs, check the JSON. This is the
//! genuinely-standalone counterpart to the cross-project test this
//! plugin used to have when it lived inside darkmux's own workspace.

use std::process::Command;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_darkmux-bundler-rust")
}

fn run(diff_path: &std::path::Path, worktree: Option<&std::path::Path>) -> std::process::Output {
    let mut cmd = Command::new(bin_path());
    if let Some(wt) = worktree {
        cmd.args(["--worktree", wt.to_str().unwrap()]);
    }
    cmd.args(["--diff", diff_path.to_str().unwrap()]);
    cmd.output().expect("failed to run the compiled binary")
}

#[test]
fn diff_only_mode_emits_the_documented_contract_shape() {
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

    let out = run(&diff_path, None);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    let bundles = value.get("bundles").expect("top-level `bundles` key").as_array().expect("`bundles` is an array");
    assert_eq!(bundles.len(), 1);
    let b = &bundles[0];
    assert_eq!(b["id"], "process@src/lib.rs");
    assert_eq!(b["fact_family"], "differential");
    assert!(b.get("truncated").is_none(), "false truncated must be OMITTED per the frozen contract");
    let code = b["code"].as_array().expect("`code` is an array");
    assert_eq!(code.len(), 1);
    assert_eq!(code[0]["path"], "src/lib.rs");
    assert!(code[0]["start"].is_u64());
    assert!(code[0]["end"].is_u64());
    let facts = b["facts"].as_array().expect("`facts` is an array");
    assert!(facts.iter().any(|f| f.as_str().unwrap().contains("`validate(...)`")));
}

#[test]
fn worktree_mode_emits_the_documented_contract_shape() {
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

    let out = run(&diff_path, Some(dir.path()));
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let bundles = value["bundles"].as_array().unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0]["id"], "process@src/lib.rs");
}

#[test]
fn no_rust_files_fails_loudly_not_silently() {
    // #1113 doctrine: never a silent pass. A diff with zero .rs content
    // must exit non-zero with a clear stderr reason, never print an
    // empty bundle array that a caller might mistake for "nothing to
    // review."
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("d.diff");
    std::fs::write(&diff_path, "+++ b/README.md\n@@ -1,1 +1,1 @@\n-old\n+new\n").unwrap();

    let out = run(&diff_path, None);
    assert!(!out.status.success(), "must exit non-zero on a diff with no .rs content");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no .rs files"), "expected a named reason, got: {stderr}");
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

    let out = run(&diff_path, None);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let bundles = value["bundles"].as_array().unwrap();
    assert_eq!(bundles.len(), 2, "both functions in the one hunk must be bundled, not just one");
    let ids: Vec<&str> = bundles.iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"first@src/new.rs"));
    assert!(ids.contains(&"second@src/new.rs"));
}
