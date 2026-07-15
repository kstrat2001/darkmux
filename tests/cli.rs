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
    cmd.args(["profiles", "--profiles-file", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast"))
        .stdout(predicate::str::contains("deep"))
        .stdout(predicate::str::contains("(default)"));
}

#[test]
fn profiles_errors_when_config_missing() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["profiles", "--profiles-file", "/no/such/path.json"])
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
        "--profiles-file",
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
        "--profiles-file",
        p.to_str().unwrap(),
        "--dry-run",
        "--quiet",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("not found"));
}

#[test]
fn status_runs_with_explicit_profiles() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["status", "--profiles-file", p.to_str().unwrap()])
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
/// (post-Phase-D) requires Docker + LMStudio, which CI doesn't have. The
/// openclaw shell-out path is mockable: we point `--runtime-cmd` at
/// `/usr/bin/true` (Phase-E replacement for the removed
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
        "--profiles-file",
        cfg.to_str().unwrap(),
        "--quiet",
    ])
    .assert()
    .success();

    // The run dir should exist under .darkmux/runs/<id>/ in the tempdir,
    // and contain a v2 manifest with the right run_id.
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
    // (#487, #489) Phase 2 of the lab cluster: lab/run.rs's
    // enrich_manifest_with_fixture_info adds the `fixture` section
    // post-provider and bumps schema_version to 4. Pre-Phase-1 was v2;
    // Phase 1 (coding-task only) was v3; Phase 2 brings v4 to ALL
    // providers via the enrich step.
    assert_eq!(manifest["schema_version"].as_u64(), Some(4));
    assert_eq!(manifest["workload"].as_str(), Some("quick-q"));
    assert_eq!(manifest["provider"].as_str(), Some("prompt"));
    assert_eq!(manifest["profile"].as_str(), Some("deep"));
    assert_eq!(manifest["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(manifest["ok"].as_bool(), Some(true));
    // Phase 2 fixture section: for a self-contained workload (quick-q
    // has no source sandbox) BOTH baseline_hash and source_path are
    // null — the #496 resolution records an explicit "no source" signal
    // rather than a non-canonical raw-path fallback that would
    // spuriously mismatch a canonicalized run under `dm lab compare`.
    assert!(
        manifest["fixture"].is_object(),
        "expected fixture section, got: {}",
        manifest["fixture"]
    );
    assert!(
        manifest["fixture"]["baseline_hash"].is_null(),
        "expected null baseline_hash for self-contained workload, got: {}",
        manifest["fixture"]["baseline_hash"]
    );
    assert!(
        manifest["fixture"]["source_path"].is_null(),
        "expected null source_path for self-contained workload, got: {}",
        manifest["fixture"]["source_path"]
    );
}

/// Phase-E: `--runtime-cmd <path>` overrides the openclaw binary used
/// by the shell-out path, replacing the pre-Phase-E `DARKMUX_RUNTIME_CMD`
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

    let bogus_path = "/this/binary/definitely/does/not/exist/darkmux-phase-e";
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
            "--profiles-file",
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

/// Phase-E QA: passing `--runtime-cmd` without `--runtime openclaw` is
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
            "--profiles-file",
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
        // Assert on the machine name, NOT the 2-char run id: `notebook list`
        // prints each entry's full file path (under a random TempDir), so a
        // `contains("r2")` predicate spuriously fails whenever the tmp path
        // happens to contain "r2". Machine names don't collide with paths.
        .stdout(predicate::str::contains("m5-home"))
        .stdout(predicate::str::contains("m3-laptop").not());

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

/// (#895) `notebook list` with an absent notebook dir exits 0 — "nothing to
/// list" is success (fresh user / `notebook list && …` chaining), not an error.
#[test]
fn notebook_list_no_dir() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("notebook")
        .arg("list")
        .env("DARKMUX_NOTEBOOK_DIR", "/no/such/path/xyz")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook directory yet"));
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
        "phase_ids": [],
        "created_ts": 1,
    });
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
}

fn write_flat_phase_file(root: &std::path::Path, id: &str, mission_id: &str) {
    let dir = root.join("phases");
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
    write_flat_phase_file(tmp.path(), "s1", "alpha");

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
        tmp.path().join("phases/s1.json").is_file(),
        "dry-run must not move the flat phase file"
    );
}

/// `--apply` actually moves files to the per-mission nested layout.
#[test]
fn mission_migrate_apply_moves_files() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_phase_file(tmp.path(), "s1", "alpha");

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
        tmp.path().join("missions/alpha/phases/s1.json").is_file(),
        "phase json should be at nested path after --apply"
    );
    // Old flat paths must be gone.
    assert!(
        !tmp.path().join("missions/alpha.json").exists(),
        "flat mission file should be gone after --apply"
    );
    assert!(
        !tmp.path().join("phases/s1.json").exists(),
        "flat phase file should be gone after --apply"
    );
}

/// Re-running `--apply` after a successful migration is a no-op (idempotent).
#[test]
fn mission_migrate_apply_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_phase_file(tmp.path(), "s1", "alpha");

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

/// Phase-F: `darkmux recommendations show <tier>` prints the registry
/// entry for a known tier. This verb is referenced by the bootstrap
/// skill to read the live primary + compactor model ids; if the verb
/// regresses the skill breaks for every new operator.
#[test]
fn recommendations_show_known_tier_prints_validated_entry() {
    Command::cargo_bin("darkmux")
        .unwrap()
        .args(["recommendations", "show", "m-series-128"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Tier:     m-series-128"))
        .stdout(predicate::str::contains("Status:"))
        .stdout(predicate::str::contains("Primary:"))
        .stdout(predicate::str::contains("Compactor:"))
        .stdout(predicate::str::contains("Rationale:"));
}

/// Phase-F: `recommendations show <unknown-tier>` errors clearly.
/// The bootstrap skill's tier-resolution path depends on the verb
/// bailing loud rather than silently returning an empty placeholder.
#[test]
fn recommendations_show_unknown_tier_errors() {
    Command::cargo_bin("darkmux")
        .unwrap()
        .args(["recommendations", "show", "no-such-tier"])
        .assert()
        .failure();
}


/// Phase-H: `notebook draft --role <id>` is the new flag (renamed
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

/// Phase-H: `notebook draft --role <id>` accepts the new flag and
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
        r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","session_id":"s","duration_ms":5000,"ok":true}"#,
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
        "phase-h-test",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("phase-h-test"));
}

/// Phase-H + Phase-E loud-bail gate: notebook draft also enforces
/// "--runtime-cmd is only valid under --runtime openclaw."
#[test]
fn notebook_draft_runtime_cmd_without_openclaw_bails_loud() {
    let tmp = TempDir::new().unwrap();
    let darkmux = tmp.path().join(".darkmux");
    let runs_dir = darkmux.join("runs/test-run-h2");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(
        runs_dir.join("manifest.json"),
        r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","session_id":"s","duration_ms":5000,"ok":true}"#,
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

// ─── (#491) Phase 4 lab CLI verbs: register / unregister / fixtures / doctor ──

/// Operator runs `dm lab register <path>` against a fixture dir with
/// a valid `.fixture.json`. Registry file is created at
/// `{paths.root}/lab-registry.json` with one entry.
#[test]
fn lab_register_creates_registry_entry() {
    let tmp = TempDir::new().unwrap();
    // Create the fixture dir + manifest.
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(
        fixture_dir.join(".fixture.json"),
        r#"{"name": "demo", "satisfies": "tiny@1.0"}"#,
    )
    .unwrap();
    fs::write(fixture_dir.join("a.txt"), "alpha").unwrap();

    // Force project-scope so registry lands in tmp.
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.current_dir(tmp.path());
    cmd.args(["lab", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo`"));

    let reg_path = tmp.path().join(".darkmux/lab-registry.json");
    assert!(reg_path.exists(), "registry should exist at {}", reg_path.display());
    let raw = fs::read_to_string(&reg_path).unwrap();
    assert!(raw.contains("\"demo\""));
    assert!(raw.contains("\"tiny@1.0\""));
    assert!(raw.contains("\"content_hash\""));
}

/// `dm lab fixtures` shows the registered entry after a register.
#[test]
fn lab_fixtures_shows_registered_entries() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    // Register first.
    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    // Now list.
    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "fixtures"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("1 fixture"));
}

/// `dm lab unregister` removes the entry without touching the dir.
#[test]
fn lab_unregister_removes_entry_but_not_dir() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "unregister", "demo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Unregistered"));

    // Dir still on disk (operator-sovereignty).
    assert!(fixture_dir.join(".fixture.json").exists());
    // Registry no longer has the entry.
    let raw = fs::read_to_string(tmp.path().join(".darkmux/lab-registry.json")).unwrap();
    assert!(!raw.contains("\"demo\""));
}

/// `dm lab doctor` with no registry exits non-zero + emits a warning
/// with the three options for the operator.
#[test]
fn lab_doctor_warns_when_no_registry() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "doctor"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "doctor should exit non-zero on warnings");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no registry found"), "got: {stdout}");
    assert!(stdout.contains("lab-init.sh") || stdout.contains("dm lab register"), "got: {stdout}");
}

/// `dm lab doctor` passes when a registered fixture is unchanged.
#[test]
fn lab_doctor_passes_for_clean_fixture() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok]"))
        .stdout(predicate::str::contains("demo"));
}

/// `dm lab doctor` warns + exits non-zero when a registered fixture's
/// content has drifted (hash mismatch).
#[test]
fn lab_doctor_warns_on_hash_drift() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    // Mutate the fixture → drift.
    fs::write(fixture_dir.join("source.txt"), "MUTATED").unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "doctor"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "drift should exit non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content drift"), "got: {stdout}");
    assert!(
        stdout.contains("dm lab register --force"),
        "expected recovery hint: {stdout}"
    );
}

// ─── (#492) Phase 5: built-in fixture + lab-init.sh + demo-quickstart workload ──

/// The built-in `demo-tiny-py` fixture ships with a valid
/// `.fixture.json` that registers successfully.
#[test]
fn lab_register_builtin_demo_tiny_py_succeeds() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    // Resolve the in-tree built-in fixture path from CARGO_MANIFEST_DIR.
    let repo_root = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let fixture_path = format!(
        "{}/templates/builtin/lab-fixtures/demo-tiny-py",
        repo_root
    );

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", &fixture_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo-tiny-py`"))
        .stdout(predicate::str::contains("tiny-python-suite@1.0"));
}

/// `darkmux lab doctor` passes against the freshly-registered
/// Recursively copy `src` → `dst`, skipping run-artifact dirs (the
/// `crates/darkmux-lab` `RUN_ARTIFACT_DIRS` set). (#613) A dev machine that has
/// run a dispatch against the in-repo builtin fixture leaves `__pycache__/` /
/// `coverage/` / `.darkmux-runtime/` under it; registering that raw source
/// would trip `lab doctor`'s cleanliness check (warn → exit 1) and fail the
/// test below locally, though CI (fresh checkout) stays green. Registering a
/// pruned copy gives the test the same isolation the real lab flow gets from
/// its COW clone (#609), so the result no longer depends on dev-machine cruft.
fn copy_pruned(src: &std::path::Path, dst: &std::path::Path) {
    const PRUNE: &[&str] = &[
        ".darkmux-runtime",
        ".darkmux-agent",
        "coverage",
        ".coverage",
        "target",
        "__pycache__",
        ".git",
    ];
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if entry.file_type().unwrap().is_dir() {
            if PRUNE.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            copy_pruned(&entry.path(), &dst.join(&name));
        } else {
            fs::copy(entry.path(), dst.join(&name)).unwrap();
        }
    }
}

/// `demo-tiny-py` built-in — schema check, required_files present,
/// hash matches. Registers a pruned copy (not the raw in-repo source) so
/// dev-machine artifact cruft can't trip the cleanliness check (#613).
#[test]
fn lab_doctor_passes_for_builtin_demo_tiny_py() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();
    let repo_root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let fixture_src = format!(
        "{}/templates/builtin/lab-fixtures/demo-tiny-py",
        repo_root
    );
    // Copy to an isolated, artifact-pruned location and register THAT, so the
    // test is hermetic regardless of cruft under the in-repo fixture (#613).
    let fixture_dir = tmp.path().join("demo-tiny-py");
    copy_pruned(std::path::Path::new(&fixture_src), &fixture_dir);
    let fixture_path = fixture_dir.to_string_lossy().to_string();
    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "register", &fixture_path])
        .assert()
        .success();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo-tiny-py"))
        .stdout(predicate::str::contains("1 pass"));
}

/// (#893) `crew sync` mutates operator-owned openclaw.json — a bare run
/// (no `--yes`) must PREVIEW and bail without writing; `--yes` applies.
#[test]
fn crew_sync_refuses_to_write_without_yes() {
    let tmp = TempDir::new().unwrap();
    let oc = tmp.path().join("openclaw.json");
    fs::write(&oc, r#"{"agents":{"list":[]}}"#).unwrap();
    let crew_dir = tmp.path().join("crew"); // empty → builtin roles only
    fs::create_dir_all(&crew_dir).unwrap();

    // Bare `crew sync`: builtin roles are pending → bail + leave the file alone.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_OPENCLAW_CONFIG", &oc)
        .env("DARKMUX_CREW_DIR", &crew_dir)
        .args(["crew", "sync"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("re-run").and(predicate::str::contains("--yes")));
    assert!(
        !fs::read_to_string(&oc).unwrap().contains("darkmux/"),
        "no agent should be written without --yes"
    );

    // `--yes` applies.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_OPENCLAW_CONFIG", &oc)
        .env("DARKMUX_CREW_DIR", &crew_dir)
        .args(["crew", "sync", "--yes"])
        .assert()
        .success();
    assert!(
        fs::read_to_string(&oc).unwrap().contains("darkmux/"),
        "--yes should have written agents to openclaw.json"
    );

    // `--dry-run` previews and exits 0 without writing (even with pending).
    let oc2 = tmp.path().join("openclaw2.json");
    fs::write(&oc2, r#"{"agents":{"list":[]}}"#).unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_OPENCLAW_CONFIG", &oc2)
        .env("DARKMUX_CREW_DIR", &crew_dir)
        .args(["crew", "sync", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY RUN]"));
    assert!(
        !fs::read_to_string(&oc2).unwrap().contains("darkmux/"),
        "--dry-run must not write"
    );
}

/// (#386) `crew dispatch` accepts the message via `--message-from-file` and
/// enforces the xor with `--message`. These fail before any container work, so
/// they don't need docker / a model.
#[test]
fn crew_dispatch_message_from_file_flag_contract() {
    // Mutual exclusion: --message AND --message-from-file → clap rejects.
    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "crew",
            "dispatch",
            "code-reviewer",
            "-m",
            "inline",
            "--message-from-file",
            "/tmp/whatever",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    // Neither → clap requires one (the error names --message as the required arg).
    Command::cargo_bin("darkmux")
        .unwrap()
        .args(["crew", "dispatch", "code-reviewer"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("required arguments were not provided")
                .and(predicate::str::contains("--message")),
        );

    // Missing file → resolved early, fails loud BEFORE any dispatch setup
    // (the message is resolved at the top of the handler, ahead of out-dir
    // creation / container spawn).
    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "crew",
            "dispatch",
            "code-reviewer",
            "--message-from-file",
            "/nonexistent/darkmux-386/brief.md",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("message-from-file")
                .and(predicate::str::contains("out-dir").not())
                .and(predicate::str::contains("spawning").not()),
        );
}

// ── `mission launch review` integration tests (#1284 Packet 4b — retired
// from `pr-review run`, #1222 Phase B packet 5) ────────────────────────────

/// A small diff whose one added line ("const b = 2;") lands at new-side
/// line 2 of src/x.ts — the anchor the canned envelope's confirmed flag
/// resolves against.
fn pr_review_run_diff() -> &'static str {
    // A single-line literal with explicit `\n` escapes (NOT a backslash-
    // continued multi-line literal) — Rust's line-continuation trims
    // leading whitespace on the next physical line, which would silently
    // eat the single-space context-line marker unified diffs rely on.
    "diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n@@ -1,2 +1,3 @@\n const a = 1;\n+const b = 2;\n const c = 3;\n"
}

/// A canned `FunnelEnvelope` (see `darkmux_lab::lab::funnel::FunnelEnvelope`)
/// with one double-confirmed flag anchored to the diff above — the
/// `--from-envelope` synthesis-only path's fixture. Deliberately hand-built
/// JSON (not produced by a real dispatch) so this test needs zero model
/// calls and zero bundling, matching the CLI's own "CI-testable path"
/// framing for `--from-envelope`.
fn pr_review_run_envelope() -> &'static str {
    r#"{
        "case_id": "test-case",
        "crew": "test-crew",
        "mode": "sequential",
        "members": [
            {"model": "darkmux:probe-model", "seat": "review-probe", "draws": 2, "wall_ms": 10, "total_tokens": 100},
            {"model": "darkmux:judge-model", "seat": "review-judge", "draws": 2, "wall_ms": 5, "total_tokens": 50}
        ],
        "steps": [],
        "bundles": 1,
        "raw_flags": 2,
        "deduped_flags": 1,
        "flags": [],
        "judged": [
            {
                "flag": {
                    "bundle_id": "computeB@src/x.ts",
                    "fact_family": "unscoped",
                    "member": "darkmux:probe-model",
                    "draw": 0,
                    "charge_text": "the added constant shadows the config default",
                    "anchor": "const b = 2;"
                },
                "pass1": {"ruling": "confirmed", "decisive_evidence": "the clamp is bypassed", "note_for_author": "shadows the config default", "pass": 1, "seconds": 0.2},
                "pass2": {"ruling": "confirmed", "decisive_evidence": "confirmed on recheck", "note_for_author": "shadows the config default", "pass": 2, "seconds": 0.2},
                "tier": "confirmed",
                "demoted_by_pass2": false
            }
        ],
        "confirmed": 1,
        "needs_check": 0,
        "archived": 0,
        "fingerprint": {"judge_model": "darkmux:judge-model", "judge_temperature": 0.2, "judge_persona_blake3": "abc123", "protocol": "double-confirm-v1"}
    }"#
}

/// `--from-envelope` + `--diff` + `--emit -` synthesizes the canned
/// envelope's confirmed flag into an inline review comment on a NON-blocking
/// `COMMENT`-event review (#1302 — advisory by default; the canned envelope
/// carries no `request_changes` opt-in) — zero model calls, zero bundling
/// (the CI-testable path the packet brief names).
#[test]
fn pr_review_run_from_envelope_synthesizes_confirmed_review_to_stdout() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("funnel.json");
    fs::write(&envelope_path, pr_review_run_envelope()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {stdout}"));
    assert_eq!(v["mode"], "review");
    assert_eq!(v["review"]["event"], "COMMENT", "advisory by default (#1302)");
    let comments = v["review"]["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "src/x.ts");
    assert_eq!(comments[0]["line"], 2);
    let body = comments[0]["body"].as_str().unwrap();
    assert!(body.contains("shadows the config default"), "{body}");
    assert!(
        body.contains("needs frontier verification"),
        "confirmed comments carry the local-judge marker: {body}"
    );
}

/// `--from-envelope` also honors `--envelope-out` (a round-trip re-write of
/// the same envelope, pretty-printed) alongside the rendered `--emit`.
#[test]
fn pr_review_run_from_envelope_also_writes_envelope_out() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("funnel.json");
    fs::write(&envelope_path, pr_review_run_envelope()).unwrap();
    let out_path = tmp.path().join("out-envelope.json");
    let emit_path = tmp.path().join("rendered.json");

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("envelope_out={}", out_path.to_str().unwrap()),
            "--param",
            &format!("emit={}", emit_path.to_str().unwrap()),
        ])
        .assert()
        .success();

    let rewritten = fs::read_to_string(&out_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
    assert_eq!(v["confirmed"], 1);
    assert_eq!(v["case_id"], "test-case");

    let rendered = fs::read_to_string(&emit_path).unwrap();
    let r: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(r["mode"], "review");
}

/// (#1311, part of #1278) The dependency-free liveness FLOOR: `mission
/// launch review` emits phase markers to BOTH stderr and a
/// `<darkmux-home>/liveness/<pid>.log` heartbeat file, in order. Driven
/// offline via `from_envelope` (no model, no keychain, no network) so it
/// exercises `mission_launch_review::launch`'s early path — the markers a
/// real hang would leave behind. `DARKMUX_HOME` points the floor's home
/// resolution at the tempdir so the heartbeat file is inspectable.
#[test]
fn pr_review_run_emits_liveness_floor_markers_in_order() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("funnel.json");
    fs::write(&envelope_path, pr_review_run_envelope()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));

    // The from-envelope path fires process-start -> synthesis -> done (it skips
    // run_dispatch's config/crew/bundling markers, which need a live dispatch).
    let expected = ["process-start", "synthesis", "done"];

    // Surface 1: stderr (the most reliable surface — all that #563 could ever
    // have shown). Assert the markers appear in order.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_in_order(&stderr, &expected, "stderr");

    // Surface 2: the heartbeat FILE — proves the best-effort append landed.
    // Exactly one `<pid>.log` for this one child process.
    let liveness_dir = tmp.path().join("liveness");
    let mut logs: Vec<_> = fs::read_dir(&liveness_dir)
        .expect("liveness dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    assert_eq!(logs.len(), 1, "expected one heartbeat file, got {logs:?}");
    let file_body = fs::read_to_string(logs.pop().unwrap()).unwrap();
    assert_in_order(&file_body, &expected, "heartbeat file");
    // Each file line is `<ts> <phase> pid=<pid> case=<case>`.
    assert!(file_body.contains("pid="), "line shape: {file_body}");
    assert!(file_body.contains("case="), "line shape: {file_body}");
}

/// Assert each of `needles` appears in `haystack`, in the given order.
fn assert_in_order(haystack: &str, needles: &[&str], label: &str) {
    let mut from = 0;
    for n in needles {
        match haystack[from..].find(n) {
            Some(idx) => from += idx + n.len(),
            None => panic!("{label}: expected {n:?} after offset {from} in:\n{haystack}"),
        }
    }
}

/// `worktree` and `github` are mutually exclusive — `mission_launch_review::
/// resolve_source` enforces it manually now (`mission launch` has no clap
/// `conflicts_with`/`requires` pairing across `--param` inputs the way the
/// retired `pr-review run` flags did).
#[test]
fn pr_review_run_worktree_and_github_conflict() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            "github=kstrat2001/darkmux",
            "--param",
            "head_sha=deadbeef",
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "crew=whatever",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mutually exclusive"));
}

/// `github` without `head_sha` is also rejected — loud and named.
#[test]
fn pr_review_run_github_without_head_sha_rejected() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            "github=kstrat2001/darkmux",
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "crew=whatever",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("head_sha"));
}

/// A real (non `from_envelope`) run with no `crew` input fails loud, naming
/// the requirement, before any bundling/dispatch happens.
#[test]
fn pr_review_run_missing_crew_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let missing_profiles = tmp.path().join("no-such-profiles.json");

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", missing_profiles.to_str().unwrap()),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("input `crew` is required"));
}

// ─── review-bench --funnel flag plumbing (#1222 Phase B packet 7) ─────────
//
// The funnel condition's real dispatch path needs a live LMStudio + a real
// crew registry, so these tests stay at the clap-plumbing layer: the flag
// conflicts and `requires` relationships fail loud BEFORE any dispatch is
// attempted. A live corpus run is maintainer-executed (see the doc comment
// on `run_funnel_case` in `crates/darkmux-lab/src/lab/review_bench.rs`).

#[test]
fn review_bench_funnel_conflicts_with_dialectic() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "review-bench", "--funnel", "--dialectic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn review_bench_funnel_conflicts_with_agentic_and_freeform() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "review-bench", "--funnel", "--agentic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    let mut cmd2 = Command::cargo_bin("darkmux").unwrap();
    cmd2.args(["lab", "review-bench", "--funnel", "--freeform"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn review_bench_crew_requires_funnel() {
    // --crew named without --funnel: clap's `requires = "funnel"` fires
    // before the command handler ever runs (no dispatch, no cases loaded).
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "review-bench", "--crew", "review-funnel"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("required arguments were not provided")
                .and(predicate::str::contains("--funnel")),
        );
}

#[test]
fn review_bench_exec_mode_k_and_bundler_each_require_funnel() {
    for (flag, value) in [
        ("--exec-mode", "sequential"),
        ("--k", "3"),
        ("--bundler", "some-bundler"),
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(["lab", "review-bench", flag, value])
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("required arguments were not provided")
                    .and(predicate::str::contains("--funnel")),
            );
    }
}

#[test]
fn review_bench_funnel_requires_workdirs() {
    // --funnel alone (no --workdirs): reuses the same preflight
    // --agentic/--dialectic already run, extended to include --funnel.
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "review-bench", "--funnel", "--crew", "review-funnel"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--funnel requires --workdirs"));
}

#[test]
fn review_bench_funnel_requires_crew() {
    // --funnel + --workdirs (with a satisfied repo-tree preflight) but no
    // --crew: the funnel-context preflight (resolve_funnel_ctx) fails loud
    // before any dispatch spends a token. Uses a minimal one-case fixture so
    // the --workdirs tree-existence check (which runs first) passes and the
    // funnel-context check is the one under test.
    let tmp = TempDir::new().unwrap();
    let cases_dir = tmp.path().join("cases");
    fs::create_dir_all(&cases_dir).unwrap();
    fs::write(
        cases_dir.join("c1.label.json"),
        r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
    )
    .unwrap();
    fs::write(cases_dir.join("c1.diff"), "diff --git a b\n").unwrap();
    let workdirs = tmp.path().join("workdirs");
    fs::create_dir_all(workdirs.join("c1")).unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "review-bench",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("--funnel requires --crew"));
}

#[test]
fn review_bench_funnel_k_zero_rejected_at_cli_layer() {
    // --k 0 would otherwise slip past resolve_crew's k>=1 guard via the
    // post-resolution override (resolve_funnel_ctx overwrites every
    // review-probe staffing's k AFTER resolve_crew validated the crew's OWN
    // k), guaranteeing a degenerate run (zero probe draws). The clap
    // `value_parser` range rejects it before the command handler ever runs.
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "review-bench",
        "--funnel",
        "--crew",
        "review-funnel",
        "--k",
        "0",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("not in 1.."));
}

#[test]
fn review_bench_funnel_preflight_validates_seat_requirements_before_dispatch() {
    // A crew that resolves fine at the schema layer (resolve_crew doesn't
    // know about funnel-specific seat names) but is missing "review-judge"
    // must fail at PREFLIGHT (resolve_funnel_ctx calling
    // funnel::validate_funnel_crew), not at the first case's dispatch — the
    // per-case table header must never print.
    let tmp = TempDir::new().unwrap();
    let cases_dir = tmp.path().join("cases");
    fs::create_dir_all(&cases_dir).unwrap();
    fs::write(
        cases_dir.join("c1.label.json"),
        r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
    )
    .unwrap();
    fs::write(cases_dir.join("c1.diff"), "diff --git a b\n").unwrap();
    let workdirs = tmp.path().join("workdirs");
    fs::create_dir_all(workdirs.join("c1")).unwrap();

    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{
            "profiles": {
                "fast": {
                    "description": "test",
                    "models": [{"id": "model-a", "n_ctx": 32000, "role": "primary"}]
                }
            },
            "default_profile": "fast",
            "crews": {
                "no-judge": {
                    "seats": {
                        "review-probe": [{"profile": "fast", "k": 2}]
                    }
                }
            }
        }"#,
    )
    .unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "lab",
            "review-bench",
            "--cases-dir",
            cases_dir.to_str().unwrap(),
            "--funnel",
            "--workdirs",
            workdirs.to_str().unwrap(),
            "--crew",
            "no-judge",
            "--profiles-file",
            profiles_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("review-judge"))
        .stdout(predicate::str::contains("outcome").not());
}

#[test]
fn review_bench_funnel_preflight_fails_loud_on_crew_own_resolve_crew_error() {
    // (#1269) The registry LOADS fine (a bad crew no longer fails load), but
    // this specific crew fails at `resolve_crew` itself (a LOCAL staffing
    // whose model omits `n_ctx`, #1282 — not a funnel-specific seat-shape
    // gap like the sibling test above; the original remote-endpoint fixture
    // became a LEGAL crew when #1260 lifted the local-only fence). The
    // funnel preflight must still fail loud, BEFORE the per-case table
    // header prints, naming the specific resolve_crew error.
    let tmp = TempDir::new().unwrap();
    let cases_dir = tmp.path().join("cases");
    fs::create_dir_all(&cases_dir).unwrap();
    fs::write(
        cases_dir.join("c1.label.json"),
        r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
    )
    .unwrap();
    fs::write(cases_dir.join("c1.diff"), "diff --git a b\n").unwrap();
    let workdirs = tmp.path().join("workdirs");
    fs::create_dir_all(workdirs.join("c1")).unwrap();

    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{
            "profiles": {
                "fast": {
                    "models": [{"id": "model-a", "n_ctx": 32000}]
                },
                "ctxless": {
                    "models": [{"id": "local-b"}]
                }
            },
            "default_profile": "fast",
            "crews": {
                "broken-crew": {
                    "seats": {
                        "review-probe": [{"profile": "ctxless", "k": 2}],
                        "review-judge": [{"profile": "fast"}]
                    }
                }
            }
        }"#,
    )
    .unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "lab",
            "review-bench",
            "--cases-dir",
            cases_dir.to_str().unwrap(),
            "--funnel",
            "--workdirs",
            workdirs.to_str().unwrap(),
            "--crew",
            "broken-crew",
            "--profiles-file",
            profiles_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("n_ctx").and(predicate::str::contains("review-probe")))
        .stdout(predicate::str::contains("outcome").not());
}

// ─── review-bench --funnel end-to-end, offline (#1222 Phase B coverage) ───
//
// A funnel run whose bundler produces ZERO bundles short-circuits to a
// degenerate envelope BEFORE any probe/judge dispatch — so a full
// `review-bench --funnel` invocation over a non-TypeScript diff corpus is
// end-to-end testable with no LMStudio and no crew models loaded. These
// tests exercise the real preflight (registry load + crew resolution +
// role-prompt resolution), the per-case funnel branch, the console line,
// and the scores.json/funnels.json artifact pair.

/// A profiles registry carrying a crews block that satisfies
/// `validate_funnel_crew` (>= 1 review-probe staffing, exactly 1
/// review-judge staffing). `resolve_funnel_ctx`'s own preflight call to
/// `resolve_crew` resolves every staffing against `profiles` — no LMStudio
/// involved.
fn funnel_registry_json() -> &'static str {
    r#"{
        "profiles": {
            "fast": {
                "description": "bounded tasks",
                "models": [
                    {"id": "model-a", "n_ctx": 32000}
                ]
            }
        },
        "crews": {
            "review-funnel": {
                "seats": {
                    "review-probe": [{"profile": "fast", "k": 2}],
                    "review-judge": [{"profile": "fast", "k": 1}]
                }
            }
        },
        "default_profile": "fast"
    }"#
}

/// One-case corpus whose diff touches only a non-TS file — the built-in
/// bundler finds zero bundles, so the funnel resolves degenerately with
/// zero dispatches.
fn write_funnel_fixture(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let cases_dir = tmp.path().join("cases");
    fs::create_dir_all(&cases_dir).unwrap();
    fs::write(
        cases_dir.join("c1.label.json"),
        r#"{"kind":"clean","intent_title":"docs touch-up","expect_verdict":"pass"}"#,
    )
    .unwrap();
    fs::write(
        cases_dir.join("c1.diff"),
        "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Title\n+New line\n",
    )
    .unwrap();
    let workdirs = tmp.path().join("workdirs");
    fs::create_dir_all(workdirs.join("c1")).unwrap();
    let registry = tmp.path().join("profiles.json");
    fs::write(&registry, funnel_registry_json()).unwrap();
    (cases_dir, workdirs, registry)
}

#[test]
fn review_bench_funnel_nonexistent_crew_fails_preflight_listing_available() {
    // --crew names a crew the registry doesn't have: resolve_funnel_ctx's
    // preflight fails loud BEFORE any dispatch, and the error names both the
    // missing crew and the crews that DO exist (resolve_crew's "Available:"
    // listing) — the operator never has to open profiles.json to find the
    // right name.
    let tmp = TempDir::new().unwrap();
    let (cases_dir, workdirs, registry) = write_funnel_fixture(&tmp);

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "review-bench",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--crew",
        "ghost",
        "--profiles-file",
        registry.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(
        predicate::str::contains(r#"resolving crew "ghost" for --funnel"#)
            .and(predicate::str::contains("not found"))
            .and(predicate::str::contains("review-funnel")),
    );
}

#[test]
fn review_bench_funnel_degenerate_run_completes_offline_with_console_line_and_artifact_pair() {
    // The full --funnel path, end-to-end, zero dispatches: preflight
    // (registry + crew + embedded review-probe.md/review-judge.md role
    // prompts) → per-case run_funnel_case → built-in bundler finds no TS
    // bundles → degenerate envelope → scored degenerate (never a clean
    // pass) → per-case funnel console line → scores.json + funnels.json
    // both written.
    let tmp = TempDir::new().unwrap();
    let (cases_dir, workdirs, registry) = write_funnel_fixture(&tmp);
    let scores_out = tmp.path().join("out").join("scores.json");

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "review-bench",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--crew",
        "review-funnel",
        "--exec-mode",
        "sequential",
        "--profiles-file",
        registry.to_str().unwrap(),
        "--scores-out",
        scores_out.to_str().unwrap(),
    ])
    .assert()
    .success()
    // The per-case funnel console line (#1222 packet 7's funnel branch in
    // run_review_bench) — a degenerate case still reports its shape.
    .stdout(
        predicate::str::contains("bundles 0")
            .and(predicate::str::contains("flags 0"))
            .and(predicate::str::contains("DEGENERATE")),
    )
    .stderr(predicate::str::contains("mode=funnel").and(predicate::str::contains("funnels:")));

    // scores.json: funnel provenance extras (crew / exec_mode / k).
    let scores: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&scores_out).unwrap()).unwrap();
    assert_eq!(scores["mode"], serde_json::json!("funnel"));
    assert_eq!(scores["crew"], serde_json::json!("review-funnel"));
    assert_eq!(scores["exec_mode"], serde_json::json!("sequential"));
    assert_eq!(scores["k"], serde_json::json!("(profile default)"));

    // funnels.json: one envelope, degenerate reason set, zero dispatches.
    let funnels: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(scores_out.with_file_name("funnels.json")).unwrap())
            .unwrap();
    let env = &funnels[0];
    assert_eq!(env["case_id"], serde_json::json!("c1"));
    assert_eq!(env["crew"], serde_json::json!("review-funnel"));
    assert_eq!(env["mode"], serde_json::json!("sequential"));
    assert_eq!(env["bundles"], serde_json::json!(0));
    assert!(
        env["degenerate"].as_str().unwrap().contains("no bundles"),
        "the zero-bundle reason must be recorded on the envelope: {}",
        env["degenerate"]
    );
    assert_eq!(env["members"], serde_json::json!([]), "zero dispatches — no member rows");
}

#[cfg(unix)]
#[test]
fn review_bench_funnel_bundler_flag_reaches_external_bundles_and_fails_loud_per_case() {
    // --bundler plumbing, CLI → run_funnel_case → bundle::external_bundles:
    // a stub bundler emitting an empty bundle set trips external_bundles'
    // own loud contract check, wrapped with the case id. The failure happens
    // BEFORE any probe/judge dispatch, so this too runs fully offline. The
    // diff names a .ts file so the failure is attributable to the external
    // bundler, not to the built-in bundler's TS filter.
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let (cases_dir, workdirs, registry) = write_funnel_fixture(&tmp);
    fs::write(
        cases_dir.join("c1.diff"),
        "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1 +1,2 @@\n foo\n+bar\n",
    )
    .unwrap();
    let bundler = tmp.path().join("empty-bundler.sh");
    fs::write(&bundler, "#!/bin/sh\necho '{\"bundles\":[]}'\n").unwrap();
    let mut perms = fs::metadata(&bundler).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bundler, perms).unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "review-bench",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--crew",
        "review-funnel",
        "--exec-mode",
        "sequential",
        "--profiles-file",
        registry.to_str().unwrap(),
        "--bundler",
        bundler.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(
        predicate::str::contains("funneling case c1")
            .and(predicate::str::contains("external bundler"))
            .and(predicate::str::contains("empty bundle set")),
    );
}


/// `--envelope-out` pointed at a path whose parent directory doesn't exist
/// must fail loudly (`std::fs::write` errors, wrapped by `.with_context`)
/// — not silently swallow the write. `fn main() -> Result<()>` propagating
/// an `Err` up through `anyhow` prints the error chain to stderr and exits
/// **1** (characterized here, not previously asserted anywhere).
#[test]
fn pr_review_run_envelope_out_unwritable_dir_fails_loudly() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("funnel.json");
    fs::write(&envelope_path, pr_review_run_envelope()).unwrap();
    let bad_out = tmp.path().join("no-such-dir").join("out.json");

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("envelope_out={}", bad_out.to_str().unwrap()),
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unwritable envelope_out dir must exit 1, stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("writing envelope_out"), "{stderr}");
}

/// A degenerate envelope routed through `--from-envelope` still exits
/// **0** — `synthesize_funnel`'s `mode: "degraded"` is carried in the JSON
/// payload, not surfaced as a process failure (that distinction is the
/// posting workflow's job to read, not the CLI's job to signal via exit
/// code). Characterizes the previously-unasserted exit-code half of the
/// degraded contract.
#[test]
fn pr_review_run_from_envelope_degenerate_exits_zero_with_degraded_mode() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("degenerate.json");
    fs::write(
        &envelope_path,
        r#"{
            "case_id": "test-case", "crew": "test-crew", "mode": "sequential",
            "members": [], "steps": [], "bundles": 0, "raw_flags": 0, "deduped_flags": 0,
            "flags": [], "judged": [], "confirmed": 0, "needs_check": 0, "archived": 0,
            "degenerate": "zero flags from all probe draws",
            "fingerprint": {}
        }"#,
    )
    .unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "a degraded outcome is still a successful *run* — stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["mode"], "degraded");
}

/// A malformed `--charges-file` (re-judge without re-probing) must fail
/// loudly, named, BEFORE any model dispatch — the parse happens right
/// after bundling and before the judge's `chat` closure is ever called, so
/// this is exercisable with a stub `--bundler` and no live LMStudio.
#[cfg(unix)]
#[test]
fn pr_review_run_malformed_charges_file_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();

    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{
            "profiles": { "fast": { "models": [{"id": "test-model", "n_ctx": 8000}] } },
            "crews": {
                "test-crew": {
                    "seats": {
                        "review-probe": [{"profile": "fast", "k": 1}],
                        "review-judge": [{"profile": "fast", "k": 1}]
                    }
                }
            }
        }"#,
    )
    .unwrap();

    // A stub external bundler emitting exactly one valid `Bundle` — cheap
    // to satisfy `parse_bundle_set`'s non-empty-set requirement without
    // needing a real checkout matching the diff (`slice_code` tolerates an
    // unreadable/missing path; it just marks the excerpt unreadable).
    let bundler_path = tmp.path().join("fake-bundler.sh");
    fs::write(
        &bundler_path,
        "#!/bin/sh\necho '{\"bundles\":[{\"id\":\"computeEnd@src/x.ts\",\"code\":[{\"path\":\"src/x.ts\",\"start\":1,\"end\":2}],\"facts\":[],\"fact_family\":\"unscoped\"}]}'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bundler_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bundler_path, perms).unwrap();
    }

    let charges_path = tmp.path().join("charges.json");
    fs::write(&charges_path, "not valid json{{{").unwrap();

    let worktree_dir = tmp.path().join("wt");
    fs::create_dir(&worktree_dir).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", worktree_dir.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "crew=test-crew",
            "--param",
            &format!("profiles={}", profiles_path.to_str().unwrap()),
            "--param",
            &format!("bundler={}", bundler_path.to_str().unwrap()),
            "--param",
            &format!("charges_file={}", charges_path.to_str().unwrap()),
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "malformed charges_file must exit loud, stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("charges_file"), "{stderr}");
    assert!(stderr.contains("flag list"), "{stderr}");
}

/// `mission_launch_review::launch`'s `from_envelope` ignored-input warning
/// (src/mission_launch_review.rs) surfaces `bundler` as a dispatch-shaping
/// input with nothing to shape when synthesis-only (`k` follows the same
/// `ignored` Vec and warning path) — operator sovereignty: surface, never
/// silently ignore.
#[test]
fn pr_review_run_bundler_should_warn_ignored_with_from_envelope() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let envelope_path = tmp.path().join("funnel.json");
    fs::write(&envelope_path, pr_review_run_envelope()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("from_envelope={}", envelope_path.to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            "bundler=/nonexistent-bundler-binary",
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bundler"),
        "expected an ignored-flag warning naming `bundler`: {stderr}"
    );
}
