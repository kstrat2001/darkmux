//! Integration tests for the darkmux CLI.
//!
//! These spawn the compiled binary and assert its observable surface:
//! exit codes, stdout/stderr shape, and behavior across the basic
//! subcommands. Tests that need a real `lms` are skipped when the
//! `DARKMUX_LMS_BIN` is unset (i.e., not in CI without LMStudio).

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn fixture_json() -> &'static str {
    r#"{
        "profiles": {
            "fast": {
                "description": "bounded tasks",
                "models": [
                    {"id": "model-a", "n_ctx": 32000, "role": "primary"}
                ]
            },
            "deep": {
                "description": "long tasks",
                "models": [
                    {"id": "model-a", "n_ctx": 100000, "role": "primary"},
                    {"id": "model-b", "n_ctx": 50000, "role": "compactor"}
                ]
            }
        },
        "default_profile": "fast"
    }"#
}

#[test]
fn version_outputs_semver() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux"));
}

#[test]
fn help_lists_subcommands() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("swap"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("profiles"))
        .stdout(predicate::str::contains("lab"));
}

#[test]
fn profiles_lists_from_explicit_config() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["profiles", "--config", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast"))
        .stdout(predicate::str::contains("deep"))
        .stdout(predicate::str::contains("(default)"));
}

#[test]
fn profiles_errors_when_config_missing() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["profiles", "--config", "/no/such/path.json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry")),
        );
}

#[test]
fn swap_dry_run_succeeds_without_real_lms() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    // Force a non-functional `lms` binary. list_loaded falls back to text
    // parsing on JSON failure and treats an empty result as "no models loaded."
    // dry-run skips the actual unload/load calls, so the swap completes cleanly.
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args([
        "swap",
        "fast",
        "--config",
        p.to_str().unwrap(),
        "--dry-run",
        "--quiet",
    ])
    .assert()
    .success();
}

#[test]
fn swap_unknown_profile_errors() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args([
        "swap",
        "nonexistent-profile",
        "--config",
        p.to_str().unwrap(),
        "--dry-run",
        "--quiet",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("not found"));
}

#[test]
fn status_runs_with_explicit_config() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["status", "--config", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("registry:"));
}

#[test]
fn unknown_command_exits_nonzero() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("nonexistent-command").assert().failure();
}

#[test]
fn lab_with_no_subcommand_reports() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab"])
        .assert()
        .stderr(predicate::str::contains("not yet wired").or(predicate::str::contains("lab")));
}

/// End-to-end: `darkmux lab run quick-q` works from a non-source CWD using the
/// embedded built-in workload. This is the headline guarantee of the embedded
/// approach — `cargo install --path .` produces a binary that doesn't need
/// the source tree at runtime.
///
/// Test passes `--runtime openclaw` because the default-internal runtime
/// (post-Sprint-D) requires Docker + LMStudio, which CI doesn't have. The
/// openclaw shell-out path is mockable: we point `--runtime-cmd` at
/// `/usr/bin/true` (Sprint-E replacement for the removed
/// `DARKMUX_RUNTIME_CMD` env var) so the dispatch always "succeeds"
/// without actually hitting any backend. The test verifies that the
/// surrounding plumbing (workload load → provider dispatch → manifest
/// write → run dir creation) works end-to-end from a clean tempdir.
#[test]
fn lab_run_quick_q_from_clean_cwd_uses_embedded_workload() {
    let tmp = TempDir::new().unwrap();
    // Profile registry with `deep` as default.
    let cfg = tmp.path().join("profiles.json");
    fs::write(
        &cfg,
        r#"{
            "profiles": {
                "deep": {
                    "description": "test deep stack",
                    "models": [
                        {"id": "model-a", "n_ctx": 100000, "role": "primary"}
                    ]
                }
            },
            "default_profile": "deep"
        }"#,
    )
    .unwrap();

    // Force project-scope path resolution: paths::resolve(Auto) falls back to
    // ~/.darkmux/ when `./.darkmux/` is absent. Pre-create the project dir so
    // the test writes to the tempdir, not the user's home.
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    // Force an empty templates dir so on-disk lookup doesn't accidentally
    // resolve before the embedded fallback. This proves the embedded path.
    cmd.env(
        "DARKMUX_TEMPLATES_DIR",
        tmp.path().join("nope").to_str().unwrap(),
    );
    cmd.current_dir(tmp.path());
    cmd.args([
        "lab",
        "run",
        "quick-q",
        // Force openclaw so the --runtime-cmd=/usr/bin/true mock applies.
        // Internal-runtime path requires Docker + LMStudio, unavailable
        // in CI. /usr/bin/true exits 0 with empty stdout — the prompt
        // provider treats this as a successful (but empty-reply)
        // dispatch under the openclaw shell-out path.
        "--runtime",
        "openclaw",
        "--runtime-cmd",
        "/usr/bin/true",
        "--config",
        cfg.to_str().unwrap(),
        "--quiet",
    ])
    .assert()
    .success();

    // The run dir should exist under .darkmux/runs/<id>/ in the tempdir,
    // and contain a v2 manifest with the right runId.
    let runs_dir = tmp.path().join(".darkmux").join("runs");
    assert!(
        runs_dir.is_dir(),
        "expected {} to exist",
        runs_dir.display()
    );
    let entries: Vec<_> = fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one run dir");
    let run_dir = entries[0].path();
    let run_id = run_dir.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        run_id.starts_with("quick-q-deep-"),
        "expected run_id to start with workload-profile-, got: {run_id}"
    );

    let manifest_raw = fs::read_to_string(run_dir.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
    assert_eq!(manifest["schemaVersion"].as_u64(), Some(2));
    assert_eq!(manifest["workload"].as_str(), Some("quick-q"));
    assert_eq!(manifest["provider"].as_str(), Some("prompt"));
    assert_eq!(manifest["profile"].as_str(), Some("deep"));
    assert_eq!(manifest["runId"].as_str(), Some(run_id.as_str()));
    assert_eq!(manifest["ok"].as_bool(), Some(true));
}

/// Sprint-E: `--runtime-cmd <path>` overrides the openclaw binary used
/// by the shell-out path, replacing the pre-Sprint-E `DARKMUX_RUNTIME_CMD`
/// env var.
///
/// The test points `--runtime-cmd` at a binary that DOES NOT EXIST and
/// confirms the dispatch fails with an error mentioning that exact path
/// — proving the flag is reaching the Command::new() call rather than
/// silently falling through to the default `openclaw`. Inverse signal:
/// if the flag weren't plumbed through, we'd either get a clap parse
/// error or a "no such binary `openclaw`" message depending on whether
/// openclaw is on PATH in the test env.
#[test]
fn lab_run_runtime_cmd_flag_overrides_openclaw_binary() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("profiles.json");
    fs::write(
        &cfg,
        r#"{
            "profiles": {
                "deep": {
                    "description": "test deep stack",
                    "models": [
                        {"id": "model-a", "n_ctx": 100000, "role": "primary"}
                    ]
                }
            },
            "default_profile": "deep"
        }"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let bogus_path = "/this/binary/definitely/does/not/exist/darkmux-sprint-e";
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env(
        "DARKMUX_TEMPLATES_DIR",
        tmp.path().join("nope").to_str().unwrap(),
    );
    cmd.current_dir(tmp.path());
    let output = cmd
        .args([
            "lab",
            "run",
            "quick-q",
            "--runtime",
            "openclaw",
            "--runtime-cmd",
            bogus_path,
            "--config",
            cfg.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();

    // Dispatch should NOT succeed (the binary doesn't exist).
    assert!(
        !output.status.success(),
        "expected non-zero exit when --runtime-cmd points at a non-existent binary, got stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Error should mention the bogus path — proves the flag reached the
    // Command::new call.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains(bogus_path),
        "expected error to mention `{bogus_path}` (proving --runtime-cmd plumbed through); got: {combined}"
    );
}

/// Sprint-E QA: passing `--runtime-cmd` without `--runtime openclaw` is
/// an operator-intent conflict (the flag is only consulted under
/// openclaw shell-out). The CLI must bail loudly rather than silently
/// ignoring the flag — Beat 36 doctrine: "no implicit state, operator-
/// explicit intent only."
///
/// `--runtime internal` (the default) + `--runtime-cmd /some/path` →
/// must NOT succeed; error must reference `--runtime openclaw`.
#[test]
fn lab_run_runtime_cmd_without_openclaw_bails_loud() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("profiles.json");
    fs::write(
        &cfg,
        r#"{
            "profiles": {
                "deep": {
                    "description": "test",
                    "models": [
                        {"id": "model-a", "n_ctx": 100000, "role": "primary"}
                    ]
                }
            },
            "default_profile": "deep"
        }"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env(
        "DARKMUX_TEMPLATES_DIR",
        tmp.path().join("nope").to_str().unwrap(),
    );
    cmd.current_dir(tmp.path());
    let output = cmd
        .args([
            "lab",
            "run",
            "quick-q",
            // --runtime defaults to "internal" — explicit here for
            // clarity that the test exercises the conflict path.
            "--runtime",
            "internal",
            "--runtime-cmd",
            "/opt/aider/aider",
            "--config",
            cfg.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "expected non-zero exit when --runtime-cmd set without --runtime openclaw"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--runtime openclaw"),
        "expected stderr to point operator at `--runtime openclaw`; got: {stderr}"
    );
}

/// `notebook list` enumerates .md files and prints aligned columns.
#[serial_test::serial]
#[test]
fn notebook_list_shows_entries() {
    let tmp = TempDir::new().unwrap();
    let nb_dir = tmp.path().join("notebook");
    fs::create_dir_all(&nb_dir).unwrap();

    // Create a few notebook entries.
    fs::write(
        nb_dir.join("2026-05-10-run-a.md"),
        "<!-- darkmux:notebook-entry: run=abc123 machine=m5-home date=2026-05-10 -->\n\nContent A.",
    )
    .unwrap();
    fs::write(
        nb_dir.join("2026-05-11-run-b.md"),
        "<!-- darkmux:notebook-entry: run=def456 machine=m3-laptop date=2026-05-11 -->\n\nContent B.",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    // Set notebook dir via env var.
    cmd.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("notebook")
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("2026-05-11"))
        .stdout(predicate::str::contains("2026-05-10"))
        .stdout(predicate::str::contains("def456"))
        .stdout(predicate::str::contains("abc123"));
}

/// `notebook list --machine` filters entries.
#[serial_test::serial]
#[test]
fn notebook_list_machine_filter() {
    let tmp = TempDir::new().unwrap();
    let nb_dir = tmp.path().join("notebook");
    fs::create_dir_all(&nb_dir).unwrap();

    fs::write(
        nb_dir.join("e1.md"),
        "<!-- darkmux:notebook-entry: run=r1 machine=m5-home date=2026-05-10 -->\n",
    )
    .unwrap();
    fs::write(
        nb_dir.join("e2.md"),
        "<!-- darkmux:notebook-entry: run=r2 machine=m3-laptop date=2026-05-11 -->\n",
    )
    .unwrap();

    // Filter to m5-home.
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("notebook")
        .arg("list")
        .arg("--machine")
        .arg("m5-home")
        .assert()
        .success()
        .stdout(predicate::str::contains("r1"))
        .stdout(predicate::str::contains("r2").not());

    // Filter to nonexistent machine → no output.
    let mut cmd2 = Command::cargo_bin("darkmux").unwrap();
    cmd2.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("notebook")
        .arg("list")
        .arg("--machine")
        .arg("nonexistent")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook entries found"));
}

/// `notebook list` with no notebook dir returns error.
#[test]
fn notebook_list_no_dir() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("notebook")
        .arg("list")
        .env("DARKMUX_NOTEBOOK_DIR", "/no/such/path/xyz")
        .assert()
        .failure()
        .stdout(predicate::str::contains("no notebook directory found"));
}

/// `external pull --stdin` echoes stdin to stdout. The other two plugins
/// (`--gh`, `--url`) shell out to `gh`/`curl` and aren't reliable to
/// exercise in CI; their dispatch routing is covered by unit tests in
/// `src/external/mod.rs`.
#[test]
fn external_pull_stdin_passes_through() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("external")
        .arg("pull")
        .arg("--stdin")
        .write_stdin("hello from #113")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from #113"));
}

/// `external pull` with no source flag fails with a clap-level error.
#[test]
fn external_pull_requires_a_source() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("external")
        .arg("pull")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

/// `external pull --gh ... --stdin` fails because the flags are
/// mutually exclusive (clap enforces this via the `source` ArgGroup).
#[test]
fn external_pull_rejects_multiple_sources() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("external")
        .arg("pull")
        .arg("--gh")
        .arg("https://example.invalid/issues/1")
        .arg("--stdin")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

// ── mission migrate integration tests (#148 Task 8) ───────────────────────

fn write_flat_mission_file(root: &std::path::Path, id: &str) {
    let dir = root.join("missions");
    fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "id": id,
        "description": "test",
        "sprint_ids": [],
        "created_ts": 1,
    });
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
}

fn write_flat_sprint_file(root: &std::path::Path, id: &str, mission_id: &str) {
    let dir = root.join("sprints");
    fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "id": id,
        "mission_id": mission_id,
        "description": "test",
        "depends_on": [],
        "created_ts": 1,
    });
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
}

/// Dry-run lists proposed moves but does NOT move files.
#[test]
fn mission_migrate_dry_run_shows_moves_without_moving() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_sprint_file(tmp.path(), "s1", "alpha");

    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("s1"))
        .stdout(predicate::str::contains("Re-run with --apply"));

    // Files must NOT have been moved.
    assert!(
        tmp.path().join("missions/alpha.json").is_file(),
        "dry-run must not move the flat mission file"
    );
    assert!(
        tmp.path().join("sprints/s1.json").is_file(),
        "dry-run must not move the flat sprint file"
    );
}

/// `--apply` actually moves files to the per-mission nested layout.
#[test]
fn mission_migrate_apply_moves_files() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_sprint_file(tmp.path(), "s1", "alpha");

    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("applied"));

    // New nested paths must exist.
    assert!(
        tmp.path().join("missions/alpha/mission.json").is_file(),
        "mission.json should be at nested path after --apply"
    );
    assert!(
        tmp.path().join("missions/alpha/sprints/s1.json").is_file(),
        "sprint json should be at nested path after --apply"
    );
    // Old flat paths must be gone.
    assert!(
        !tmp.path().join("missions/alpha.json").exists(),
        "flat mission file should be gone after --apply"
    );
    assert!(
        !tmp.path().join("sprints/s1.json").exists(),
        "flat sprint file should be gone after --apply"
    );
}

/// Re-running `--apply` after a successful migration is a no-op (idempotent).
#[test]
fn mission_migrate_apply_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_sprint_file(tmp.path(), "s1", "alpha");

    // First apply.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success();

    // Second apply: must succeed and report nothing to do.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to do"));
}

/// Sprint-H: `notebook draft --role <id>` is the new flag (renamed
/// from `--agent` per Beat 36). The old `--agent` flag must NOT be
/// accepted — clap should reject it as an unknown argument so
/// operators with stale scripts get a loud failure instead of a
/// silent mis-dispatch.
#[test]
fn notebook_draft_rejects_old_agent_flag() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.current_dir(tmp.path());
    let output = cmd
        .args([
            "notebook",
            "draft",
            "nonexistent",
            "--agent",
            "main",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected --agent to be rejected by clap; got success: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // QA NIT 3: tighten to "unexpected argument" specifically — `agent`
    // alone could appear in clap's help suggestion text and false-pass.
    assert!(
        stderr.contains("unexpected argument"),
        "expected clap to flag `--agent` as unexpected argument; got: {stderr}"
    );
}

/// Sprint-H: `notebook draft --role <id>` accepts the new flag and
/// proceeds. Uses --dry-run + an absolute manifest path so we don't
/// need a real dispatch.
#[test]
fn notebook_draft_accepts_role_flag_under_dry_run() {
    let tmp = TempDir::new().unwrap();
    let darkmux = tmp.path().join(".darkmux");
    let runs_dir = darkmux.join("runs/test-run-h");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(
        runs_dir.join("manifest.json"),
        r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","sessionId":"s","durationMs":5000,"ok":true}"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.current_dir(tmp.path());
    cmd.env("DARKMUX_NOTEBOOK_DIR", darkmux.join("notebook").to_str().unwrap());
    cmd.args([
        "notebook",
        "draft",
        "test-run-h",
        "--role",
        "scribe",
        "--dry-run",
        "--slug",
        "sprint-h-test",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("sprint-h-test"));
}

/// Sprint-H + Sprint-E loud-bail gate: notebook draft also enforces
/// "--runtime-cmd is only valid under --runtime openclaw."
#[test]
fn notebook_draft_runtime_cmd_without_openclaw_bails_loud() {
    let tmp = TempDir::new().unwrap();
    let darkmux = tmp.path().join(".darkmux");
    let runs_dir = darkmux.join("runs/test-run-h2");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(
        runs_dir.join("manifest.json"),
        r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","sessionId":"s","durationMs":5000,"ok":true}"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.current_dir(tmp.path());
    let output = cmd
        .args([
            "notebook",
            "draft",
            "test-run-h2",
            "--runtime",
            "internal",
            "--runtime-cmd",
            "/opt/aider/aider",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected non-zero exit when --runtime-cmd set without --runtime openclaw"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--runtime openclaw"),
        "expected stderr to point operator at `--runtime openclaw`; got: {stderr}"
    );
}
